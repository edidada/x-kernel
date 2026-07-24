// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal partition-table parsing for block-backed root filesystems.
//!
//! [`find_root_partition_lba`] locates the first usable partition on a block
//! device — GPT first, with a legacy MBR fallback — so the root filesystem can
//! be mounted from a partition rather than the raw device. A raw device with
//! no partition table (a direct filesystem at LBA 0) yields `0`.
//! [`PartitionBlockDevice`] exposes such a partition as an offset
//! [`BlockDevice`].
//!
//! Assumes 512-byte logical blocks (the SD/eMMC case). The structures are
//! indexed at the start of each 512-byte LBA.

use alloc::{sync::Arc, vec};

use block::{BlockDevice, Device, DeviceKind, DriverError, DriverResult};

/// GPT protective-MBR partition type.
const GPT_PROTECTIVE: u8 = 0xEE;
/// Boot/signature marker at the end of an MBR sector.
const MBR_SIGNATURE: u16 = 0xAA55;

/// Parse the partition table on `dev` and return the LBA where its first
/// usable partition starts.
///
/// Returns `Ok(0)` when the device has no recognisable partition table (a
/// direct filesystem at LBA 0). Returns an error only on a read failure or a
/// malformed table.
pub fn find_root_partition_lba(dev: &dyn BlockDevice) -> DriverResult<u64> {
    let bs = dev.block_size();
    if bs < 512 {
        return Err(DriverError::Io);
    }
    let mut sector = vec![0u8; bs];

    // LBA 0: MBR (or the GPT protective MBR).
    dev.read_block(0, &mut sector)?;
    let sig = u16::from_le_bytes([sector[510], sector[511]]);
    if sig != MBR_SIGNATURE {
        return Ok(0); // no legacy MBR signature -> assume a direct filesystem
    }
    let p1_type = sector[450];
    let p1_lba = u32::from_le_bytes([sector[454], sector[455], sector[456], sector[457]]) as u64;
    if p1_type != GPT_PROTECTIVE {
        // Plain MBR: use partition 1 if it is a real partition.
        return Ok(if p1_type != 0 && p1_lba != 0 {
            p1_lba
        } else {
            0
        });
    }

    // GPT protective MBR: parse the GPT header at LBA 1.
    dev.read_block(1, &mut sector)?;
    if &sector[0..8] != b"EFI PART" {
        return Ok(0);
    }
    let entry_lba = u64::from_le_bytes(sector[72..80].try_into().unwrap());
    let entry_size = u32::from_le_bytes(sector[84..88].try_into().unwrap()) as usize;
    if !(56..=bs).contains(&entry_size) {
        return Err(DriverError::Io);
    }

    // Read the first block of partition entries and pick the first used one
    // (non-zero type GUID). The root partition is conventionally the first.
    dev.read_block(entry_lba, &mut sector)?;
    let mut off = 0usize;
    while off + 40 <= bs {
        let entry = &sector[off..];
        let type_guid_used = entry[0..16].iter().any(|&b| b != 0);
        if type_guid_used {
            let first_lba = u64::from_le_bytes(entry[32..40].try_into().unwrap());
            return Ok(first_lba);
        }
        off += entry_size;
    }
    Ok(0)
}

/// A block device that exposes a single partition at a fixed LBA offset.
///
/// Access to logical block `n` is forwarded to the underlying device at
/// `start_lba + n`, so a filesystem mounted on a `PartitionBlockDevice` sees
/// only the partition's contents.
pub struct PartitionBlockDevice {
    inner: Arc<dyn BlockDevice>,
    start_lba: u64,
}

impl PartitionBlockDevice {
    /// Wrap `inner` so that logical block 0 maps to `start_lba` on it.
    pub fn new(inner: Arc<dyn BlockDevice>, start_lba: u64) -> Self {
        Self { inner, start_lba }
    }
}

impl Device for PartitionBlockDevice {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn device_kind(&self) -> DeviceKind {
        self.inner.device_kind()
    }
}

impl BlockDevice for PartitionBlockDevice {
    fn num_blocks(&self) -> u64 {
        self.inner.num_blocks().saturating_sub(self.start_lba)
    }

    fn block_size(&self) -> usize {
        self.inner.block_size()
    }

    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        let lba = block_id
            .checked_add(self.start_lba)
            .ok_or(DriverError::Io)?;
        self.inner.read_block(lba, buf)
    }

    fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult {
        let lba = block_id
            .checked_add(self.start_lba)
            .ok_or(DriverError::Io)?;
        self.inner.write_block(lba, buf)
    }

    fn flush(&self) -> DriverResult {
        self.inner.flush()
    }
}
