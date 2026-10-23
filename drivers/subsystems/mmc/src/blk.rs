// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The mmc block consumer — Linux `drivers/mmc/core/block.c` (`mmc_blk`),
//! driving the card through [`MmcHost`] and publishing one disk into the
//! block class.
//!
//! Binds on the card bus: the host driver discovers the card, adds it to
//! the domain, and the best-scoring consumer claims it here. I/O goes
//! through the core helpers (`read_blocks`/`write_blocks`), which map onto
//! CMD17/CMD18 (reads, with auto-stop) and per-block CMD24 (writes).
//!
//! The consumer also owns the **partition scan** (Linux `mmc_blk` reads
//! the partition table and creates `mmcblk0pN` devices): every used
//! partition is published as its own disk, which is what a root
//! filesystem actually mounts from.

use alloc::{boxed::Box, string::String, sync::Arc};

use driver_base::{Device, DriverOps, ProbeError, Publication, RuntimeError};
use kerrno::{KError, KResult};
use subsys_block::{BlockDeviceImpl, BlockDeviceOperations, Gendisk, class::BLOCK};

use crate::{
    BLOCK_SIZE,
    bus::{CardKind, MmcBus, MmcCardData, MmcIdEntry},
    read_blocks, write_blocks,
};

/// Linux `MMC_BLOCK_MAJOR` — the mmc block devices major.
const MMC_BLOCK_MAJOR: u32 = 179;

/// `static const struct mmc_driver mmc_driver` id table: every SD card.
static IDS: &[MmcIdEntry] = &[MmcIdEntry::kind(CardKind::Sd)];

/// I/O operations over one card (Linux `mmc_blk_drvdata`'s request path).
/// `start_lba` is zero for the whole-device view and the partition's
/// start for a partition view (`mmcblk0pN`).
struct MmcBlkOps {
    card: MmcCardData,
    start_lba: u64,
}

impl MmcBlkOps {
    /// Device address for a partition-relative block.
    fn device_address(&self, block_id: u64) -> u32 {
        self.card.device_address(block_id + self.start_lba)
    }
}

impl BlockDeviceOperations for MmcBlkOps {
    fn num_blocks(&self) -> u64 {
        self.card.sectors.saturating_sub(self.start_lba)
    }

    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> KResult {
        let addr = self.device_address(block_id);
        let blocks = blocks_in(buf.len()).map_err(|_| KError::Io)?;
        read_blocks(self.card.host.as_ref(), addr, blocks, buf).map_err(|err| err.to_kerror())
    }

    fn write_block(&self, block_id: u64, buf: &[u8]) -> KResult {
        let addr = self.device_address(block_id);
        let blocks = blocks_in(buf.len()).map_err(|_| KError::Io)?;
        write_blocks(
            self.card.host.as_ref(),
            addr,
            blocks,
            buf,
            self.card.rca,
            self.card.block_addressed,
        )
        .map_err(|err| err.to_kerror())
    }

    fn flush(&self) -> KResult {
        // SD media has no write-back cache to flush; each write's data
        // phase only completes once programming-busy clears (checked per
        // block by the core helper).
        Ok(())
    }
}

/// `usize` byte length → whole 512-byte block count (the block layer
/// hands in block-multiple buffers).
fn blocks_in(bytes: usize) -> Result<u16, ()> {
    if !bytes.is_multiple_of(BLOCK_SIZE) {
        return Err(());
    }
    u16::try_from(bytes / BLOCK_SIZE).map_err(|_| ())
}

/// Driver-private data: every published disk (whole card + partitions).
/// Dropping unpublishes all of them.
pub struct MmcBlkData {
    _publications: alloc::vec::Vec<Publication<BlockDeviceImpl>>,
    _disks: alloc::vec::Vec<Arc<Gendisk>>,
}

/// The card-bus consumer (Linux `mmc_blk` driver).
pub struct MmcBlkDriver;

impl DriverOps<MmcBus> for MmcBlkDriver {
    type Data = MmcBlkData;

