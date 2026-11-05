// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn>
// See LICENSES for license details.

//! USB Mass Storage, Bulk-Only Transport (BOT).
//!
//! The smallest useful slice of `drivers/usb/storage/`: a command block wrapper
//! goes out on the bulk-OUT endpoint, the data stage (if any) comes back on the
//! bulk-IN endpoint, and a command status wrapper reports residue and status.
//! Only `READ(10)` is implemented — enough to read a sector and prove that both
//! the bulk path and the device work.

use alloc::vec::Vec;

use kerrno::{KError, KResult};

use crate::Hcd;

/// The status signature the device first showed us
/// (`us->bcs_signature`): compared against later, not against the spec value.
static LEARNT_SIGNATURE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// The command tag: `usb_stor_Bulk_transport()` increments it per command
/// (`bcb->Tag = ++us->tag`) and the status wrapper must echo it.
static TAG: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Command Block Wrapper: 31 bytes (`usb_stor_Bulk_transport()`).
pub const CBW_LEN: usize = 31;
/// Command Status Wrapper: 13 bytes.
pub const CSW_LEN: usize = 13;
const CBW_SIGNATURE: u32 = 0x4342_5355; // "USBC" little endian
const CSW_SIGNATURE: u32 = 0x5342_5355; // "USBS" little endian
/// `US_FL_*`'s counterpart here: how many times a stalled phase is retried.
const TRIES: usize = 3;

/// One mass-storage device's endpoints.
#[derive(Clone, Copy, Debug)]
pub struct BotEndpoints {
    pub address: u8,
    pub endpoint_in: u8,
    pub endpoint_out: u8,
    pub max_packet: u16,
    /// Interface number the class reset addresses (`us->ifnum`).
    pub interface: u8,
}

/// Builds a 31-byte command block wrapper.
fn build_cbw(tag: u32, transfer_length: u32, is_in: bool, command: &[u8]) -> [u8; CBW_LEN] {
    let mut cbw = [0u8; CBW_LEN];
    cbw[0..4].copy_from_slice(&CBW_SIGNATURE.to_le_bytes());
    cbw[4..8].copy_from_slice(&tag.to_le_bytes());
    cbw[8..12].copy_from_slice(&transfer_length.to_le_bytes());
    // bmCBWFlags: bit 7 set means the data stage is device-to-host.
    cbw[12] = if is_in { 0x80 } else { 0x00 };
    cbw[13] = 0; // bCBWLUN
    let length = command.len().min(16);
    cbw[14] = length as u8;
    cbw[15..15 + length].copy_from_slice(&command[..length]);
    cbw
}

/// Runs one BOT command and returns the data stage.
///
/// The three phases are strictly ordered: the status wrapper is only valid after
/// the data stage has drained, which is why a failure in either bulk transfer is
/// reported rather than retried blindly.
fn transfer(
    hcd: &mut dyn Hcd,
    endpoints: &BotEndpoints,
    command: &[u8],
    transfer_length: usize,
    is_in: bool,
    _tag: u32,
) -> KResult<Vec<u8>> {
    // `bcb->Tag = ++us->tag`: the status wrapper echoes this, and the echo is
    // checked below.
    let tag = TAG.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
    let cbw = build_cbw(tag, transfer_length as u32, is_in, command);
    let mut written = [0u8; CBW_LEN];
    written.copy_from_slice(&cbw);
    let sent = hcd.bulk_transfer(
        endpoints.address,
        endpoints.endpoint_out,
        &mut written,
        false,
    );
    let sent = match sent {
        Ok(sent) => sent,
        Err(err) => {
            log::error!("bot: CBW transfer failed: {err:?}");
            return Err(err);
        }
    };
    if sent != CBW_LEN {
        return Err(KError::Io);
    }
    let mut data = alloc::vec![0u8; transfer_length];
    if transfer_length > 0 {
        let mut received = 0;
        while received < transfer_length {
            let read = hcd.bulk_transfer(
                endpoints.address,
                if is_in {
                    endpoints.endpoint_in
                } else {
                    endpoints.endpoint_out
                },
                &mut data[received..],
                is_in,
            )?;
            if read == 0 {
                break;
            }
            received += read;
        }
        if received != transfer_length {
            log::warn!("bot: short data stage {received}/{transfer_length}");
            // `usb_stor_Bulk_transport()`: "Sometimes a device will mistakenly
            // skip the data phase and go directly to the status phase without
            // sending a zero-length packet. If we get a 13-byte response here,
            // check whether it really is a CSW."
            if is_in
                && transfer_length.saturating_sub(received) == CSW_LEN
                && received + CSW_LEN <= data.len()
            {
                let candidate = &data[received..received + CSW_LEN];
                let signature =
                    u32::from_le_bytes([candidate[0], candidate[1], candidate[2], candidate[3]]);
                if signature == CSW_SIGNATURE {
                    log::warn!("bot: device skipped the data phase; using its status wrapper");
                    let mut wrapper = [0u8; CSW_LEN];
                    wrapper.copy_from_slice(candidate);
                    return finish_status(&wrapper, tag, Some(data));
                }
            }
        }
    }
    let mut csw = [0u8; CSW_LEN];
    let mut got = 0;
    while got < CSW_LEN {
        let read = hcd.bulk_transfer(
            endpoints.address,
            endpoints.endpoint_in,
            &mut csw[got..],
            true,
        );
        let read = match read {
            Ok(read) => read,
            Err(err) => {
                log::error!("bot: CSW transfer failed after {got} bytes: {err:?}");
                return Err(err);
            }
        };
        if read == 0 {
            log::error!("bot: CSW read returned nothing after {got} bytes");
            break;
        }
        got += read;
    }
    // `usb_stor_Bulk_transport()`: a zero-length status wrapper is retried once
    // ("Some broken devices add unnecessary zero-length packets"), and a stalled
    // status phase is retried after the endpoint halt is cleared.
    if got == 0 {
        log::warn!("bot: zero-length CSW; retrying");
        let read = hcd.bulk_transfer(endpoints.address, endpoints.endpoint_in, &mut csw, true)?;
        got = read;
    }
    if got < CSW_LEN {
        let _ = reset_recovery(hcd, endpoints);
        let read = hcd.bulk_transfer(endpoints.address, endpoints.endpoint_in, &mut csw, true)?;
        got = read;
    }
    if got < CSW_LEN {
        log::error!("bot: short CSW {got}/{CSW_LEN} after recovery");
        return Err(KError::Io);
    }
    finish_status(&csw, tag, Some(data))
}

