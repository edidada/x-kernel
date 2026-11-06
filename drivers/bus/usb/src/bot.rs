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
use log::info;

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

/// Moves the data stage, returning how many bytes moved.
///
/// `usb_stor_Bulk_transfer_buf()` loops until the whole length is done or the
/// device stops answering; a short stage is the device's business, not an error
/// (`usb_stor_Bulk_transport()` decides what it means).
fn data_stage(
    hcd: &mut dyn Hcd,
    endpoints: &BotEndpoints,
    data: &mut [u8],
    is_in: bool,
) -> KResult<usize> {
    let length = data.len();
    let mut received = 0;
    while received < length {
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
    Ok(received)
}

/// The status wrapper a device may send in place of a skipped data stage.
///
/// `usb_stor_Bulk_transport()`: "Sometimes a device will mistakenly skip the data
/// phase and go directly to the status phase without sending a zero-length packet.
/// If we get a 13-byte response here, check whether it really is a CSW."
fn status_wrapper_in_data(
    data: &[u8],
    received: usize,
    expected: usize,
    is_in: bool,
) -> Option<[u8; CSW_LEN]> {
    if !is_in || expected.saturating_sub(received) != CSW_LEN || received + CSW_LEN > data.len() {
        return None;
    }
    let candidate = &data[received..received + CSW_LEN];
    let signature = u32::from_le_bytes([candidate[0], candidate[1], candidate[2], candidate[3]]);
    if signature != CSW_SIGNATURE {
        return None;
    }
    let mut wrapper = [0u8; CSW_LEN];
    wrapper.copy_from_slice(candidate);
    Some(wrapper)
}

/// What the data phase of one command carries.
#[derive(Clone, Copy)]
enum DataPhase<'a> {
    /// No data stage: `TEST UNIT READY`, `START STOP UNIT`.
    None,
    /// Data-in of this many bytes (`READ(10)`, `INQUIRY`, `REQUEST SENSE`).
    In(usize),
    /// Data-out: the bytes `WRITE(10)` puts on the wire.
    Out(&'a [u8]),
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
    phase: DataPhase<'_>,
    _tag: u32,
) -> KResult<Vec<u8>> {
    let (transfer_length, is_in) = match phase {
        DataPhase::None => (0, false),
        DataPhase::In(length) => (length, true),
        DataPhase::Out(data) => (data.len(), false),
    };
    // `bcb->Tag = ++us->tag`: the status wrapper echoes this, and the echo is
    // checked below.
    let tag = TAG.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
    let cbw = build_cbw(tag, transfer_length as u32, is_in, command);
    let mut written = [0u8; CBW_LEN];
    written.copy_from_slice(&cbw);
    // One line per command: opcode, transfer length and direction, plus the CDB
    // as it goes on the wire. A device that stops accepting a CBW has to be
    // matched against the command stream that led to it, and the descriptors are
    // the only place that stream is visible.
    // The address is part of every line: the tag counter is global across
    // controllers, so without it a "no CSW for tag N" cannot be attributed to a
    // device, which cost this bring-up more than one round.
    info!(
        "bot: addr {} cbw tag {tag} op {:#04x} len {transfer_length} dir {} cdb {:02x?}",
        endpoints.address,
        command.first().copied().unwrap_or(0),
        if is_in { "in" } else { "out" },
        &command[..command.len().min(16)],
    );
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
    // The OUT payload is the caller's, not a fresh zero buffer: `WRITE(10)` used to
    // ship 512 zero bytes for every block because this allocation was never filled
    // from the command's data -- the bug that zeroed the first blocks of the stick
    // this driver was being brought up on.
    let mut data = match phase {
        DataPhase::Out(source) => {
            let mut buffer = alloc::vec![0u8; source.len()];
            buffer.copy_from_slice(source);
            buffer
        }
        _ => alloc::vec![0u8; transfer_length],
    };
    if transfer_length > 0 {
        let received = data_stage(hcd, endpoints, &mut data, is_in)?;
        if received != transfer_length {
            log::warn!("bot: short data stage {received}/{transfer_length}");
            if let Some(wrapper) = status_wrapper_in_data(&data, received, transfer_length, is_in) {
                log::warn!("bot: device skipped the data phase; using its status wrapper");
                return finish_status(&wrapper, tag, endpoints.address, Some(data));
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
    finish_status(&csw, tag, endpoints.address, Some(data))
}

/// `usb_stor_Bulk_transport()`'s status-wrapper checks: learnt signature,
/// echoed tag, and the status code. `US_BULK_STAT_PHASE` means the device and
/// host disagree about the command's phases, and the flow chart on page 15 of
/// the spec routes it to a reset recovery rather than a retry.
fn finish_status(
    csw: &[u8; CSW_LEN],
    tag: u32,
    address: u8,
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
    let residue = u32::from_le_bytes([csw[8], csw[9], csw[10], csw[11]]);
    info!(
        "bot: addr {address} csw tag {echoed_tag} status {} residue {residue}",
        csw[12]
    );
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
    transfer(
        hcd,
        endpoints,
        &command,
        DataPhase::In(usize::from(count) * 512),
        1,
    )
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
    if !blocks.len().is_multiple_of(512) {
        return Err(KError::InvalidInput);
    }
    let count = (blocks.len() / 512) as u16;
    let mut command = [0u8; 10];
    command[0] = 0x2A; // WRITE(10)
    command[2..6].copy_from_slice(&lba.to_be_bytes());
    command[7..9].copy_from_slice(&count.to_be_bytes());
    transfer(hcd, endpoints, &command, DataPhase::Out(blocks), 4).map(|_| ())
}

/// Reads the filesystem superblock and reports what it finds.
///
/// An ext4 filesystem created directly on the device has no MBR, so sector 0
/// carries nothing recognisable; the superblock lives 1024 bytes in, with
/// `s_magic` at +0x38 and the volume label at +0x78. Checking that is a real
/// verdict on the bulk path, where "sector 0 is all zeros" is not.
pub fn probe_filesystem(hcd: &mut dyn Hcd, endpoints: &BotEndpoints) -> bool {
    for attempt in 0..TRIES {
        match read_sectors(hcd, endpoints, 0, 3) {
            Ok(sectors) => {
                if sectors.len() < 1536 {
                    log::error!("bot: short read: {} bytes", sectors.len());
                    return false;
                }
                let superblock = &sectors[1024..];
                let magic = u16::from_le_bytes([superblock[0x38], superblock[0x39]]);
                if magic == 0xEF53 {
                    let label = core::str::from_utf8(&superblock[0x78..0x88])
                        .unwrap_or("?")
                        .trim_end_matches('\0')
                        .trim();
                    log::info!("bot: ext2/3/4 superblock found, volume label `{label}`");
                    return true;
                } else {
                    // Not ext4 -- say what the media actually is, so "the driver
                    // read garbage" and "the stick is exFAT/FAT32" cannot be
                    // confused: the signatures are at fixed offsets in sector 0,
                    // and a boot sector that starts with a jump instruction plus
                    // the usual OEM/tail bytes proves the data came back intact.
                    let fs_type = if &sectors[3..11] == b"EXFAT   " {
                        "exFAT"
                    } else if &sectors[82..87] == b"FAT32" {
                        "FAT32"
                    } else if &sectors[54..59] == b"FAT16" || &sectors[54..59] == b"FAT12" {
                        "FAT"
                    } else if sectors[510] == 0x55 && sectors[511] == 0xAA {
                        "unknown (sector 0 has a valid boot signature)"
                    } else {
                        "none (sector 0 has no boot signature)"
                    };
                    log::info!(
                        "bot: no ext4 superblock at offset 1024, magic {magic:#06x}; media looks \
                         like {fs_type}: sector0 {:02x?} ... {:02x?}",
                        &sectors[..8],
                        &sectors[504..512],
                    );
                    // Diagnostic, and the only thing that separates "the medium
                    // has no filesystem" from "our reads come back empty": two
                    // LBAs that a bare ext4 always fills (block 1 holds the group
                    // descriptors, and 1 MiB in is inside the first group).
                    for lba in [8u32, 2048u32] {
                        match read_sectors(hcd, endpoints, lba, 1) {
                            Ok(sector) => log::info!(
                                "bot: lba {lba} reads {:02x?} ... {:02x?}",
                                &sector[..8],
                                &sector[504..512]
                            ),
                            Err(err) => log::warn!("bot: lba {lba} read failed: {err:?}"),
                        }
                    }
                }
                return false;
            }
            Err(err) => log::warn!("bot: read attempt {attempt} failed: {err:?}"),
        }
    }
    false
}
/// `TEST UNIT READY`: no data stage, only the transport status means anything.
///
/// `sd_spinup_disk()` uses it as the readiness gate before anything else is
/// asked of the medium, because a stick that has just been powered answers
/// commands with `NOT READY` until its flash controller is up.
pub fn test_unit_ready(hcd: &mut dyn Hcd, endpoints: &BotEndpoints) -> KResult<()> {
    let command = [0u8, 0, 0, 0, 0, 0];
    transfer(hcd, endpoints, &command, DataPhase::None, 0).map(|_| ())
}

/// One sense triple from `REQUEST SENSE` (`scsi_normalize_sense()`'s fields).
#[derive(Clone, Copy, Debug)]
pub struct Sense {
    pub response_code: u8,
    pub sense_key: u8,
    pub asc: u8,
    pub ascq: u8,
}

pub const SENSE_KEY_NOT_READY: u8 = 0x02;
pub const SENSE_KEY_UNIT_ATTENTION: u8 = 0x06;

/// `REQUEST SENSE`: what the device refused the last command for.
pub fn request_sense(hcd: &mut dyn Hcd, endpoints: &BotEndpoints) -> KResult<Sense> {
    let mut command = [0u8; 6];
    command[0] = 0x03; // REQUEST SENSE
    command[4] = 18; // allocation length
    let data = transfer(hcd, endpoints, &command, DataPhase::In(18), 5)?;
    if data.len() < 14 {
        return Err(KError::InvalidInput);
    }
    Ok(Sense {
        response_code: data[0] & 0x7f,
        sense_key: data[2] & 0x0f,
        asc: data[12],
        ascq: data[13],
    })
}

/// `sd_spinup_disk()`: poll `TEST UNIT READY` until the device reports ready.
///
/// Transcribed from `drivers/scsi/sd.c:2156`:
///
/// - up to three `TEST UNIT READY`s per round, retrying while the status is not
///   good or the sense says `UNIT_ATTENTION` (a media change is acknowledged by
///   failing the next command once);
/// - `NOT READY` → send `START STOP UNIT` with the start bit (spin up), then wait
///   one second before asking again;
/// - `UNIT_ATTENTION` with ASC `0x28` -- the "not ready to ready transition"
///   that the reference calls out as characteristic of cheap flash sticks --
///   waits one second too;
/// - any other sense is pointless to loop on, and the reference stops.
///
/// The reference's spin-up window is 100 s (or 5 s for the flash-stick case).
/// Ten rounds keeps a boot bounded while still covering the sticks that need a
/// couple of seconds; the total is logged so a device that needs more shows up
/// as a number rather than as a hang.
pub fn wait_until_ready(hcd: &mut dyn Hcd, endpoints: &BotEndpoints) -> KResult<()> {
    const ROUNDS: usize = 10;
    let mut ready = false;
    for round in 0..ROUNDS {
        let mut tried = 0;
        let mut sense = None;
        let mut last_error = None;
        loop {
            tried += 1;
            match test_unit_ready(hcd, endpoints) {
                Ok(()) => {
                    ready = true;
                    break;
                }
                Err(err) => {
                    last_error = Some(err);
                    // The transport failed, so there is no status to interpret
                    // and no sense to ask for; the reference's retry loop breaks
                    // out on anything that is not a command failure.
                    sense = request_sense(hcd, endpoints).ok();
                }
            }
            let retry = sense.is_some_and(|s| s.sense_key == SENSE_KEY_UNIT_ATTENTION);
            if tried >= 3 || !retry {
                break;
            }
        }
        if ready {
            if round > 0 {
                info!(
                    "bot: addr {} ready after {round} wait(s)",
                    endpoints.address
                );
            }
            return Ok(());
        }
        let Some(sense) = sense else {
            // `sd_spinup_disk()`: "no sense, TUR either succeeded or failed with
            // a status error" -- there is nothing to wait for.
            log::warn!(
                "bot: addr {} test unit ready failed without sense: {:?}",
                endpoints.address,
                last_error
            );
            return Err(last_error.unwrap_or(KError::Io));
        };
        match (sense.sense_key, sense.asc, sense.ascq) {
            (SENSE_KEY_NOT_READY, 0x04, 0x03)
            | (SENSE_KEY_NOT_READY, 0x04, 0x0b)
            | (SENSE_KEY_NOT_READY, 0x04, 0x0c)
            | (SENSE_KEY_NOT_READY, 0x04, 0x1b) => {
                // Manual intervention / standby / unavailable / sanitizing: the
                // reference breaks out for all four.
                log::warn!(
                    "bot: addr {} not ready and will not become ready (asc {:#04x}/{:#04x})",
                    endpoints.address,
                    sense.asc,
                    sense.ascq
                );
                return Err(KError::Io);
            }
            (SENSE_KEY_NOT_READY, ..) => {
                if round == 0 {
                    // "Spinning up disk...": START STOP UNIT with the start bit
                    // and the no-flush bit set (`cmd[1] = 1`, `cmd[4] = 1`).
                    let mut command = [0u8; 10];
                    command[0] = 0x1b;
                    command[1] = 1;
                    command[4] = 1;
                    if let Err(err) = transfer(hcd, endpoints, &command, DataPhase::None, 6) {
                        log::warn!("bot: start stop unit failed: {err:?}");
                    }
                }
                khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1_000_000));
            }
            (SENSE_KEY_UNIT_ATTENTION, 0x28, _) => {
                khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1_000_000));
            }
            _ => {
                log::warn!(
                    "bot: addr {} unit not ready, sense key {:#04x} asc {:#04x}/{:#04x}",
                    endpoints.address,
                    sense.sense_key,
                    sense.asc,
                    sense.ascq
                );
                return Err(KError::Io);
            }
        }
    }
    log::error!(
        "bot: addr {} never became ready after {ROUNDS} rounds",
        endpoints.address
    );
    Err(KError::TimedOut)
}

