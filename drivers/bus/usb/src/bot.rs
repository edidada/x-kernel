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
    tag: u32,
) -> KResult<Vec<u8>> {
    let cbw = build_cbw(tag, transfer_length as u32, is_in, command);
    let mut written = [0u8; CBW_LEN];
    written.copy_from_slice(&cbw);
    let sent = hcd.bulk_transfer(
        endpoints.address,
        endpoints.endpoint_out,
        &mut written,
        false,
    )?;
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
        )?;
        if read == 0 {
            break;
        }
        got += read;
    }
    if got < CSW_LEN {
        return Err(KError::Io);
    }
    let signature = u32::from_le_bytes([csw[0], csw[1], csw[2], csw[3]]);
    let status = csw[12];
    if signature != CSW_SIGNATURE {
        log::error!("bot: bad status signature {signature:#010x}");
        return Err(KError::Io);
    }
    if status != 0 {
        log::error!("bot: command status {status}");
        return Err(KError::Io);
    }
    Ok(data)
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