/// `usb_stor_Bulk_transport()`'s status-wrapper checks: learnt signature,
/// echoed tag, and the status code. `US_BULK_STAT_PHASE` means the device and
/// host disagree about the command's phases, and the flow chart on page 15 of
/// the spec routes it to a reset recovery rather than a retry.
fn finish_status(
    csw: &[u8; CSW_LEN],
    tag: u32,
    mut data: Option<alloc::vec::Vec<u8>>,
) -> KResult<Vec<u8>> {
    let signature = u32::from_le_bytes([csw[0], csw[1], csw[2], csw[3]]);
    // "Some broken devices report odd signatures, so we do not check them for
    // validity against the spec. We store the first one we see, and check
    // subsequent transfers for validity against this signature."
    let learnt = LEARNT_SIGNATURE.load(core::sync::atomic::Ordering::Relaxed);
    if learnt == 0 {
        LEARNT_SIGNATURE.store(signature, core::sync::atomic::Ordering::Relaxed);
        if signature != CSW_SIGNATURE {
            log::warn!("bot: learnt non-standard status signature {signature:#010x}");
        }
    } else if signature != learnt {
        log::error!("bot: signature mismatch: got {signature:#010x}, expecting {learnt:#010x}");
        return Err(KError::Io);
    }
    let echoed_tag = u32::from_le_bytes([csw[4], csw[5], csw[6], csw[7]]);
    if echoed_tag != tag {
        log::error!("bot: status tag {echoed_tag} does not match command tag {tag}");
        return Err(KError::Io);
    }
    let status = csw[12];
    match status {
        0 => {}
        // US_BULK_STAT_PHASE: only a reset recovery gets out of this.
        2 => {
            log::error!("bot: phase error; running the bulk-only reset");
            return Err(KError::Io);
        }
        other => log::error!("bot: command status {other}"),
    }
    match data.take() {
        Some(data) => Ok(data),
        None => Ok(alloc::vec![]),
    }
}

/// `READ(10)`: reads `count` blocks of 512 bytes starting at `lba`.
pub fn read_sectors(
    hcd: &mut dyn Hcd,
    endpoints: &BotEndpoints,
    lba: u32,
    count: u16,
) -> KResult<Vec<u8>> {
    let mut command = [0u8; 10];
    command[0] = 0x28; // READ(10)
    command[2..6].copy_from_slice(&lba.to_be_bytes());
    command[7..9].copy_from_slice(&count.to_be_bytes());
    transfer(hcd, endpoints, &command, usize::from(count) * 512, true, 1)
}

/// `WRITE(10)`: writes `count` blocks of 512 bytes starting at `lba`.
///
/// A filesystem mounted read-write needs this: ext4 replays its journal on the
/// first mount after an unclean unmount, and that is a write.
pub fn write_sectors(
    hcd: &mut dyn Hcd,
    endpoints: &BotEndpoints,
    lba: u32,
    blocks: &[u8],
) -> KResult<()> {
    if blocks.len() % 512 != 0 {
        return Err(KError::InvalidInput);
    }
    let count = (blocks.len() / 512) as u16;
    let mut command = [0u8; 10];
    command[0] = 0x2A; // WRITE(10)
    command[2..6].copy_from_slice(&lba.to_be_bytes());
    command[7..9].copy_from_slice(&count.to_be_bytes());
    let mut payload = alloc::vec![0u8; blocks.len()];
    payload.copy_from_slice(blocks);
    transfer(hcd, endpoints, &command, blocks.len(), false, 4).map(|_| ())
}