/// One block-device view of a mass-storage device.
///
/// A stick with a partition table carries its filesystem on a partition, so the
/// consumer has to read at a partition-relative LBA -- the same split
/// `mmc_blk`/`sd` make between the whole disk and `…pN`
/// (`scan_partitions()` in `drivers/subsystems/mmc/src/blk.rs` is the local
/// transcription of Linux `check_partition()`).
#[derive(Clone, Copy, Debug)]
pub struct VolumeView {
    /// Partition number (`0` = the whole device).
    pub part: u32,
    /// First LBA of the view, relative to the device.
    pub start_lba: u64,
    /// Size of the view in 512-byte blocks.
    pub blocks: u64,
}

/// Reads the partition table and decides which block-device views to publish.
///
/// Follows Linux `check_partition()` -> `efi_partition()`/`msdos_partition()`
/// through the local `scan_partitions()`: a GPT-protective MBR is parsed as GPT,
/// otherwise the four primary MBR entries are used (skipping empty, protective
/// and extended markers). No usable table means the whole device is the view.
///
/// Every partition is clamped to the device: a bogus entry must not become an
/// out-of-range transfer.
pub fn plan_views(
    hcd: &mut dyn Hcd,
    endpoints: &BotEndpoints,
    total_blocks: u64,
) -> Vec<VolumeView> {
    let whole = VolumeView {
        part: 0,
        start_lba: 0,
        blocks: total_blocks,
    };
    let Ok(lba0) = read_sectors(hcd, endpoints, 0, 1) else {
        return alloc::vec![whole];
    };
    if u16::from_le_bytes([lba0[510], lba0[511]]) != 0xAA55 {
        return alloc::vec![whole];
    }
    let mut raw: Vec<(u64, u64)> = Vec::new();
    if lba0[450] == 0xEE {
        // GPT: the header lives in LBA 1 and points at the first entry block.
        let mut header = [0u8; 512];
        if let Ok(read) = read_sectors(hcd, endpoints, 1, 1) {
            if read.len() == 512 {
                header.copy_from_slice(&read);
            }
        }
        if header[0..8] != *b"EFI PART" {
            return alloc::vec![whole];
        }
        let entry_lba = u64::from_le_bytes(header[72..80].try_into().unwrap_or([0; 8]));
        let entry_size = u32::from_le_bytes(header[84..88].try_into().unwrap_or([0; 4])) as usize;
        if entry_lba == 0 || !(56..=512).contains(&entry_size) {
            return alloc::vec![whole];
        }
        let Ok(entries) = read_sectors(hcd, endpoints, entry_lba as u32, 1) else {
            return alloc::vec![whole];
        };
        let mut offset = 0usize;
        while offset + entry_size <= entries.len() && raw.len() < 8 {
            let entry = &entries[offset..offset + entry_size];
            if entry[0..16].iter().any(|&byte| byte != 0) {
                let first = u64::from_le_bytes(entry[32..40].try_into().unwrap_or([0; 8]));
                // `last_lba` is inclusive; the size is not the device capacity.
                let last = u64::from_le_bytes(entry[40..48].try_into().unwrap_or([0; 8]));
                if last >= first {
                    raw.push((first, last - first + 1));
                }
            }
            offset += entry_size;
        }
    } else {
        for index in 0..4usize {
            let base = 446 + index * 16;
            let ptype = lba0[base + 4];
            let lba = u32::from_le_bytes(lba0[base + 8..base + 12].try_into().unwrap_or([0; 4]));
            let sectors =
                u32::from_le_bytes(lba0[base + 12..base + 16].try_into().unwrap_or([0; 4]));
            if ptype != 0 && lba != 0 && sectors != 0 && ptype != 0xEE && ptype != 0x05 {
                // The type byte is the cheapest answer to "which filesystem would
                // have to exist before this partition can be mounted": 0x01/0x04/
                // 0x06/0x0b/0x0c are FAT, 0x07 is NTFS/exFAT, 0x83 is Linux.
                log::info!(
                    "bot: addr {} mbr entry {index}: type {ptype:#04x} lba {lba} sectors {sectors}",
                    endpoints.address
                );
                raw.push((u64::from(lba), u64::from(sectors)));
            } else if ptype != 0 {
                log::info!(
                    "bot: addr {} mbr entry {index}: type {ptype:#04x} ignored (lba {lba} sectors \
                     {sectors})",
                    endpoints.address
                );
            }
        }
    }
    if raw.is_empty() {
        return alloc::vec![whole];
    }
    let mut views = Vec::new();
    for (index, (start_lba, sectors)) in raw.into_iter().enumerate() {
        let part = index as u32 + 1;
        if start_lba >= total_blocks {
            log::warn!(
                "bot: ignoring partition {part} of addr {} (start LBA {start_lba} outside the \
                 device)",
                endpoints.address
            );
            continue;
        }
        let blocks = sectors.min(total_blocks - start_lba);
        if blocks == 0 {
            continue;
        }
        views.push(VolumeView {
            part,
            start_lba,
            blocks,
        });
    }
    if views.is_empty() {
        log::warn!(
            "bot: addr {} has a partition table but no usable entry",
            endpoints.address
        );
        return alloc::vec![whole];
    }
    views
}

