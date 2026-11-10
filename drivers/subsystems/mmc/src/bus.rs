// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The mmc card bus — Linux `drivers/mmc/core/bus.c` (`mmc_bus_type`).
//!
//! Cards are first-class devices: the host driver adds them to the domain
//! from its own probe (`mmc_add_card`), the domain keeps them in the one
//! heterogeneous device table, and consumers bind through this bus's
//! id table. The bus vocabulary is the card's protocol identity — card
//! kind, RCA, geometry — plus the [`MmcHost`] handle every consumer needs
//! to reach the silicon.

use alloc::{boxed::Box, format, string::String, sync::Arc};
use core::any::Any;

use driver_base::bus::{Bus, BusDeviceData, BusId, MatchScore};

use crate::MmcHost;

/// Card protocol family (Linux `MMC_TYPE_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardKind {
    /// SD/SDHC/SDXC memory card.
    Sd,
}

/// One row of a consumer's id table on this bus.
#[derive(Debug, Clone, Copy)]
pub struct MmcIdEntry {
    /// Which card family this consumer drives.
    pub kind: CardKind,
}

impl MmcIdEntry {
    /// A row matching one card kind.
    pub const fn kind(kind: CardKind) -> Self {
        Self { kind }
    }
}

/// The discovered card's protocol identity and its host handle (Linux
/// `struct mmc_card`, the parts consumers need).
pub struct MmcCardData {
    /// Card family.
    pub kind: CardKind,
    /// Host slot index this card was found on (`mmc_host->index`): keys
    /// disk naming (`mmcblk0`) and the DEVPATH component.
    pub slot: u8,
    /// Relative card address assigned at CMD3.
    pub rca: u16,
    /// Card identification register (CMD2, R2), unshifted `{127:96}..{31:0}`.
    pub cid: [u32; 4],
    /// Capacity in 512-byte sectors (from CSD v1/v2).
    pub sectors: u64,
    /// Whether the card uses block (LBA) addressing (SDHC/SDXC, CSD v2+).
    pub block_addressed: bool,
    /// Whether the card accepted the high-speed switch (CMD6, up to 50 MHz).
    pub high_speed: bool,
    /// Whether the card negotiated a 4-bit data bus (ACMD6).
    pub bus_width_4bit: bool,
    /// The slot this card lives on; consumers issue commands through it.
    pub host: Arc<dyn MmcHost>,
}

impl MmcCardData {
    /// Device address for logical block `lba` in the card's addressing
    /// unit (Linux `mmc_card_blockaddr` translation).
    pub fn device_address(&self, lba: u64) -> u32 {
        if self.block_addressed {
            lba as u32
        } else {
            (lba * crate::BLOCK_SIZE as u64) as u32
        }
    }
}

impl core::fmt::Debug for MmcCardData {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The host handle is intentionally opaque (a `dyn MmcHost` has no
        // printable identity); protocol facts carry the diagnostics.
        f.debug_struct("MmcCardData")
            .field("kind", &self.kind)
            .field("slot", &self.slot)
            .field("rca", &self.rca)
            .field("cid", &self.cid)
            .field("sectors", &self.sectors)
            .field("block_addressed", &self.block_addressed)
            .field("high_speed", &self.high_speed)
            .field("bus_width_4bit", &self.bus_width_4bit)
            .finish_non_exhaustive()
    }
}

impl BusDeviceData for MmcCardData {
    fn bus_id(&self) -> BusId {
        MmcBus::ID
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn clone_erased(&self) -> Box<dyn BusDeviceData> {
        Box::new(MmcCardData {
            kind: self.kind,
            slot: self.slot,
            rca: self.rca,
            cid: self.cid,
            sectors: self.sectors,
            block_addressed: self.block_addressed,
            high_speed: self.high_speed,
            bus_width_4bit: self.bus_width_4bit,
            host: self.host.clone(),
        })
    }

    fn path_component(&self) -> String {
        // Linux `mmc_host:x/card_y` collapsed: unique within the bus, tied
        // to the slot index + RCA the host assigned.
        format!("mmc/slot{}:{:04x}", self.slot, self.rca)
    }

    fn modalias(&self) -> Option<String> {
        // Linux uevent MODALIAS for SD cards (`sd.c mmc_sd_uevent` is
        // protocol-specific); the consumer-facing alias is the block one.
        Some(String::from("sd:block"))
    }

    fn display_identity(&self) -> String {
        format!(
            "SD card slot{} rca {:04x}, {} sectors{}",
            self.slot,
            self.rca,
            self.sectors,
            if self.high_speed { ", HS" } else { "" }
        )
    }
}

/// The mmc bus type (Linux `mmc_bus_type`).
pub struct MmcBus;

impl Bus for MmcBus {
    /// Enumeration product: cards added by host drivers.
    type DeviceData = MmcCardData;
    /// Consumer id-table row: which card family to bind.
    type IdEntry = MmcIdEntry;

    const ID: BusId = BusId("mmc");

    fn match_device(data: &Self::DeviceData, entry: &Self::IdEntry) -> MatchScore {
        if data.kind == entry.kind {
            MatchScore::Exact
        } else {
            MatchScore::NoMatch
        }
    }
}
