// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Realtek r8169 family (RTL8125) PCIe NIC — probe stage.
//!
//! # What this driver does today
//!
//! It binds through the driver core (id-table match on `10ec:8125`), maps the
//! first MMIO BAR, identifies the chip the way `r8169_main.c` does, reads the
//! station address, and logs the register state it found. Nothing is
//! transmitted, received, or published to the `net` class yet — a NIC that
//! advertises a queue it cannot service is worse than a NIC that does not
//! exist.
//!
//! # Why identify first
//!
//! Every later step (reset, descriptor rings, PHY access, interrupt masks)
//! branches on the MAC version, and the version is read from the chip, not
//! from the PCI device id: `10ec:8125` covers RTL8125A/B/BP/CP/D and the
//! register layouts differ. `r8169` derives it from `TxConfig`:
//!
//! ```text
//! xid = (TxConfig >> 20) & 0xfcf   // rtl_chip_infos[].mask / .val
//! ```
//!
//! The id is only trusted after a `~0` guard: a BAR that decodes nowhere
//! returns all ones, which would otherwise look like a plausible `xid`.
//!
//! # References
//!
//! - `/workspace/linux-7.1.8/drivers/net/ethernet/realtek/r8169_main.c`
//!   (`rtl_chip_infos`, `rtl8169_probe`, `enum rtl_registers`).
//! - Board evidence: BAR assignment and reachability live in
//!   `docs/ai/skills/linux-driver-port/examples/rk3588-pcie/bring-up-log.md`
//!   §3.4.

use driver_base::{Device, DriverOps, IoMap, ProbeError, RuntimeError};
use pci::{PciBus, PciMatch};

/// Register offsets used by the probe (`enum rtl_registers`).
mod reg {
    /// Station address, bytes 0..3 / 4..5 (`MAC0`, `MAC4`).
    pub(super) const MAC0: usize = 0x00;
    pub(super) const MAC4: usize = 0x04;
    /// Command register: reset/rx/tx enable bits.
    pub(super) const CHIP_CMD: usize = 0x37;
    pub(super) const INTR_STATUS: usize = 0x3e;
    /// Carries the chip version id in bits [29:20].
    pub(super) const TX_CONFIG: usize = 0x40;
    pub(super) const RX_CONFIG: usize = 0x44;
}

/// `ChipCmd` bits (`enum rtl_register_content`).
const CMD_RESET: u8 = 0x10;

/// One row of `rtl_chip_infos`: `(mask, val, name)`.
///
/// Only the PCIe 2.5GbE parts are listed: this driver is instantiated for
/// `10ec:8125`, and a table that names parts we cannot bind would lie about
/// coverage. The mask keeps the version's don't-care bits out of the compare,
/// exactly as the reference table does.
const CHIP_INFOS: &[(u32, u32, &str)] = &[
    (0x7cf, 0x609, "RTL8125A"),
    (0x7cf, 0x641, "RTL8125B"),
    (0x7cf, 0x688, "RTL8125D"),
    (0x7cf, 0x689, "RTL8125D"),
    (0x7cf, 0x68a, "RTL8125K"),
    (0x7cf, 0x68b, "RTL9151A"),
    (0x7cf, 0x681, "RTL8125BP"),
    (0x7cf, 0x708, "RTL8125CP"),
];

/// Binds `10ec:8125`. The PCI id says "r8169 family part"; the MAC version
/// read at probe says which one, which is why the id table stays this coarse.
static IDS: &[PciMatch] = &[PciMatch::device(0x10ec, 0x8125)];

pub struct Rtl8125Driver;

/// Driver-private data for one bound NIC.
pub struct Rtl8125Data {
    /// The BAR0 mapping. Held for the binding lifetime: the registers are
    /// re-read by later stages and the mapping costs a page-table entry.
    _bar0: IoMap,
    mac: [u8; 6],
    chip: &'static str,
    xid: u32,
}

impl Rtl8125Data {
    /// The station address read from the chip.
    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    /// The identified part name, e.g. `RTL8125B`.
    pub fn chip(&self) -> &'static str {
        self.chip
    }

    /// The raw version id the part reported.
    pub fn xid(&self) -> u32 {
        self.xid
    }
}

impl DriverOps<PciBus> for Rtl8125Driver {
    type Data = Rtl8125Data;

    fn probe(&self, dev: &mut Device<'_, PciBus>) -> Result<Rtl8125Data, ProbeError> {
        // BAR0 is the register window; BARs 2/3 are the MSI-X table, which
        // this stage does not use.
        let region = dev.res().mmio(0)?;
        let bar0 = dev.res().iomap_region(region, "rtl8125-bar0")?;

        let tx_config = bar0.read32(reg::TX_CONFIG);
        // A window whose decode is dead reads all ones. Check before the
        // version arithmetic turns it into a plausible-looking id.
        if tx_config == u32::MAX {
            log::error!("rtl8125: BAR0 at {:#x} reads all ones", region.base);
            return Err(ProbeError::Fatal(RuntimeError::Io));
        }
        let xid = (tx_config >> 20) & 0xfcf;
        let chip = CHIP_INFOS
            .iter()
            .find(|(mask, val, _)| xid & mask == *val)
            .map(|(_, _, name)| *name)
            .ok_or_else(|| {
                // Report the id we saw: the next bring-up starts from it.
                log::error!("rtl8125: unknown chip, xid {xid:#05x} (TxConfig {tx_config:#010x})");
                ProbeError::Fatal(RuntimeError::Unsupported)
            })?;

        let mac0 = bar0.read32(reg::MAC0);
        let mac4 = bar0.read32(reg::MAC4);
        if mac0 == u32::MAX || mac0 == 0 {
            // The station address is what a consumer will key on; a device
            // that answers with an impossible one is not ready.
            log::error!("rtl8125: {chip} reports no station address (MAC0 {mac0:#010x})");
            return Err(ProbeError::Fatal(RuntimeError::Io));
        }
        let mac = [
            mac0 as u8,
            (mac0 >> 8) as u8,
            (mac0 >> 16) as u8,
            (mac0 >> 24) as u8,
            mac4 as u8,
            (mac4 >> 8) as u8,
        ];

        let chip_cmd = bar0.read8(reg::CHIP_CMD);
        let intr_status = bar0.read16(reg::INTR_STATUS);
        let rx_config = bar0.read32(reg::RX_CONFIG);
        log::info!(
            "rtl8125: {chip} xid {xid:#05x} MAC {mac:02x?} BAR0 {:#x}+{:#x} cmd {chip_cmd:#04x} \
             intr {intr_status:#06x} rxcfg {rx_config:#010x}",
            region.base,
            region.size,
        );
        if chip_cmd & CMD_RESET != 0 {
            // Sticky across a warm boot: a reset left in progress means the
            // previous owner did not finish. Say so instead of proceeding as
            // if the chip were usable.
            log::warn!("rtl8125: {chip} still reports reset in progress (cmd {chip_cmd:#04x})");
        }

        Ok(Rtl8125Data {
            _bar0: bar0,
            mac,
            chip,
            xid,
        })
    }

    fn remove(&self, _dev: &mut Device<'_, PciBus>, _data: &mut Rtl8125Data) {
        // Nothing to unpublished or release: the register mapping follows the
        // Data object.
    }
}

driver_base::declare_driver!(PciBus, Rtl8125Driver, "rtl8125", IDS);
