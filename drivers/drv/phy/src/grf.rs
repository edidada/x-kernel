// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! GRF (general register file) access for the rk3588 phy drivers.
//!
//! Every Rockchip phy driver programs windows that are NOT its own reg
//! property: the phy nodes reference them by phandle
//! (`rockchip,pipe-grf = <&pipe_grf>`, `rockchip,usbctrl-grf`, ...).
//! Linux reaches them through regmap/syscon; here the driver resolves the
//! phandle to the GRF node's `reg`, maps it through the device model's
//! iomap door (`Device::res().iomap_region` — one mapped window, devres
//! guard, same as any other MMIO), and does hiword-masked updates.

use alloc::sync::Arc;

use driver_base::{Device, IoMap, ProbeError, RuntimeError};
use of::FdtNode;

/// One mapped GRF window.
pub struct Grf {
    regs: IoMap,
}

impl Grf {
    /// Resolve `prop` (a phandle cell on the device's node) to the target
    /// node's `reg` base and map it.
    ///
    /// `map` is the caller's bound device handle: the GRF is not the
    /// device's own `reg` resource, so the mapping goes through the
    /// explicit-region door.
    pub fn by_property(
        dev: &mut Device<'_, driver_base::PlatformBus>,
        prop: &str,
    ) -> Result<Self, ProbeError> {
        // Bind the view: `node()` borrows the firmware snapshot.
        let fw = dev.fw();
        let firmware = fw.node().ok_or(ProbeError::Fatal(RuntimeError::Invalid))?;
        // A one-cell phandle decodes as FirmwareProp::U32 — u32_list only
        // serves multi-cell payloads.
        let phandle = firmware
            .prop(prop)
            .and_then(|p| {
                p.u32_list()
                    .and_then(|cells| cells.first().copied())
                    .or_else(|| p.as_u32())
            })
            .ok_or_else(|| {
                log::error!("grf: property `{prop}` missing on {}", dev.info().path());
                ProbeError::Fatal(RuntimeError::Invalid)
            })?;

        let node: FdtNode<'static, 'static> =
            of::find_node_by_phandle(phandle).ok_or_else(|| {
                log::error!("grf: phandle {phandle:#x} (`{prop}`) not found in the dt");
                ProbeError::Fatal(RuntimeError::Invalid)
            })?;
        let base = of::node_reg(node).ok_or_else(|| {
            log::error!("grf: node for phandle {phandle:#x} has no reg");
            ProbeError::Fatal(RuntimeError::Invalid)
        })?;
        // GRF blocks are one 64 KiB page in practice; map a page-sized
        // window at the base (the phy drivers touch offsets well inside).
        let region = driver_base::MmioRegion::new(base as usize, 0x10000);
        let regs = dev.res().iomap_region(region, "grf")?;
        Ok(Self { regs })
    }

    /// Read the 32-bit word at `offset`.
    pub fn read(&self, offset: usize) -> u32 {
        self.regs.read32(offset)
    }

    /// Write the 32-bit word at `offset` (raw — GRF banks that are NOT
    /// hiword-conventioned, e.g. usb2phy-grf usable halves).
    pub fn write(&self, offset: usize, value: u32) {
        self.regs.write32(offset, value);
    }

    /// Hiword-masked update (`val << shift | mask << (shift + 16)`) — the
    /// CRU/GRF write convention: upper half is the write-enable mask, so
    /// other fields in the word survive one store.
    pub fn update_hiword(&self, offset: usize, value: u32, mask: u32, shift: u8) {
        let word = ((value & mask) << shift) | (mask << (shift + 16));
        self.regs.write32(offset, word);
    }
}

/// Shared-window adapter: some sequences need to hand a GRF out as a
/// long-lived register accessor (the phy core owns one register
/// abstraction per driver; see the combphy's config writer).
pub type SharedGrf = Arc<Grf>;

/// Map and share a GRF window (the drivers keep these in their priv
/// structs).
pub fn share(grf: Grf) -> SharedGrf {
    Arc::new(grf)
}
