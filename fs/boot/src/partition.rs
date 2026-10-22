// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal partition-table scan for the root block device — the
//! `fs/block/partition.rs` shape from the verified reference: locate the
//! first usable partition (GPT first, legacy MBR fallback) and expose it
//! as its own block device so the root filesystem mounts from the
//! partition, not the raw disk.

use alloc::{boxed::Box, format, string::String, sync::Arc, vec};

use devnode::DeviceNumber;
use subsys_block::{BlockDevice, BlockDeviceOperations, Gendisk, KResult};

/// GPT protective-MBR partition type.
const GPT_PROTECTIVE: u8 = 0xEE;
/// Boot/signature marker at the end of an MBR sector.
const MBR_SIGNATURE: u16 = 0xAA55;

/// Parse the partition table on `dev` and return the LBA of the first
/// usable partition. `Ok(0)` means "no recognisable table — a direct
/// filesystem at LBA 0"; errors are read failures or malformed tables.
pub fn find_root_partition_lba(dev: &BlockDevice) -> KResult<u64> {
    let block_size = dev.block_size();
    let mut sector = alloc::vec![0u8; block_size];

    // LBA 0: legacy MBR (or the GPT protective MBR).
    dev.read_block(0, &mut sector)?;
    let signature = u16::from_le_bytes([sector[510], sector[511]]);
    if signature != MBR_SIGNATURE {
        return Ok(0);
    }
    let p1_type = sector[450];
    let p1_lba = u32::from_le_bytes([sector[454], sector[455], sector[456], sector[457]]) as u64;
    if p1_type != GPT_PROTECTIVE {
        // Plain MBR: partition 1 when it is a real entry.
        return Ok(if p1_type != 0 && p1_lba != 0 {
            p1_lba
        } else {
            0
        });
    }

    // GPT protective MBR: parse the GPT header at LBA 1.
    dev.read_block(1, &mut sector)?;
    if sector[0..8] != *b"EFI PART" {
        return Ok(0);
    }
    let entry_lba = u64::from_le_bytes(sector[72..80].try_into().map_err(|_| kerrno::KError::Io)?);
    let entry_size =
        u32::from_le_bytes(sector[84..88].try_into().map_err(|_| kerrno::KError::Io)?) as usize;
    if entry_size < 56 || entry_size > block_size {
        return Err(kerrno::KError::Io);
    }

    // First used partition entry (non-zero type GUID) — conventionally
    // the root partition.
    dev.read_block(entry_lba, &mut sector)?;
    let mut off = 0usize;
    while off + 40 <= block_size {
        let entry = &sector[off..];
        let used = entry[0..16].iter().any(|&b| b != 0);
        if used {
            return Ok(u64::from_le_bytes(
                entry[32..40].try_into().map_err(|_| kerrno::KError::Io)?,
            ));
        }
        off += entry_size;
    }
    Ok(0)
}

/// Block operations forwarding to the underlying disk with a fixed LBA
/// offset (the partition's start).
struct PartitionBlockOps {
    inner: Arc<BlockDevice>,
    start_lba: u64,
}

impl BlockDeviceOperations for PartitionBlockOps {
    fn num_blocks(&self) -> u64 {
        self.inner.num_blocks().saturating_sub(self.start_lba)
    }

    fn block_size(&self) -> usize {
        self.inner.block_size()
    }

    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> KResult {
        self.inner.read_block(
            block_id
                .checked_add(self.start_lba)
                .ok_or(kerrno::KError::Io)?,
            buf,
        )
    }

    fn write_block(&self, block_id: u64, buf: &[u8]) -> KResult {
        self.inner.write_block(
            block_id
                .checked_add(self.start_lba)
                .ok_or(kerrno::KError::Io)?,
            buf,
        )
    }

    fn flush(&self) -> KResult {
        self.inner.flush()
    }
}

/// Scan `disk` for a root partition and, when one exists, register it as
/// its own block device (`<disk-name>p<N>`, minor `first_minor + N`).
///
/// Returns the mount source (`/dev/<name>`) — either the partition device
/// or, with no partition table, the raw disk itself.
pub fn resolve_root_source(
    disk: &Arc<BlockDevice>,
    device_number: DeviceNumber,
) -> KResult<String> {
    let start_lba = find_root_partition_lba(disk)?;
    if start_lba == 0 {
        return Ok(format!("/dev/{}", disk.name()));
    }

    let base = disk.name();
    let name = alloc::format!("{base}p1");
    let ops = PartitionBlockOps {
        inner: disk.clone(),
        start_lba,
    };
    let partition = Gendisk::new(
        name,
        device_number.major(),
        device_number.minor() + 1,
        1,
        Box::new(ops),
    )
    .map_err(|_| kerrno::KError::Io)?;
    let device_number = partition.device_number();
    let partition_name = alloc::string::String::from(partition.name());
    subsys_block::add_disk(Arc::new(partition))?;

    // The devfs node is what the root-mount source resolves through.
    devnode::publish(
        driver_base::PublicationId::next(),
        "block",
        partition_name.clone(),
        device_number,
        &alloc::format!("/dev/{base}"),
        None,
        alloc::vec![],
        None,
    );

    log::info!("fs-boot: root partition {partition_name} at LBA {start_lba}");
    Ok(alloc::format!("/dev/{partition_name}"))
}
