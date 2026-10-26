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
use core::sync::atomic::Ordering;

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
    /// This view's size in 512-byte blocks: the card capacity for the
    /// whole-card view, the partition's own bounds for a `mmcblk0pN` view.
    blocks: u64,
    /// Whether a multi-block (CMD18 + auto-stop) read is still worth
    /// trying. Starts from `MMC_MULTIBLOCK` and latches off the first time
    /// the controller loses a multi-block data phase, so the optimisation
    /// can never turn into a persistent I/O failure.
    multiblock: alloc::sync::Arc<core::sync::atomic::AtomicBool>,
}

impl MmcBlkOps {
    /// Device address for a partition-relative block.
    fn device_address(&self, block_id: u64) -> u32 {
        self.card.device_address(block_id + self.start_lba)
    }
}

impl BlockDeviceOperations for MmcBlkOps {
    fn num_blocks(&self) -> u64 {
        self.blocks
    }

    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> KResult {
        if !buf.len().is_multiple_of(BLOCK_SIZE) {
            return Err(KError::Io);
        }
        let host = self.card.host.as_ref();
        // The block layer may hand in more than one host transfer (the DMA
        // engine stages through a bounce window), so walk the buffer in
        // host-sized pieces, each of which stays a whole number of blocks.
        let max_bytes = host.max_transfer_bytes().max(BLOCK_SIZE);
        let mut done = 0usize;
        while done < buf.len() {
            let take = (buf.len() - done).min(max_bytes);
            let take = take - take % BLOCK_SIZE;
            let chunk = &mut buf[done..done + take];
            let blocks = blocks_in(take).map_err(|_| KError::Io)?;
            let first_block = block_id + (done / BLOCK_SIZE) as u64;
            // Multi-block first (one CMD18 for the whole piece), then the
            // durable per-block shape. The auto-stop path streams blocks
            // back-to-back, so a polled drain that gets preempted can
            // overrun the FIFO — a failure that says nothing about the
            // media. Latch it off for the remaining I/O instead of retrying
            // it on every request.
            let mut retry_per_block = true;
            if blocks > 1 && self.multiblock.load(Ordering::Acquire) {
                let addr = self.device_address(first_block);
                match read_blocks(host, addr, blocks, chunk) {
                    Ok(()) => retry_per_block = false,
                    Err(err) => {
                        self.multiblock.store(false, Ordering::Release);
                        log::warn!(
                            "mmcblk: {blocks}-block read failed ({err:?}); falling back to one \
                             CMD17 per block for the rest of this session"
                        );
                    }
                }
            }
            if retry_per_block {
                for (index, piece) in chunk.chunks_mut(BLOCK_SIZE).enumerate() {
                    let addr = self.device_address(first_block + index as u64);
                    read_blocks(host, addr, 1, piece).map_err(|err| err.to_kerror())?;
                }
            }
            done += take;
        }
        Ok(())
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

        // Partition scan first (Linux mmc_blk creates mmcblk0pN devices):
        // one disk per used partition, falling back to the whole card
        // when the card has no partition table.
        bus_data.host.recover();
        let partitions = scan_partitions(bus_data.host.as_ref());
        let slot = bus_data.slot;
        let mut views: alloc::vec::Vec<(alloc::string::String, u64, u64, u32)> =
            alloc::vec::Vec::new();
        if partitions.is_empty() {
            views.push((alloc::format!("mmcblk{slot}"), 0, bus_data.sectors, 0));
        } else {
            // Disk numbering follows Linux `mmcblk0` / `mmcblk0pN`: the
            // whole card would take minor 0, partition N takes minor N.
            // Every view is its own gendisk here, so the minor has to be
            // allocated per view — a fixed 0 makes the second publish
            // collide in `add_disk` (`AlreadyExists`) and, because the
            // probe then fails, withdraws the disks that did publish.
            for (index, (start_lba, sectors)) in partitions.iter().enumerate() {
                let part =
                    u32::try_from(index).map_err(|_| ProbeError::Fatal(RuntimeError::Invalid))? + 1;
                // Never claim blocks the card does not have: a bogus entry
                // must not turn into an out-of-range transfer.
                let sectors = (*sectors).min(bus_data.sectors.saturating_sub(*start_lba));
                if sectors == 0 || *start_lba >= bus_data.sectors {
                    log::warn!(
                        "mmcblk: ignoring partition {part} (start LBA {start_lba}, {sectors} \
                         sectors) outside the card"
                    );
                    continue;
                }
                views.push((
                    alloc::format!("mmcblk{slot}p{part}"),
                    *start_lba,
                    sectors,
                    part,
                ));
            }
        }

        let mut publications: alloc::vec::Vec<Publication<BlockDeviceImpl>> =
            alloc::vec::Vec::new();
        let mut disks: alloc::vec::Vec<Arc<Gendisk>> = alloc::vec::Vec::new();
        // One latch for every view of this card: the multi-block capability
        // belongs to the host, not to a partition.
        let multiblock = Arc::new(core::sync::atomic::AtomicBool::new(
            kbuild_config::MMC_MULTIBLOCK,
        ));
        for (name, start_lba, sectors, minor) in views {
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
                blocks: sectors,
                multiblock: multiblock.clone(),
            });
            let path: String = alloc::format!("mmc/slot{}:{:04x}", bus_data.slot, bus_data.rca);
            let modalias: String = String::from("sd:block");
            let disk = Arc::new(
                Gendisk::new(name.clone(), MMC_BLOCK_MAJOR, minor, 1, ops)
                    .map_err(|_| ProbeError::Fatal(RuntimeError::Invalid))?
                    .with_devnode_context(&path, Some(modalias)),
            );
            let publication = dev
                .publish(&BLOCK, disk.clone(), name.clone())
                .map_err(|_| ProbeError::Fatal(RuntimeError::Io))?;
            log::info!(
                "mmcblk: published {name} (minor {minor}, start LBA {start_lba}, {sectors} \
                 sectors, {}-bit bus)",
                if bus_data.bus_width_4bit { 4 } else { 1 },
            );
            publications.push(publication);
            disks.push(disk);
        }

        // Boot-time I/O self-test: re-read the first blocks and print what
        // came back. Diagnostic only, so it is opt-in (MMC_BOOT_IO_PROBE) —
        // it distinguishes "no partition table" from "the read returned
        // zeros", which is what pointed at the missing MBR on the board.
        if kbuild_config::MMC_BOOT_IO_PROBE {
            lba0_integrity_probe(bus_data.host.as_ref());
        }

        Ok(MmcBlkData {
            _publications: publications,
            _disks: disks,
        })
    }
}