/// Round-trips one sector through `WRITE(10)` and back, then restores it.
///
/// Until this existed, every `WRITE(10)` shipped 512 zero bytes: the data phase
/// allocated a fresh buffer and nothing ever copied the caller's blocks into it,
/// so a read-modify-write filesystem wrote zeros over its own metadata. The
/// damage is silent -- the command succeeds, the status wrapper reports no
/// residue -- so the only honest proof the write path works is to write a known
/// pattern somewhere and read it back.
///
/// Only the last sector is touched, and it is written back verbatim afterwards.
/// This is called only when `probe_filesystem()` found no filesystem, so a volume
/// that a user can mount never sees it.
pub fn verify_write_path(hcd: &mut dyn Hcd, endpoints: &BotEndpoints) {
    let Ok(capacity) = read_capacity(hcd, endpoints) else {
        log::warn!("bot: write self-test skipped: no capacity");
        return;
    };
    if capacity.len() < 8 {
        return;
    }
    let last = u32::from_be_bytes([capacity[0], capacity[1], capacity[2], capacity[3]]);
    let Ok(original) = read_sectors(hcd, endpoints, last, 1) else {
        log::warn!("bot: write self-test skipped: cannot read last sector {last}");
        return;
    };
    let mut pattern = [0u8; 512];
    pattern[..8].copy_from_slice(b"XKUSBWRT");
    pattern[8..16].copy_from_slice(&(last as u64).to_le_bytes());
    if let Err(err) = write_sectors(hcd, endpoints, last, &pattern) {
        log::error!("bot: write self-test failed at lba {last}: {err:?}");
        return;
    }
    match read_sectors(hcd, endpoints, last, 1) {
        Ok(read_back) if read_back.len() == 512 && read_back[..16] == pattern[..16] => {
            log::info!("bot: write self-test ok at lba {last} (pattern read back)");
        }
        Ok(read_back) => log::error!(
            "bot: write self-test mismatch at lba {last}: wrote {:02x?} read {:02x?}",
            &pattern[..8],
            &read_back[..8.min(read_back.len())]
        ),
        Err(err) => log::error!("bot: write self-test read-back failed: {err:?}"),
    }
    // Leave the medium as it was found.
    if let Err(err) = write_sectors(hcd, endpoints, last, &original) {
        log::error!("bot: write self-test restore failed at lba {last}: {err:?}");
    }
}

