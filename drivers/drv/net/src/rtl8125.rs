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

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicU64, Ordering};

use driver_base::{CoherentMem, Device, DriverOps, IoMap, ProbeError, Publication, RuntimeError};
use ksync::Mutex;
use pci::{PciBus, PciMatch};
use subsys_net::{KError, MacAddress, NetBuf, NetBufHandle, NetBufPool, NetDevice, NetDeviceImpl};

/// Register offsets used by this stage (`enum rtl_registers`).
mod reg {
    /// Station address, bytes 0..3 / 4..5 (`MAC0`, `MAC4`).
    pub(super) const MAC0: usize = 0x00;
    pub(super) const MAC4: usize = 0x04;
    /// Command register: reset/rx/tx enable bits.
    pub(super) const CHIP_CMD: usize = 0x37;
    /// RTL8125 carries a 32-bit interrupt pair in the *816x `TxPoll`/`IntrMask`
    /// slots: mask at 0x38, status at 0x3c. (The legacy 16-bit 0x3c/0x3e pair
    /// used by the 816x family is not where this chip reports; reading it
    /// always returned zero, which hid the MAC's own status entirely.)
    pub(super) const INTR_MASK: usize = 0x38;
    pub(super) const INTR_STATUS: usize = 0x3c;
    /// Carries the chip version id in bits [29:20]; also the TX burst and
    /// inter-frame-gap configuration.
    pub(super) const TX_CONFIG: usize = 0x40;
    pub(super) const RX_CONFIG: usize = 0x44;
    /// Configuration write-protect latch (`Cfg9346`).
    pub(super) const CFG_9346: usize = 0x50;
    /// MDIO tunnel for the integrated PHY (`GPHY_OCP`).
    pub(super) const GPHY_OCP: usize = 0xb8;
    /// MAC-side OCP tunnel (`OCPDR`): a synchronous register file, unlike the
    /// polled GPHY window.
    pub(super) const OCPDR: usize = 0xb0;
    /// Queue count: 0 selects the single-queue mode these rings use.
    pub(super) const Q_NUM_CTRL_8125: usize = 0x4800;
    /// RSS is multi-queue territory; zero it for one queue.
    pub(super) const RSS_CTRL_8125: usize = 0x4500;
    /// `Config1`: bit 4 gates the "UPS" (power saving) block.
    pub(super) const CONFIG1: usize = 0x52;
    /// Transmit/receive arbitration threshold (undocumented, set by the
    /// reference's 8125 start sequence).
    pub(super) const TX_RX_ARB_8125: usize = 0x382;
    /// Interrupt coalescing configuration block (`INT_CFG0/1_8125`).
    pub(super) const INT_CFG0_8125: usize = 0x34;
    pub(super) const INT_CFG1_8125: usize = 0x7a;
    /// Legacy "MISC" register, whose bit 19 gates the receive data-valid path.
    pub(super) const MISC: usize = 0xf0;
    /// MAC MCU status byte: carries `NOW_IS_OOB` and `LINK_LIST_RDY`.
    pub(super) const MCU: usize = 0xd3;
    /// MAC-side PHY status: bit 1 is the link the *MAC* acts on.
    pub(super) const PHY_STATUS: usize = 0x6c;
    /// `Config2` (clock request), `Config3` (L2/L3 readiness) and `Config5`
    /// (ASPM) -- the PCIe power-management knobs.
    pub(super) const CONFIG2: usize = 0x53;
    pub(super) const CONFIG3: usize = 0x54;
    pub(super) const CONFIG5: usize = 0x56;
    /// Descriptor ring base addresses (the reference writes the high word
    /// first: some boards only latch the low word after it).
    pub(super) const TX_DESC_ADDR_LOW: usize = 0x20;
    pub(super) const TX_DESC_ADDR_HIGH: usize = 0x24;
    pub(super) const RX_DESC_ADDR_LOW: usize = 0xe4;
    pub(super) const RX_DESC_ADDR_HIGH: usize = 0xe8;
    /// Largest frame the receiver accepts; low values silently drop.
    pub(super) const RX_MAX_SIZE: usize = 0xda;
    /// Kick the transmit queue. RTL8125 replaced the legacy `TxPoll` byte
    /// (`NPQ`) with a 16-bit doorbell register where bit 0 is the poll.
    pub(super) const TX_POLL_8125: usize = 0x90;
}

/// OCP transaction flag: set while a request is in flight (and, when written,
/// marks the word as a write).
const OCP_FLAG: u32 = 0x8000_0000;

/// MDIO register address of the integrated PHY, in OCP space:
/// `OCP_STD_PHY_BASE` (0xa400) + MDIO register * 2.
const OCP_STD_PHY_BASE: u32 = 0xa400;

/// MDIO register numbers used here (`include/uapi/linux/mii.h`).
const MII_BMCR: u16 = 0x00;
const MII_BMSR: u16 = 0x01;
const MII_PHYSID1: u16 = 0x02;
const MII_PHYSID2: u16 = 0x03;

/// `BMSR_LSTATUS`: the PHY currently has a link.
const BMSR_LSTATUS: u16 = 0x0004;

/// OCP poll budget, `r8169`'s `rtl_loop_wait_{low,high}(..., 25, 10)`.
const OCP_POLL_ITERS: usize = 25;
const OCP_POLL_STEP_US: u64 = 10;

/// One descriptor: `opts1`, `opts2`, and a 64-bit buffer address.
const DESC_LEN: usize = 16;
/// Descriptor ownership bit: set by software to hand a descriptor to the NIC,
/// cleared by the NIC when it is done with it.
const DESC_OWN: u32 = 1 << 31;
/// Last descriptor of the ring (`RingEnd`).
const DESC_RING_END: u32 = 1 << 30;
/// Packet boundaries for a one-buffer frame.
const DESC_FIRST_FRAG: u32 = 1 << 29;
const DESC_LAST_FRAG: u32 = 1 << 28;
/// Receive length lives in the low 14 bits of `opts1`.
const RX_LEN_MASK: u32 = 0x3fff;
/// `RxMaxSize` is written as "largest frame + 1" (the reference comments that
/// a low value only hurts: oversized frames are dropped instead of received).
const RX_MAX_SIZE_FIELD: u16 = (RX_SLOT_LEN + 1) as u16;