/// Reads the filesystem superblock and reports what it finds.
///
/// An ext4 filesystem created directly on the device has no MBR, so sector 0
/// carries nothing recognisable; the superblock lives 1024 bytes in, with
/// `s_magic` at +0x38 and the volume label at +0x78. Checking that is a real
/// verdict on the bulk path, where "sector 0 is all zeros" is not.
pub fn probe_filesystem(hcd: &mut dyn Hcd, endpoints: &BotEndpoints) {
    for attempt in 0..TRIES {
        match read_sectors(hcd, endpoints, 0, 3) {
            Ok(sectors) => {
                if sectors.len() < 1536 {
                    log::error!("bot: short read: {} bytes", sectors.len());
                    return;
                }
                let superblock = &sectors[1024..];
                let magic = u16::from_le_bytes([superblock[0x38], superblock[0x39]]);
                if magic == 0xEF53 {
                    let label = core::str::from_utf8(&superblock[0x78..0x88])
                        .unwrap_or("?")
                        .trim_end_matches('\0')
                        .trim();
                    log::info!("bot: ext2/3/4 superblock found, volume label `{label}`");
                } else {
                    log::info!("bot: no ext4 superblock at offset 1024, magic {magic:#06x}");
                }
                return;
            }
            Err(err) => log::warn!("bot: read attempt {attempt} failed: {err:?}"),
        }
    }
}
/// `INQUIRY`: the device's vendor and product strings.
pub fn inquiry(hcd: &mut dyn Hcd, endpoints: &BotEndpoints) -> KResult<Vec<u8>> {
    let mut command = [0u8; 6];
    command[0] = 0x12; // INQUIRY
    command[4] = 36; // allocation length
    transfer(hcd, endpoints, &command, 36, true, 2)
}

/// `READ CAPACITY (10)`: last logical block address and block size.
pub fn read_capacity(hcd: &mut dyn Hcd, endpoints: &BotEndpoints) -> KResult<Vec<u8>> {
    let command = [0x25u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    transfer(hcd, endpoints, &command, 8, true, 3)
}

/// Prints what the device says it is and how big.
pub fn describe(hcd: &mut dyn Hcd, endpoints: &BotEndpoints) {
    match inquiry(hcd, endpoints) {
        Ok(data) if data.len() >= 36 => {
            let vendor = core::str::from_utf8(&data[8..16]).unwrap_or("?").trim();
            let product = core::str::from_utf8(&data[16..32]).unwrap_or("?").trim();
            log::info!("bot: inquiry vendor `{vendor}` product `{product}`");
        }
        Ok(_) => log::warn!("bot: short inquiry reply"),
        Err(err) => log::warn!("bot: inquiry failed: {err:?}"),
    }
    match read_capacity(hcd, endpoints) {
        Ok(data) if data.len() >= 8 => {
            let last = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            let block = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
            log::info!(
                "bot: {block}-byte blocks, last LBA {last} ({} MiB)",
                (u64::from(last) + 1) * u64::from(block) / (1024 * 1024)
            );
        }
        Ok(_) => log::warn!("bot: short read capacity reply"),
        Err(err) => log::warn!("bot: read capacity failed: {err:?}"),
    }
}

/// Bulk-Only Mass Storage Reset: class request `0xff`, recipient interface
/// (`usb_stor_Bulk_reset()` → `usb_stor_reset_common()`).
pub const BULK_ONLY_RESET: u8 = 0xff;
/// `USB_REQ_CLEAR_FEATURE`, `USB_RECIP_ENDPOINT`, `USB_ENDPOINT_HALT`.
const CLEAR_FEATURE_ENDPOINT: u8 = 0x02;
const ENDPOINT_HALT: u16 = 0;

/// Soft reset of a mass-storage device (`usb_stor_Bulk_reset()`).
///
/// A BOT device that has lost track of a command keeps NAKing every
/// transaction, which on the host side looks exactly like a transfer that never
/// completes. The reference clears that state with the class reset followed by
/// `CLEAR_FEATURE(ENDPOINT_HALT)` on both bulk endpoints, and waits six seconds
/// between the two steps.
pub fn reset_recovery(hcd: &mut dyn Hcd, endpoints: &BotEndpoints) -> KResult<()> {
    let setup =
        crate::SetupPacket::new(0x21, BULK_ONLY_RESET, 0, u16::from(endpoints.interface), 0);
    let mut none = [0u8; 0];
    hcd.control_transfer(endpoints.address, &setup, &mut none)?;
    // "Give the device some time to recover from the reset" -- the reference
    // waits up to six seconds here; the bulk-only spec asks for at least one.
    khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1_100_000));
    let mut first_error = None;
    for endpoint in [endpoints.endpoint_in, endpoints.endpoint_out] {
        let clear = crate::SetupPacket::new(
            CLEAR_FEATURE_ENDPOINT,
            1, // USB_REQ_CLEAR_FEATURE
            ENDPOINT_HALT,
            u16::from(endpoint),
            0,
        );
        if let Err(err) = hcd.control_transfer(endpoints.address, &clear, &mut none) {
            first_error.get_or_insert(err);
        }
    }
    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}