/// `INQUIRY`: the device's vendor and product strings.
pub fn inquiry(hcd: &mut dyn Hcd, endpoints: &BotEndpoints) -> KResult<Vec<u8>> {
    let mut command = [0u8; 6];
    command[0] = 0x12; // INQUIRY
    command[4] = 36; // allocation length
    transfer(hcd, endpoints, &command, DataPhase::In(36), 2)
}

/// `read_capacity()` behind the readiness gate, resetting only on failure.
///
/// This is the reference's shape, and it is not the same thing as resetting
/// first: `usb_stor_invoke_transport()` sends a command, and only routes a
/// *transport error* to `Handle_Errors` -> `usb_stor_port_reset()` ->
/// `usb_stor_reset_common()`. Resetting a healthy device before every command
/// costs six seconds each time and, on this stick, leaves it NAKing the command
/// that follows the reset.
pub fn read_capacity_ready(hcd: &mut dyn Hcd, endpoints: &BotEndpoints) -> KResult<Vec<u8>> {
    wait_until_ready(hcd, endpoints)?;
    match read_capacity(hcd, endpoints) {
        Ok(data) => Ok(data),
        Err(err) => {
            log::warn!(
                "bot: addr {} read capacity failed ({err:?}); bulk-only reset, then one retry",
                endpoints.address
            );
            reset_recovery(hcd, endpoints)?;
            wait_until_ready(hcd, endpoints)?;
            read_capacity(hcd, endpoints)
        }
    }
}

/// `READ CAPACITY (10)`: last logical block address and block size.
pub fn read_capacity(hcd: &mut dyn Hcd, endpoints: &BotEndpoints) -> KResult<Vec<u8>> {
    let command = [0x25u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    transfer(hcd, endpoints, &command, DataPhase::In(8), 3)
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
    // `usb_stor_reset_common()`: "Give the device some time to recover from the
    // reset" -- `wait_event_interruptible_timeout(..., HZ*6)`, i.e. six seconds.
    // Waiting less leaves the device answering `NOT READY`/NAKing, which looks
    // exactly like a wedged endpoint on the host side.
    khal::time::busy_wait(ktime_types::TimeSpan::from_secs(6));
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
        } else {
            // `usb_hcd_reset_endpoint()`: the device reset its toggle with the
            // clear-halt, so the controller's copy has to go back to DATA0 too.
            // Without this the two diverge and every later packet is discarded
            // by the device.
            hcd.clear_endpoint_halt(
                endpoints.address,
                endpoint,
                endpoint == endpoints.endpoint_out,
            );
        }
    }
    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}