/// Ring depths. 256 is what the reference driver uses, and a power of two
/// makes the wrap arithmetic a mask.
const TX_DESC_COUNT: usize = 256;
const RX_DESC_COUNT: usize = 256;
/// Per-slot packet buffers. One slot holds one whole Ethernet frame (1518 max
/// with VLAN), so no fragmentation machinery is needed.
const TX_SLOT_LEN: usize = 2048;
const RX_SLOT_LEN: usize = 2048;

/// `ChipCmd` enables.
const CMD_RX_ENB: u8 = 0x08;
const CMD_TX_ENB: u8 = 0x04;
/// `TxPoll_8125` bit 0: "poll the transmit queue now".
const TX_POLL_8125_BIT: u16 = 1 << 0;

/// Transmit completion poll budget used by the bring-up trace (1 ms).
const TX_POLL_ITERS: usize = 100;
const TX_POLL_STEP_US: u64 = 10;
/// Counts traced transmits so the log stays bounded.
static TX_TRACE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
/// `RxConfig` accept bits: broadcast, multicast, and frames addressed to our
/// own station address (`rx_mode_bits` in the reference).
const RX_ACCEPT_MODE: u32 = 0x08 | 0x04 | 0x02;

const OCPDR_WRITE_CMD: u32 = 0x8000_0000;

/// `Config3` bit 1: allow the link to enter L2/L3.
const CFG3_RDY_TO_L23: u8 = 1 << 1;
/// `Config2` bit 7: clock request enable.
const CFG2_CLKREQ_EN: u8 = 1 << 7;
/// `Config5` bit 0: ASPM enable.
const CFG5_ASPM_EN: u8 = 1 << 0;
/// Value the reference writes to `0x382` for RTL8125.
const TX_RX_ARB_8125_VALUE: u16 = 0x221b;

/// OCP register that selects the transmit descriptor format. The 8125 powers up
/// with the **new** format; the rings here are the legacy 16-byte layout every
/// r8169 generation uses, so the bit is cleared in the start sequence
/// (`rtl_hw_start_8125_common`: "disable new tx descriptor format").
const OCP_TX_DESC_FORMAT: u16 = 0xeb58;
/// Spare MAC-OCP register used to prove the tunnel reads back what it wrote.
const OCP_SELF_TEST: u16 = 0xea84;
const OCP_SELF_TEST_BITS: u16 = 0x0003;