    fn probe(&self, dev: &mut Device<'_, MmcBus>) -> Result<MmcBlkData, ProbeError> {
        // A snapshot of the bus vocabulary: the card handle and geometry
        // outlive this probe through the Gendisk ops.
        let info = dev.info();
        let bus_data: &MmcCardData = info.bus_data();
        if bus_data.kind != CardKind::Sd {
            return Err(ProbeError::Fatal(RuntimeError::Unsupported));
        }
        let card = MmcCardData {
            kind: bus_data.kind,
            slot: bus_data.slot,
            rca: bus_data.rca,
            cid: bus_data.cid,
            sectors: bus_data.sectors,
            block_addressed: bus_data.block_addressed,
            high_speed: bus_data.high_speed,
            bus_width_4bit: bus_data.bus_width_4bit,
            host: bus_data.host.clone(),
        };

        // Partition scan first (Linux mmc_blk creates mmcblk0pN devices):
        // one disk per used partition, falling back to the whole card
        // when the card has no partition table.
        let partitions = scan_partitions(bus_data.host.as_ref());
        let slot = bus_data.slot;
        let mut views: alloc::vec::Vec<(alloc::string::String, u64)> = alloc::vec::Vec::new();
        if partitions.is_empty() {
            views.push((alloc::format!("mmcblk{slot}"), 0));
        } else {
            for (index, start_lba) in partitions.iter().enumerate() {
                views.push((alloc::format!("mmcblk{slot}p{}", index + 1), *start_lba));
            }
        }

        let mut publications: alloc::vec::Vec<Publication<BlockDeviceImpl>> =
            alloc::vec::Vec::new();
        let mut disks: alloc::vec::Vec<Arc<Gendisk>> = alloc::vec::Vec::new();
        for (name, start_lba) in views {
            let ops = Box::new(MmcBlkOps {
                card: MmcCardData {
                    kind: bus_data.kind,
                    slot: bus_data.slot,
                    rca: bus_data.rca,
                    cid: bus_data.cid,
                    sectors: bus_data.sectors,
                    block_addressed: bus_data.block_addressed,
                    high_speed: bus_data.high_speed,
                    bus_width_4bit: bus_data.bus_width_4bit,
                    host: bus_data.host.clone(),
                },
                start_lba,
            });
            let path: String = alloc::format!("mmc/slot{}:{:04x}", bus_data.slot, bus_data.rca);
            let modalias: String = String::from("sd:block");
            let disk = Arc::new(
                Gendisk::new(name.clone(), MMC_BLOCK_MAJOR, 0, 1, ops)
                    .map_err(|_| ProbeError::Fatal(RuntimeError::Invalid))?
                    .with_devnode_context(&path, Some(modalias)),
            );
            let publication = dev
                .publish(&BLOCK, disk.clone(), name.clone())
                .map_err(|_| ProbeError::Fatal(RuntimeError::Io))?;
            log::info!(
                "mmcblk: published {name} (start LBA {start_lba}, {} sectors, {}-bit bus)",
                bus_data.sectors,
                if bus_data.bus_width_4bit { 4 } else { 1 },
            );
            publications.push(publication);
            disks.push(disk);
        }

        lba0_integrity_probe(bus_data.host.as_ref());

        Ok(MmcBlkData {
            _publications: publications,
            _disks: disks,
        })
    }
}

/// Parse the card's partition table (GPT first, legacy MBR fallback) and
/// return the start LBAs of all used partitions. Empty = no table (the
/// whole card is one filesystem).
fn scan_partitions(host: &dyn crate::MmcHost) -> alloc::vec::Vec<u64> {
    let mut lba0 = [0u8; 512];
    if read_blocks(host, 0, 1, &mut lba0).is_err() {
        return alloc::vec::Vec::new();
    }
    if u16::from_le_bytes([lba0[510], lba0[511]]) != 0xAA55 {
        return alloc::vec::Vec::new();
    }

    // GPT protective MBR: parse the GPT header at LBA 1 and its first
    // entry block.
    if lba0[450] == 0xEE {
        let mut header = [0u8; 512];
        if read_blocks(host, 1, 1, &mut header).is_err() || header[0..8] != *b"EFI PART" {
            return alloc::vec::Vec::new();
        }
        let entry_lba = u64::from_le_bytes(header[72..80].try_into().unwrap_or([0; 8]));
        let entry_size = u32::from_le_bytes(header[84..88].try_into().unwrap_or([0; 4])) as usize;
        if entry_lba == 0 || entry_size < 56 || entry_size > 512 {
            return alloc::vec::Vec::new();
        }
        let mut entries = [0u8; 512];
        if read_blocks(host, entry_lba as u32, 1, &mut entries).is_err() {
            return alloc::vec::Vec::new();
        }
        let mut starts = alloc::vec::Vec::new();
        let mut off = 0usize;
        while off + entry_size <= 512 && starts.len() < 8 {
            let entry = &entries[off..off + entry_size];
            if entry[0..16].iter().any(|&b| b != 0) {
                starts.push(u64::from_le_bytes(
                    entry[32..40].try_into().unwrap_or([0; 8]),
                ));
            }
            off += entry_size;
        }
        return starts;
    }

    // Legacy MBR: the four primary entries (type/LBA non-zero, skipping
    // the GPT-protective and extended markers).
    let mut starts = alloc::vec::Vec::new();
    for entry_index in 0..4usize {
        let base = 446 + entry_index * 16;
        let ptype = lba0[base + 4];
        let lba = u32::from_le_bytes(lba0[base + 8..base + 12].try_into().unwrap_or([0; 4]));
        if ptype != 0 && lba != 0 && ptype != 0xEE && ptype != 0x05 {
            starts.push(u64::from(lba));
        }
    }
    starts
}

fn lba0_integrity_probe(host: &dyn crate::MmcHost) {
    // Ladder: the failing size localizes the data-path limit (FIFO
    // geometry / threshold / transfer-length dependent).
    for size in [8usize, 64, 256, 512] {
        let mut buf = [0u8; 512];
        if read_blocks(host, 0, 1, &mut buf[..size]).is_err() {
            log::warn!("mmcblk: LBA0 read {size}B FAILED");
            return;
        }
        let head: alloc::vec::Vec<alloc::string::String> =
            buf[..8].iter().map(|b| alloc::format!("{b:02x}")).collect();
        log::info!("mmcblk: LBA0 read {size}B ok, head: {}", head.join(" "));
    }
}

driver_base::declare_driver!(MmcBus, MmcBlkDriver, "mmc-blk", IDS);