/// Parse the card's partition table (GPT first, legacy MBR fallback) and
/// return each used partition as `(start_lba, sectors)`. Empty = no table
/// (the whole card is one filesystem).
///
/// The size comes from the entry's own bounds — `last_lba` is inclusive in
/// GPT, the MBR entry carries a sector count. Using the remaining card
/// capacity instead would make every partition look like it runs to the end
/// of the card, so a write near its end would land in the next partition.
fn scan_partitions(host: &dyn crate::MmcHost) -> alloc::vec::Vec<(u64, u64)> {
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
        if entry_lba == 0 || !(56..=512).contains(&entry_size) {
            return alloc::vec::Vec::new();
        }
        let mut entries = [0u8; 512];
        if read_blocks(host, entry_lba as u32, 1, &mut entries).is_err() {
            return alloc::vec::Vec::new();
        }
        let mut parts = alloc::vec::Vec::new();
        let mut off = 0usize;
        while off + entry_size <= 512 && parts.len() < 8 {
            let entry = &entries[off..off + entry_size];
            if entry[0..16].iter().any(|&b| b != 0) {
                let first = u64::from_le_bytes(entry[32..40].try_into().unwrap_or([0; 8]));
                // The partition ends at `last_lba`, inclusive; the whole
                // card's capacity is NOT this partition's size.
                let last = u64::from_le_bytes(entry[40..48].try_into().unwrap_or([0; 8]));
                if last >= first {
                    parts.push((first, last - first + 1));
                }
            }
            off += entry_size;
        }
        return parts;
    }

    // Legacy MBR: the four primary entries (type/LBA non-zero, skipping
    // the GPT-protective and extended markers).
    let mut parts = alloc::vec::Vec::new();
    for entry_index in 0..4usize {
        let base = 446 + entry_index * 16;
        let ptype = lba0[base + 4];
        let lba = u32::from_le_bytes(lba0[base + 8..base + 12].try_into().unwrap_or([0; 4]));
        let sectors = u32::from_le_bytes(lba0[base + 12..base + 16].try_into().unwrap_or([0; 4]));
        if ptype != 0 && lba != 0 && sectors != 0 && ptype != 0xEE && ptype != 0x05 {
            parts.push((u64::from(lba), u64::from(sectors)));
        }
    }
    parts
}

fn lba0_integrity_probe(host: &dyn crate::MmcHost) {
    // Ladder over **repeated single-block reads** — the shape the block
    // consumer actually uses (see `MmcBlkOps::read_block`). A failing rung
    // localizes the first transfer that does not survive repetition, which
    // is what a data-path state leak looks like.
    let mut buf = [0u8; BLOCK_SIZE];
    for blocks in [1usize, 2, 4, 8] {
        for block in 0..blocks {
            if let Err(err) = read_blocks(host, block as u32, 1, &mut buf) {
                log::warn!("mmcblk: LBA{block} read in the {blocks}-block pass FAILED: {err:?}");
                return;
            }
        }
        let head: alloc::vec::Vec<alloc::string::String> =
            buf[..8].iter().map(|b| alloc::format!("{b:02x}")).collect();
        log::info!(
            "mmcblk: {blocks}-block pass ok, LBA{} head: {}",
            blocks - 1,
            head.join(" ")
        );
    }
}

driver_base::declare_driver!(MmcBus, MmcBlkDriver, "mmc-blk", IDS);