/// `MISC` bit 19: receive data-valid gate (must be off).
const RXDV_GATED_EN: u32 = 1 << 19;
/// `MCU` bit 7: "now is out of band", cleared during MAC initialization.
const MCU_NOW_IS_OOB: u8 = 1 << 7;
/// `MCU` bit 1: the MAC's link-list (descriptor) FIFO is ready for sharing.
const MCU_LINK_LIST_RDY: u8 = 1 << 1;
/// `r8168g_wait_ll_share_fifo_ready`: 100 x 42 us.
const LL_FIFO_READY_ITERS: usize = 100;
const LL_FIFO_READY_STEP_US: u64 = 42;
/// `rtl_loop_wait_low(rtl_mac_ocp_e00e_cond, 1000, 10)`.
const MAC_OCP_SETTLE_ITERS: usize = 1000;

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
///
/// The publication is the teardown guard: dropping it withdraws the device
/// from the `net` class, and with it the boxed NIC that owns the rings.
pub struct Rtl8125Data {
    _publication: Publication<NetDeviceImpl>,
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

/// Programs the descriptor rings and starts the engines.
///
/// Order follows the reference's `rtl_hw_start`: ring base addresses and
/// `RxMaxSize` first, then the configuration registers (still behind the
/// `Cfg9346` latch), and only then `CmdRxEnb|CmdTxEnb`. A ring the MAC has not
/// been told about yet can never be fetched, and a MAC started before the
/// accept mode is set would drop the frames that arrive first.
fn start_data_path(nic: &Rtl8125Nic) -> Result<(), ProbeError> {
    let bar0 = &nic.bar0;

    program_ring_bases(nic);
    arm_receive_ring(nic);
    program_engine_config(nic);

    let cmd = bar0.read8(reg::CHIP_CMD);
    if cmd & (CMD_RX_ENB | CMD_TX_ENB) != CMD_RX_ENB | CMD_TX_ENB {
        log::error!("rtl8125: engines did not start (ChipCmd {cmd:#04x})");
        return Err(ProbeError::Fatal(RuntimeError::Io));
    }
    dump_registers(bar0);

    log::info!(
        "rtl8125: rings live, tx {TX_DESC_COUNT}x{TX_SLOT_LEN}B @ {:#x}, rx \
         {RX_DESC_COUNT}x{RX_SLOT_LEN}B @ {:#x}",
        nic.tx_ring.bus_addr(),
        nic.rx_ring.bus_addr(),
    );
    Ok(())
}

/// Programs the descriptor ring base addresses and the receive size limit.
fn program_ring_bases(nic: &Rtl8125Nic) {
    let bar0 = &nic.bar0;
    bar0.write8(reg::CFG_9346, CFG_9346_UNLOCK);
    // ldp:step start.rx-max-size
    bar0.write16(reg::RX_MAX_SIZE, RX_MAX_SIZE_FIELD);
    // ldp:step start.desc-registers
    // High word first: the reference does it for boards that only latch the
    // low word after it has seen the high one.
    bar0.write32(
        reg::TX_DESC_ADDR_HIGH,
        (nic.tx_ring.bus_addr() >> 32) as u32,
    );
    bar0.write32(
        reg::TX_DESC_ADDR_LOW,
        (nic.tx_ring.bus_addr() & u64::from(u32::MAX)) as u32,
    );
    bar0.write32(
        reg::RX_DESC_ADDR_HIGH,
        (nic.rx_ring.bus_addr() >> 32) as u32,
    );
    bar0.write32(
        reg::RX_DESC_ADDR_LOW,
        (nic.rx_ring.bus_addr() & u64::from(u32::MAX)) as u32,
    );
    bar0.write8(reg::CFG_9346, CFG_9346_LOCK);
    log::info!(
        "rtl8125: ring base tx {:#010x}:{:#010x} rx {:#010x}:{:#010x} maxsize {:#06x}",
        bar0.read32(reg::TX_DESC_ADDR_HIGH),
        bar0.read32(reg::TX_DESC_ADDR_LOW),
        bar0.read32(reg::RX_DESC_ADDR_HIGH),
        bar0.read32(reg::RX_DESC_ADDR_LOW),
        bar0.read16(reg::RX_MAX_SIZE),
    );
}

/// Hands every receive descriptor to the NIC before the receiver starts.
fn arm_receive_ring(nic: &Rtl8125Nic) {
    for index in 0..RX_DESC_COUNT {
        nic.arm_rx(index);
    }
    Rtl8125Nic::barrier_release();
}

/// Programs the RX/TX configuration and starts both engines.
fn program_engine_config(nic: &Rtl8125Nic) {
    let bar0 = &nic.bar0;

    // The reference's `rtl_hw_start` tail, in order: commit the pending
    // configuration writes with a `ChipCmd` read, *then* enable the engines,
    // and only then program the RX/TX configuration and the receive mode.
    let _ = bar0.read8(reg::CHIP_CMD); // ldp:step start.pci-commit
    bar0.write32(reg::INTR_MASK, 0xffff_ffff);
    // ldp:step start.enable-engines
    bar0.write8(reg::CHIP_CMD, CMD_RX_ENB | CMD_TX_ENB);

    bar0.write8(reg::CFG_9346, CFG_9346_UNLOCK);
    // ldp:steps start.rx-config, start.tx-config, start.rx-mode
    bar0.write32(reg::RX_CONFIG, RX_CONFIG_8125B | RX_ACCEPT_MODE);
    bar0.write32(reg::TX_CONFIG, TX_CONFIG_8125B);
    bar0.write8(reg::CFG_9346, CFG_9346_LOCK);
    log::info!(
        "rtl8125: engines on, rxcfg {:#010x} txcfg {:#010x} cmd {:#04x}",
        bar0.read32(reg::RX_CONFIG),
        bar0.read32(reg::TX_CONFIG),
        bar0.read8(reg::CHIP_CMD),
    );
}

/// Logs every non-zero word of the register block, once, at start.
///
/// This is the cross-board comparison anchor: the reference board runs the
/// vendor driver on the same SoC and chip, so a word that differs between the
/// two dumps is a candidate difference in behaviour.
fn dump_registers(bar0: &IoMap) {
    let mut line = alloc::string::String::new();
    let mut off = 0usize;
    while off < 0xf4 {
        let value = bar0.read32(off);
        if value != 0 {
            line.push_str(&alloc::format!(" {off:03x}={value:08x}"));
        }
        off += 4;
    }
    line.push_str(&alloc::format!(" 382={:04x}", bar0.read16(0x382)));
    line.push_str(&alloc::format!(" 1880={:04x}", bar0.read16(0x1880)));
    line.push_str(&alloc::format!(" 4500={:08x}", bar0.read32(0x4500)));
    line.push_str(&alloc::format!(" 4800={:04x}", bar0.read16(0x4800)));
    log::info!("rtl8125: regdump{line}");
}

/// The "common" half of the reference's `rtl_hw_start_8125_common`.
///
/// Transcribed register by register, including the undocumented MAC-OCP rows:
/// this is the chip vendor's own analog/front-end configuration, and skipping
/// it left the MAC enabling cleanly (`ChipCmd` = both engines) while never
/// fetching a single descriptor. Each row is a read-modify-write with its own
/// mask, exactly as `r8168_mac_ocp_modify` performs it.
fn hw_start_8125_common(bar0: &IoMap) {
    // Interrupt coalescing off; the receive path here is polled.
    // ldp:step start.intcfg0
    bar0.write8(reg::INT_CFG0_8125, 0x00);
    let mut offset = 0xa00usize;
    while offset < 0xa80 {
        bar0.write32(offset, 0);
        offset += 4;
    }
    bar0.write16(reg::INT_CFG1_8125, 0x0000);

    // One queue, no RSS.
    bar0.write16(reg::TX_RX_ARB_8125, TX_RX_ARB_8125_VALUE);
    bar0.write32(reg::RSS_CTRL_8125, 0);
    bar0.write16(reg::Q_NUM_CTRL_8125, 0);

    // ldp:step start.hw-start-common
    // (mask, set) rows from the reference. `set` is masked with `mask` before
    // it is OR'd in, so a clear row is "(mask, 0)".
    const OCP_ROWS: &[(u16, u16, u16)] = &[
        (0xd40a, 0x0010, 0x0000), // disable UPS
        (0xc140, 0xffff, 0xffff),
        (0xc142, 0xffff, 0xffff),
        (0xd3e2, 0x0fff, 0x03a9),
        (0xd3e4, 0x00ff, 0x0000),
        (0xe860, 0x0000, 0x0080),
        (0xeb58, 0x0001, 0x0000), // legacy 16-byte TX descriptors
        (0xe614, 0x0700, 0x0200), // RTL8125B row
        (0xe63e, 0x0c30, 0x0000), // RTL8125B row
        (0xc0b4, 0x0000, 0x000c),
        (0xeb6a, 0x00ff, 0x0033),
        (0xeb50, 0x03e0, 0x0040),
        (0xe056, 0x00f0, 0x0000),
        (0xe040, 0x1000, 0x0000),
        (0xea1c, 0x0003, 0x0001),
        (0xea1c, 0x0004, 0x0000), // RTL8125B row
        (0xe0c0, 0x4f0f, 0x4403),
        (0xe052, 0x0080, 0x0068),
        (0xd430, 0x0fff, 0x047f),
        (0xea1c, 0x0004, 0x0000),
        (0xeb54, 0x0001, 0x0000),
    ];
    for (reg_addr, mask, set) in OCP_ROWS {
        mac_ocp_modify(bar0, *reg_addr, *mask, *set);
    }
    // `rtl_loop_wait_low(rtl_mac_ocp_e00e_cond, 1000, 10)`: wait for the MAC
    // OCP block to settle before touching the receive path.
    for _ in 0..MAC_OCP_SETTLE_ITERS {
        if mac_ocp_read(bar0, 0xe00e) & (1 << 13) == 0 {
            break;
        }
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(OCP_POLL_STEP_US));
    }

