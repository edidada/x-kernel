// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Realtek r8169 family (RTL8125) PCIe NIC — probe stage.
//!
//! # What this driver does today
//!
//! It binds through the driver core (id-table match on `10ec:8125`), maps the
//! first MMIO BAR, identifies the chip the way `r8169_main.c` does, reads the
//! station address, then resets the chip and programs its RX/TX configuration.
//! The engines stay stopped (`CmdRxEnb`/`CmdTxEnb` clear), so nothing is
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

/// Register offsets used by this stage (`enum rtl_registers`).
mod reg {
    /// Station address, bytes 0..3 / 4..5 (`MAC0`, `MAC4`).
    pub(super) const MAC0: usize = 0x00;
    pub(super) const MAC4: usize = 0x04;
    /// Command register: reset/rx/tx enable bits.
    pub(super) const CHIP_CMD: usize = 0x37;
    pub(super) const INTR_MASK: usize = 0x3c;
    pub(super) const INTR_STATUS: usize = 0x3e;
    /// Carries the chip version id in bits [29:20]; also the TX burst and
    /// inter-frame-gap configuration.
    pub(super) const TX_CONFIG: usize = 0x40;
    pub(super) const RX_CONFIG: usize = 0x44;
    /// Configuration write-protect latch (`Cfg9346`).
    pub(super) const CFG_9346: usize = 0x50;
}

/// `ChipCmd` bits (`enum rtl_register_content`).
const CMD_RESET: u8 = 0x10;

/// `Cfg9346` values: the RX/TX configuration registers are write-protected
/// unless the latch says otherwise.
const CFG_9346_LOCK: u8 = 0x00;
const CFG_9346_UNLOCK: u8 = 0xc0;

/// `r8169`'s `rtl_set_rx_config_registers` for `RTL_GIGA_MAC_VER_63`
/// (RTL8125B and later): fetch default 8, unlimited DMA burst, pause slot on.
const RX_CONFIG_8125B: u32 = (8 << 27) | (7 << 8) | (1 << 11);

/// `r8169`'s `rtl_set_tx_config_registers`: unlimited DMA burst with the
/// shortest inter-frame gap. RTL8125B is *not* `rtl_is_8168evl_up`, so
/// `TXCFG_AUTO_FIFO` is deliberately not set.
const TX_CONFIG_8125B: u32 = (7 << 8) | (3 << 24);

/// The fields of `RxConfig` software owns: fetch threshold [30:27], pause slot
/// (bit 11), DMA burst [10:8]. Everything else in the register belongs to the
/// chip (and reads back changed).
const RX_CONFIG_WRITABLE: u32 = (0xf << 27) | (1 << 11) | (0x7 << 8);

/// The fields of `TxConfig` software owns: inter-frame gap [25:24] and DMA
/// burst [10:8].
///
/// The gap field is **two** bits wide, not the three a `>> 24` mask suggests:
/// bit 26 upward belongs to the read-only version id, which is exactly why
/// `rtl_chip_infos` compares the id through `0x7cf`/`0xfcf` -- those masks
/// skip bits 24/25 so the gap cannot corrupt the comparison. Writing the whole
/// word and comparing it can never match.
const TX_CONFIG_WRITABLE: u32 = (0x3 << 24) | (0x7 << 8);

/// Reset completion poll: `r8169`'s `rtl_loop_wait_low(..., 100, 100)`.
const RESET_POLL_ITERS: usize = 100;
const RESET_POLL_STEP_US: u64 = 100;

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

/// Puts the chip through `rtl_hw_reset` and programs the RX/TX configuration.
///
/// This is the control-plane half of `rtl8169_hw_reset` +
/// `rtl_set_{rx,tx}_config_registers` for `RTL_GIGA_MAC_VER_63`. It stops
/// short of anything that starts DMA: `CmdRxEnb`/`CmdTxEnb` stay clear, so
/// the engines never fetch from a descriptor ring that does not exist yet.
///
/// The write path is proven by reading the configuration registers back: they
/// are plain storage, so a value that survives the round trip means the chip
/// accepted the write. That is a weaker claim than a working data path, and it
/// is stated as such -- the register-independence trap from the PCIe bring-up
/// (§3.4.4 of the bring-up log) is about *addresses*, not about storage.
fn reset_and_configure(bar0: &IoMap, chip: &str) -> Result<(), ProbeError> {
    // Mask everything before resetting: a pending interrupt with no handler
    // installed is a surprise, not a diagnostic.
    bar0.write16(reg::INTR_MASK, 0);

    bar0.write8(reg::CHIP_CMD, CMD_RESET);
    let mut cleared = false;
    for _ in 0..RESET_POLL_ITERS {
        if bar0.read8(reg::CHIP_CMD) & CMD_RESET == 0 {
            cleared = true;
            break;
        }
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(RESET_POLL_STEP_US));
    }
    if !cleared {
        let cmd = bar0.read8(reg::CHIP_CMD);
        log::error!("rtl8125: {chip} did not finish reset (cmd {cmd:#04x})");
        return Err(ProbeError::Fatal(RuntimeError::Io));
    }

    // RX/TX configuration is write-protected behind the Cfg9346 latch.
    bar0.write8(reg::CFG_9346, CFG_9346_UNLOCK);
    bar0.write32(reg::RX_CONFIG, RX_CONFIG_8125B);
    bar0.write32(reg::TX_CONFIG, TX_CONFIG_8125B);
    bar0.write8(reg::CFG_9346, CFG_9346_LOCK);

    let rx_config = bar0.read32(reg::RX_CONFIG);
    let tx_config = bar0.read32(reg::TX_CONFIG);
    let cmd = bar0.read8(reg::CHIP_CMD);
    log::info!(
        "rtl8125: {chip} reset done, rxcfg {rx_config:#010x} txcfg {tx_config:#010x} cmd \
         {cmd:#04x} (engines still stopped)"
    );
    if rx_config & RX_CONFIG_WRITABLE != RX_CONFIG_8125B & RX_CONFIG_WRITABLE
        || tx_config & TX_CONFIG_WRITABLE != TX_CONFIG_8125B & TX_CONFIG_WRITABLE
    {
        // A field that will not hold its value means the latch or the write
        // path is wrong; do not pretend the chip is ready. Only the fields
        // software owns are compared: `TxConfig`[29:20] is the read-only
        // version id and `RxConfig` carries flow-control state, so neither
        // register ever reads back the word that was written.
        log::error!(
            "rtl8125: {chip} configuration did not stick (rx {:#010x} want {:#010x}, tx {:#010x} \
             want {:#010x})",
            rx_config & RX_CONFIG_WRITABLE,
            RX_CONFIG_8125B & RX_CONFIG_WRITABLE,
            tx_config & TX_CONFIG_WRITABLE,
            TX_CONFIG_8125B & TX_CONFIG_WRITABLE,
        );
        return Err(ProbeError::Fatal(RuntimeError::Io));
    }
    Ok(())
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

        log::info!(
            "rtl8125: {chip} xid {xid:#05x} MAC {mac:02x?} BAR0 {:#x}+{:#x} cmd {:#04x} intr \
             {:#06x} rxcfg {:#010x}",
            region.base,
            region.size,
            bar0.read8(reg::CHIP_CMD),
            bar0.read16(reg::INTR_STATUS),
            bar0.read32(reg::RX_CONFIG),
        );

        reset_and_configure(&bar0, chip)?;

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
