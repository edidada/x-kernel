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
struct MmcBlkOps {
    card: MmcCardData,
}

impl BlockDeviceOperations for MmcBlkOps {
    fn num_blocks(&self) -> u64 {
        self.card.sectors
    }

    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> KResult {
        let addr = self.card.device_address(block_id);
        let blocks = blocks_in(buf.len()).map_err(|_| KError::Io)?;
        read_blocks(self.card.host.as_ref(), addr, blocks, buf).map_err(|err| err.to_kerror())
    }

    fn write_block(&self, block_id: u64, buf: &[u8]) -> KResult {
        let addr = self.card.device_address(block_id);
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

/// Driver-private data: the publication is the class entry's owner.
pub struct MmcBlkData {
    /// Dropping unpublishes (first field: registry withdrawal happens
    /// before the disk handle goes away).
    _publication: Publication<BlockDeviceImpl>,
    /// Keeps the disk (and through its ops the host/card Arcs) alive for
    /// as long as this Data lives.
    _disk: Arc<Gendisk>,
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

        // Linux names disks `mmcblk<idx>`; one minor, no partition scan
        // in this slice.
        let name: String = alloc::format!("mmcblk{}", card.slot);
        let path: String = alloc::format!("mmc/slot{}:{:04x}", card.slot, card.rca);
        let modalias: String = String::from("sd:block");
        let disk = Arc::new(
            Gendisk::new(name, MMC_BLOCK_MAJOR, 0, 1, Box::new(MmcBlkOps { card }))
                .map_err(|_| ProbeError::Fatal(RuntimeError::Invalid))?
                .with_devnode_context(&path, Some(modalias)),
        );
        let publication = dev
            .publish(
                &BLOCK,
                disk.clone(),
                alloc::format!("mmcblk{}", bus_data.slot),
            )
            .map_err(|_| ProbeError::Fatal(RuntimeError::Io))?;

        log::info!(
            "mmcblk: published {} ({} sectors, {}-bit bus{}{})",
            disk.name(),
            bus_data.sectors,
            if bus_data.bus_width_4bit { 4 } else { 1 },
            if bus_data.high_speed { ", HS" } else { "" },
            if bus_data.block_addressed {
                ""
            } else {
                ", byte-addressed"
            }
        );
        lba0_integrity_probe(bus_data.host.as_ref());

        Ok(MmcBlkData {
            _publication: publication,
            _disk: disk,
        })
    }
}

/// Boot-time data-integrity probe: read block 0 and print its head so a
/// PC-side `dd if=<sd> count=1 | xxd` can be diffed against it — the
/// fastest way to tell a data-path corruption from a simply unformatted
/// card.
fn lba0_integrity_probe(host: &dyn crate::MmcHost) {
    let mut head = [0u8; 32];
    if read_blocks(host, 0, 1, &mut head).is_ok() {
        let hex: alloc::vec::Vec<alloc::string::String> =
            head.iter().map(|b| alloc::format!("{b:02x}")).collect();
        log::info!("mmcblk: LBA0 head: {}", hex.join(" "));
    } else {
        log::warn!("mmcblk: LBA0 integrity probe read failed");
    }
}

driver_base::declare_driver!(MmcBus, MmcBlkDriver, "mmc-blk", IDS);