    bar0.write16(0x1880, bar0.read16(0x1880) & !0x0030);
    mac_ocp_write(bar0, 0xe098, 0xc302);

    // Ungate the receive data-valid path (`rtl_disable_rxdvgate`).
    bar0.write32(reg::MISC, bar0.read32(reg::MISC) & !RXDV_GATED_EN);
    log::info!(
        "rtl8125: 8125 common start applied, misc {:#010x} qnum {:#06x}",
        bar0.read32(reg::MISC),
        bar0.read16(reg::Q_NUM_CTRL_8125),
    );
}

/// Allocates the coherent rings and the per-descriptor packet slots.
///
/// One allocation per ring keeps the descriptor table contiguous, which the
/// device requires: it is handed exactly one base address per ring.
/// Allocates a coherent buffer whose bus address is above `min_bus`.
///
/// The MAC reports `TX_DESC_UNAVAILABLE` while every descriptor it is pointed
/// at has the ownership bit set: that is the signature of the device reading a
/// different physical memory than software wrote. Walking the allocations
/// upwards makes the ring land where the working reference board's DMA does
/// (its rings are at 0xe4010000), which is the cheapest way to find out whether
/// the low DRAM band is reachable for device reads at all.
fn alloc_above(
    dev: &mut Device<'_, PciBus>,
    len: usize,
    align: usize,
    min_bus: u64,
    keep: &mut alloc::vec::Vec<CoherentMem>,
) -> Result<CoherentMem, ProbeError> {
    const CHUNK: usize = 16 << 20;
    const MAX_TRIES: usize = 128;
    let mut best = dev.res().alloc_coherent(CHUNK.max(len), align)?;
    let mut tries = 0;
    while best.bus_addr() < min_bus && tries < MAX_TRIES {
        keep.push(best);
        best = dev.res().alloc_coherent(CHUNK.max(len), align)?;
        tries += 1;
    }
    log::info!(
        "rtl8125: dma at {:#x} after {tries} bumps (target {min_bus:#x})",
        best.bus_addr()
    );
    Ok(best)
}

fn allocate_rings(
    dev: &mut Device<'_, PciBus>,
) -> Result<(CoherentMem, CoherentMem, CoherentMem, CoherentMem), ProbeError> {
    const DMA_FLOOR: u64 = 0xc000_0000;
    let mut keep = alloc::vec::Vec::new();
    let tx_ring = alloc_above(
        dev,
        TX_DESC_COUNT * DESC_LEN,
        DESC_LEN,
        DMA_FLOOR,
        &mut keep,
    )?;
    let rx_ring = alloc_above(
        dev,
        RX_DESC_COUNT * DESC_LEN,
        DESC_LEN,
        DMA_FLOOR,
        &mut keep,
    )?;
    let tx_slots = alloc_above(dev, TX_DESC_COUNT * TX_SLOT_LEN, 64, DMA_FLOOR, &mut keep)?;
    let rx_slots = alloc_above(dev, RX_DESC_COUNT * RX_SLOT_LEN, 64, DMA_FLOOR, &mut keep)?;
    // Kept alive: dropping a bump hands the page back and a later allocation
    // can land below the floor again.
    core::mem::forget(keep);
    Ok((tx_ring, rx_ring, tx_slots, rx_slots))
}

/// Waits for the MAC's link-list FIFO to report ready (`MCU & LINK_LIST_RDY`).
///
/// This FIFO is what the transmit and receive engines fetch descriptors
/// through. Until it reports ready, `ChipCmd` can be enabled and every
/// configuration register can read back correctly while the MAC never touches
/// a descriptor at all.
fn wait_link_list_ready(bar0: &IoMap) -> bool {
    for _ in 0..LL_FIFO_READY_ITERS {
        if bar0.read8(reg::MCU) & MCU_LINK_LIST_RDY != 0 {
            return true;
        }
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(LL_FIFO_READY_STEP_US));
    }
    false
}

/// `rtl_hw_init_8125`: brings the MAC's internal MCU/link-list block up.
///
/// The reference driver runs this **before** `rtl_hw_reset`, and it is the step
/// this driver was missing: it gates the receive data-valid path, parks both
/// engines, clears `NOW_IS_OOB`, then waits for the link-list FIFO to become
/// shareable around three MAC-OCP writes.
fn hw_init_8125(bar0: &IoMap) -> Result<(), ProbeError> {
    // Gate RX data-valid and park the engines for the initialization window.
    // ldp:steps init.rxdv-gate, init.stop-engines, init.sleep-1ms
    bar0.write32(reg::MISC, bar0.read32(reg::MISC) | RXDV_GATED_EN);
    bar0.write8(
        reg::CHIP_CMD,
        bar0.read8(reg::CHIP_CMD) & !(CMD_RX_ENB | CMD_TX_ENB),
    );
    khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1000));

    // ldp:steps init.mcu-clear-oob, init.ocp-e8de, init.ll-fifo-wait
    bar0.write8(reg::MCU, bar0.read8(reg::MCU) & !MCU_NOW_IS_OOB);
    mac_ocp_modify(bar0, 0xe8de, 1 << 14, 0);
    let ready_before = wait_link_list_ready(bar0);

    // ldp:steps init.ocp-c0aa, init.ocp-c0a6, init.ocp-c01e
    mac_ocp_write(bar0, 0xc0aa, 0x07d0);
    mac_ocp_write(bar0, 0xc0a6, 0x0150);
    mac_ocp_write(bar0, 0xc01e, 0x5555);
    let ready_after = wait_link_list_ready(bar0);

    log::info!(
        "rtl8125: mac init done, link-list fifo ready {ready_before}/{ready_after} (mcu {:#04x})",
        bar0.read8(reg::MCU),
    );
    if !ready_before && !ready_after {
        // Without the FIFO there is no descriptor path; fail loudly rather
        // than publish a NIC that cannot move a frame.
        return Err(ProbeError::Fatal(RuntimeError::Io));
    }
    Ok(())
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
    // installed is a surprise, not a diagnostic. The pending status is cleared
    // too -- a stale latched fatal interrupt from the previous stage can leave
    // the MAC's engines parked even though the command register reads enabled.
    bar0.write32(reg::INTR_MASK, 0);
    bar0.write32(reg::INTR_STATUS, 0xffff_ffff);
    // MAC MCU / link-list block first, in the reference's order
    // (`rtl_hw_initialize` runs before `rtl_hw_reset`).
    hw_init_8125(bar0)?;

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

    // Drop the power-saving "UPS" gate before the engines run.
    let config1 = bar0.read8(reg::CONFIG1);
    bar0.write8(reg::CONFIG1, config1 & !0x10);
    // Prove the MAC-OCP tunnel reads back what it writes before trusting any
    // value obtained through it (a read that always returns 0 would make the
    // descriptor-format bit indistinguishable from a dead tunnel).
    let probe_reg = mac_ocp_read(bar0, OCP_SELF_TEST);
    mac_ocp_write(bar0, OCP_SELF_TEST, probe_reg | OCP_SELF_TEST_BITS);
    let probe_rb = mac_ocp_read(bar0, OCP_SELF_TEST);
    mac_ocp_write(bar0, OCP_SELF_TEST, probe_reg);
    log::info!("rtl8125: mac ocp self-test {probe_reg:#06x} -> {probe_rb:#06x}");

    // PCIe power management off before anything else: RTL8125 is known for
    // DMA stalls when the link idles in a low-power state (the reference
    // disables ASPM and clock requests around its start sequence).
    disable_pcie_low_power(bar0);
    // The chip's own front-end configuration, in the reference's order.
    hw_start_8125_common(bar0);
    log::info!(
        "rtl8125: {chip} tx descriptor format {:#06x} (legacy 16-byte when bit0 is clear)",
        mac_ocp_read(bar0, OCP_TX_DESC_FORMAT),
    );
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

/// Turns off every PCIe low-power entry this chip offers.
///
/// RTL8125 is known for DMA stalls caused by ASPM L1 (the reference driver
/// disables it unconditionally in `rtl8169_init_one` and again around
/// `rtl_hw_start`). The three bits are independent, so all three go off.
fn disable_pcie_low_power(bar0: &IoMap) {
    let config3 = bar0.read8(reg::CONFIG3);
    bar0.write8(reg::CONFIG3, config3 & !CFG3_RDY_TO_L23);
    let config2 = bar0.read8(reg::CONFIG2);
    bar0.write8(reg::CONFIG2, config2 & !CFG2_CLKREQ_EN);
    let config5 = bar0.read8(reg::CONFIG5);
    bar0.write8(reg::CONFIG5, config5 & !CFG5_ASPM_EN);
    log::info!(
        "rtl8125: pcie low power off, cfg3 {config3:#04x} cfg2 {config2:#04x} cfg5 {config5:#04x} \
         -> {:#04x}/{:#04x}/{:#04x}",
        bar0.read8(reg::CONFIG3),
        bar0.read8(reg::CONFIG2),
        bar0.read8(reg::CONFIG5),
    );
}

/// Reads a MAC-side OCP register.
///
/// The MAC window is synchronous: program the address and read the data back
/// from the same register, no flag to poll (`__r8168_mac_ocp_read`).
fn mac_ocp_read(bar0: &IoMap, reg: u16) -> u16 {
    bar0.write32(reg::OCPDR, u32::from(reg) << 15);
    (bar0.read32(reg::OCPDR) & 0xffff) as u16
}

/// Writes a MAC-side OCP register.
fn mac_ocp_write(bar0: &IoMap, reg: u16, data: u16) {
    bar0.write32(
        reg::OCPDR,
        OCPDR_WRITE_CMD | (u32::from(reg) << 15) | u32::from(data),
    );
}

/// `r8168_mac_ocp_modify`: replace the bits in `mask` with `set`.
fn mac_ocp_modify(bar0: &IoMap, reg: u16, mask: u16, set: u16) {
    let value = mac_ocp_read(bar0, reg);
    mac_ocp_write(bar0, reg, (value & !mask) | (set & mask));
}

/// Reads one MDIO register through the `GPHY_OCP` tunnel.
///
/// RTL8125 has no legacy `PHYAR` register: the integrated PHY is reached by
/// programming an OCP address of `0xa400 + mdio_reg * 2` and waiting for the
/// flag bit to drop (`r8168_phy_ocp_read`).
fn mii_read(bar0: &IoMap, mdio_reg: u16) -> Option<u16> {
    let ocp = OCP_STD_PHY_BASE + u32::from(mdio_reg) * 2;
    bar0.write32(reg::GPHY_OCP, ocp << 15);
    for _ in 0..OCP_POLL_ITERS {
        let word = bar0.read32(reg::GPHY_OCP);
        if word & OCP_FLAG != 0 {
            return Some((word & 0xffff) as u16);
        }
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(OCP_POLL_STEP_US));
    }
    None
}

/// Current link state, from the PHY's status register.
///
/// `BMSR_LSTATUS` is **latched low**: a link that dropped since the last read
/// still reports up once, so it has to be read twice and the second read is
/// the current state (IEEE 802.3 clause 22 requirement, and the reason
/// `mii_read` is called twice here rather than once).
fn link_is_up(bar0: &IoMap) -> bool {
    let _ = mii_read(bar0, MII_BMSR);
    mii_read(bar0, MII_BMSR).is_some_and(|bmsr| bmsr & BMSR_LSTATUS != 0)
}

/// Ring cursor state; every field is touched from task context only.
struct RingState {
    /// Next transmit descriptor to fill.
    tx_next: usize,
    /// Next transmit descriptor to reclaim.
    tx_clean: usize,
    /// Next receive descriptor to inspect.
    rx_next: usize,
}

/// The live NIC: registers, coherent rings, and the `net` class contract.
///
/// Buffers are copied in both directions (frame -> slot on transmit, slot ->
/// frame on receive) rather than handed to the stack by reference. That keeps
/// descriptor ownership a single-writer invariant instead of a shared one, and
/// the copy is the cheap part next to the bus transaction.
struct Rtl8125Nic {
    bar0: IoMap,
    mac: [u8; 6],
    /// Ring memory: descriptors and the per-descriptor packet slots. The bus
    /// addresses the device sees come from the same allocations.
    tx_ring: CoherentMem,
    rx_ring: CoherentMem,
    tx_slots: CoherentMem,
    rx_slots: CoherentMem,
    pool: Arc<NetBufPool>,
    state: Mutex<RingState>,
    tx_completed: AtomicU64,
    rx_frames: AtomicU64,
}

impl Rtl8125Nic {
    fn tx_desc(&self, index: usize) -> *mut u8 {
        // SAFETY: `index < TX_DESC_COUNT` and the ring allocation is
        // `TX_DESC_COUNT * DESC_LEN` bytes, so the offset stays in bounds.
        unsafe { self.tx_ring.as_ptr().add(index * DESC_LEN) }
    }

    fn rx_desc(&self, index: usize) -> *mut u8 {
        // SAFETY: as `tx_desc`, for the receive ring.
        unsafe { self.rx_ring.as_ptr().add(index * DESC_LEN) }
    }

    fn tx_slot(&self, index: usize) -> *mut u8 {
        // SAFETY: `index < TX_DESC_COUNT` and the slot allocation is
        // `TX_DESC_COUNT * TX_SLOT_LEN` bytes.
        unsafe { self.tx_slots.as_ptr().add(index * TX_SLOT_LEN) }
    }

    fn rx_slot(&self, index: usize) -> *mut u8 {
        // SAFETY: as `tx_slot`, for the receive slots.
        unsafe { self.rx_slots.as_ptr().add(index * RX_SLOT_LEN) }
    }

    /// Reads `opts1` of a descriptor.
    fn desc_opts1(desc: *mut u8) -> u32 {
        // SAFETY: `desc` points at the start of a descriptor owned by one of
        // this device's coherent rings, and MMIO/DMA memory is only ever
        // touched through volatile accesses.
        unsafe { core::ptr::read_volatile(desc.cast::<u32>()) }
    }

    /// Writes `opts1` (ownership/status word) of a descriptor.
    fn desc_set_opts1(desc: *mut u8, opts1: u32) {
        // SAFETY: as `desc_opts1`; the write is the ownership handover.
        unsafe { core::ptr::write_volatile(desc.cast::<u32>(), opts1) }
    }

    /// Writes `opts2` (VLAN/TSO options) of a descriptor.
    fn desc_set_opts2(desc: *mut u8, opts2: u32) {
        // SAFETY: `opts2` is at offset 4 of a 16-byte descriptor.
        let field = unsafe { desc.add(4) }.cast::<u32>();
        // SAFETY: `field` is the descriptor's own options word.
        unsafe { core::ptr::write_volatile(field, opts2) }
    }

    /// Writes the buffer address of a descriptor.
    fn desc_set_addr(desc: *mut u8, addr: u64) {
        // SAFETY: the address field is at offset 8 of a 16-byte descriptor, so
        // the 8-byte store stays inside the descriptor.
        let field = unsafe { desc.add(8) }.cast::<u64>();
        // SAFETY: `field` is the descriptor's own 8-byte address slot, and
        // descriptor memory is only touched through volatile accesses.
        unsafe { core::ptr::write_volatile(field, addr) }
    }

    /// Hands an RX descriptor to the NIC: fresh ownership flag and length.
    fn arm_rx(&self, index: usize) {
        let desc = self.rx_desc(index);
        let ring_end = if index == RX_DESC_COUNT - 1 {
            DESC_RING_END
        } else {
            0
        };
        // The buffer address is set once at init and never moves.
        Self::desc_set_opts1(desc, DESC_OWN | ring_end | RX_SLOT_LEN as u32);
    }

    /// Publishes a store to the descriptor ring *before* the NIC reads it.
    fn barrier_release() {
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
    }

    /// Orders a descriptor read *before* the driver reads the data it points
    /// at (the NIC wrote both, and only `opts1` is polled).
    fn barrier_acquire() {
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
    }
}

impl NetDevice for Rtl8125Nic {
    fn mac(&self) -> MacAddress {
        MacAddress(self.mac)
    }

    fn can_tx(&self) -> bool {
        self.bar0.read32(reg::TX_CONFIG) != u32::MAX
    }

    fn can_rx(&self) -> bool {
        let state = self.state.lock();
        let mut index = state.rx_next;
        for _ in 0..RX_DESC_COUNT {
            if Self::desc_opts1(self.rx_desc(index)) & DESC_OWN == 0 {
                return true;
            }
            index = (index + 1) % RX_DESC_COUNT;
        }
        false
    }

    fn rx_queue_len(&self) -> usize {
        self.rx_frames.load(Ordering::Relaxed) as usize
    }

    fn tx_queue_len(&self) -> usize {
        let state = self.state.lock();
        // Filled-but-not-reclaimed descriptors are the outstanding queue.
        let used = state.tx_next + TX_DESC_COUNT - state.tx_clean;
        used % TX_DESC_COUNT
    }

    fn alloc_tx_buf(&self, size: usize) -> Result<NetBufHandle, KError> {
        if size > TX_SLOT_LEN {
            return Err(KError::InvalidInput);
        }
        let mut buf = self.pool.alloc_boxed().ok_or(KError::NoMemory)?;
        buf.set_payload_len(size)?;
        Ok(buf.into_handle())
    }

    fn send(&self, tx_buf: NetBufHandle) -> Result<(), KError> {
        let frame_len = tx_buf.len();
        if frame_len == 0 || frame_len > TX_SLOT_LEN {
            // SAFETY: the handle came from `alloc_tx_buf`, which builds it with
            // `NetBuf::into_handle`; this is its single consumption.
            drop(unsafe { NetBuf::from_handle(tx_buf) });
            return Err(KError::InvalidInput);
        }
        let mut state = self.state.lock();
        let index = state.tx_next;
        let next = (index + 1) % TX_DESC_COUNT;
        if next == state.tx_clean {
            // The ring is full: the caller recycles and retries.
            drop(state);
            // SAFETY: as above, the handle is consumed exactly once here.
            drop(unsafe { NetBuf::from_handle(tx_buf) });
            return Err(KError::ResourceBusy);
        }

        let desc = self.tx_desc(index);
        // The slot must not be written while the NIC still owns the previous
        // frame that used it.
        if Self::desc_opts1(desc) & DESC_OWN != 0 {
            drop(state);
            // SAFETY: as above, the handle is consumed exactly once here.
            drop(unsafe { NetBuf::from_handle(tx_buf) });
            return Err(KError::ResourceBusy);
        }

        // SAFETY: `index < TX_DESC_COUNT`, the destination slot is
        // `TX_SLOT_LEN` bytes, and `frame_len <= TX_SLOT_LEN` was checked.
        unsafe {
            core::ptr::copy_nonoverlapping(tx_buf.data().as_ptr(), self.tx_slot(index), frame_len);
        }
        // SAFETY: the frame bytes were copied out, so this is the handle's
        // single consumption and it returns the buffer to the pool.
        drop(unsafe { NetBuf::from_handle(tx_buf) });

        let ring_end = if index == TX_DESC_COUNT - 1 {
            DESC_RING_END
        } else {
            0
        };
        Self::barrier_release();
        Self::desc_set_opts1(
            desc,
            DESC_OWN | DESC_FIRST_FRAG | DESC_LAST_FRAG | ring_end | frame_len as u32,
        );
        state.tx_next = next;
        drop(state);

        // The 8125 doorbell, not the legacy NPQ byte: writing `NPQ` into 0x38
        // on this part does nothing, and a queued descriptor then sits there
        // forever (measured: the frame never reached the wire).
        self.bar0.write16(reg::TX_POLL_8125, TX_POLL_8125_BIT);

        // Bounded transmit evidence: the first few frames report whether the
        // MAC actually consumed the descriptor. A descriptor still owned by
        // the NIC after the poll budget means the kick did not take (wrong
        // doorbell, ring not armed, MAC stopped) -- never "eventually fine".
        if TX_TRACE.fetch_add(1, Ordering::Relaxed) < 4 {
            let mut reclaimed = false;
            for _ in 0..TX_POLL_ITERS {
                if Self::desc_opts1(desc) & DESC_OWN == 0 {
                    reclaimed = true;
                    break;
                }
                khal::time::busy_wait(ktime_types::TimeSpan::from_micros(TX_POLL_STEP_US));
            }
            log::info!(
                "rtl8125: tx[{}] len {frame_len} desc {:#010x} -> {:#010x} reclaimed {reclaimed} \
                 cmd {:#04x} intr {:#06x} rx[0] {:#010x}",
                index,
                DESC_OWN | DESC_FIRST_FRAG | DESC_LAST_FRAG | ring_end | frame_len as u32,
                Self::desc_opts1(desc),
                self.bar0.read8(reg::CHIP_CMD),
                self.bar0.read32(reg::INTR_STATUS),
                Self::desc_opts1(self.rx_desc(0)),
            );
            log::info!(
                "rtl8125: state txring {:#010x}:{:#010x} rxring {:#010x}:{:#010x} txpoll {:#06x} \
                 mcu {:#04x}",
                self.bar0.read32(reg::TX_DESC_ADDR_HIGH),
                self.bar0.read32(reg::TX_DESC_ADDR_LOW),
                self.bar0.read32(reg::RX_DESC_ADDR_HIGH),
                self.bar0.read32(reg::RX_DESC_ADDR_LOW),
                self.bar0.read16(reg::TX_POLL_8125),
                self.bar0.read8(reg::MCU),
            );
            log::info!(
                "rtl8125: mac phy_status {:#04x} (link_up {})",
                self.bar0.read8(reg::PHY_STATUS),
                self.bar0.read8(reg::PHY_STATUS) & 0x02 != 0,
            );
        }
        Ok(())
    }

    fn recycle_tx(&self) -> Result<(), KError> {
        let mut state = self.state.lock();
        while state.tx_clean != state.tx_next {
            let desc = self.tx_desc(state.tx_clean);
            if Self::desc_opts1(desc) & DESC_OWN != 0 {
                break;
            }
            state.tx_clean = (state.tx_clean + 1) % TX_DESC_COUNT;
            self.tx_completed.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    fn recv(&self) -> Result<NetBufHandle, KError> {
        let mut state = self.state.lock();
        let mut index = state.rx_next;
        for _ in 0..RX_DESC_COUNT {
            let desc = self.rx_desc(index);
            let opts1 = Self::desc_opts1(desc);
            if opts1 & DESC_OWN != 0 {
                index = (index + 1) % RX_DESC_COUNT;
                continue;
            }
            Self::barrier_acquire();
            let len = (opts1 & RX_LEN_MASK) as usize;
            let mut buf = self.pool.alloc_boxed().ok_or(KError::NoMemory)?;
            if len == 0 || len > RX_SLOT_LEN {
                // A zero-length or over-long completion is a descriptor error,
                // not a packet: re-arm and keep looking.
                self.arm_rx(index);
                index = (index + 1) % RX_DESC_COUNT;
                continue;
            }
            buf.set_payload_len(len)?;
            // SAFETY: the slot is `RX_SLOT_LEN` bytes and `len <= RX_SLOT_LEN`.
            // The NIC has released the descriptor, so the slot is ours.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    self.rx_slot(index),
                    buf.payload_mut().as_mut_ptr(),
                    len,
                );
            }
            // Re-arm immediately: the buffer was copied out, so the NIC can
            // have the slot back without waiting for the stack to recycle.
            self.arm_rx(index);
            state.rx_next = (index + 1) % RX_DESC_COUNT;
            self.rx_frames.fetch_add(1, Ordering::Relaxed);
            drop(state);
            return Ok(buf.into_handle());
        }
        state.rx_next = index;
        Err(KError::WouldBlock)
    }

    fn recycle_rx(&self, rx_buf: NetBufHandle) -> Result<(), KError> {
        // The receive buffer is a copy, so recycling only returns it to the
        // pool; the descriptor was re-armed in `recv`.
        // SAFETY: the handle came from `recv`, which built it with
        // `NetBuf::into_handle`; this is its single consumption.
        drop(unsafe { NetBuf::from_handle(rx_buf) });
        Ok(())
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

        log::info!(
            "rtl8125: {chip} xid {xid:#05x} MAC {mac:02x?} BAR0 {:#x}+{:#x} cmd {:#04x} intr \
             {:#06x} rxcfg {:#010x}",
            region.base,
            region.size,
            bar0.read8(reg::CHIP_CMD),
            bar0.read32(reg::INTR_STATUS),
            bar0.read32(reg::RX_CONFIG),
        );

        reset_and_configure(&bar0, chip)?;

        let phy_id1 = mii_read(&bar0, MII_PHYSID1).unwrap_or(0);
        let phy_id2 = mii_read(&bar0, MII_PHYSID2).unwrap_or(0);
        let bmcr = mii_read(&bar0, MII_BMCR).unwrap_or(0);
        let is_up = link_is_up(&bar0);
        log::info!(
            "rtl8125: {chip} phy {phy_id1:04x}:{phy_id2:04x} bmcr {bmcr:#06x} link {}",
            if is_up { "up" } else { "down" },
        );

        let (tx_ring, rx_ring, tx_slots, rx_slots) = allocate_rings(dev)?;
        // Point each RX descriptor at its slot once; `arm_rx` only rewrites
        // the ownership word afterwards.
        for index in 0..RX_DESC_COUNT {
            // SAFETY: `index < RX_DESC_COUNT` and the receive ring holds
            // exactly `RX_DESC_COUNT` descriptors of `DESC_LEN` bytes.
            let desc = unsafe { rx_ring.as_ptr().add(index * DESC_LEN) };
            Rtl8125Nic::desc_set_addr(desc, rx_slots.bus_addr() + (index * RX_SLOT_LEN) as u64);
        }
        for index in 0..TX_DESC_COUNT {
            // SAFETY: as above, for the transmit ring.
            let desc = unsafe { tx_ring.as_ptr().add(index * DESC_LEN) };
            Rtl8125Nic::desc_set_addr(desc, tx_slots.bus_addr() + (index * TX_SLOT_LEN) as u64);
            // opts2 carries VLAN/TSO options; a plain frame leaves it zero,
            // and the reference always writes it rather than trusting the
            // allocation to be zeroed.
            Rtl8125Nic::desc_set_opts2(desc, 0);
            // Owned by software until a frame is queued.
            let ring_end = if index == TX_DESC_COUNT - 1 {
                DESC_RING_END
            } else {
                0
            };
            Rtl8125Nic::desc_set_opts1(desc, ring_end);
        }

        let nic = Rtl8125Nic {
            bar0,
            mac,
            tx_ring,
            rx_ring,
            tx_slots,
            rx_slots,
            pool: NetBufPool::new(RX_DESC_COUNT + TX_DESC_COUNT, RX_SLOT_LEN)
                .map_err(|_| ProbeError::Fatal(RuntimeError::NoMemory))?,
            state: Mutex::new(RingState {
                tx_next: 0,
                tx_clean: 0,
                rx_next: 0,
            }),
            tx_completed: AtomicU64::new(0),
            rx_frames: AtomicU64::new(0),
        };
        start_data_path(&nic)?;

        let publication = dev
            .publish(&subsys_net::NET, Box::new(nic), "rtl8125")
            .map_err(|_| ProbeError::Fatal(RuntimeError::Io))?;
        log::info!("rtl8125: {chip} published to the net class as a NIC");

        Ok(Rtl8125Data {
            _publication: publication,
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
