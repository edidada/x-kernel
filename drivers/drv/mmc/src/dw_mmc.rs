// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DesignWare Mobile Storage Host controller core — the **PIO-mode**
//! subset of Linux `drivers/mmc/host/dw_mmc.c`: register map, clock
//! sequencing, and the FIFO-polled data path.
//!
//! The data phase is PIO by deliberate choice, with the trap the register
//! map records: with IDMAC routing left armed, the FIFO feeds the DMA
//! engine and `DATA_OVER` never fires. The phase is issued with
//! `DATA_EXPECT`, then the loop polls RINTSTS — **error bits first** (a
//! data error never raises DATA_OVER) — draining the FIFO by FCNT until
//! the transfer completes, tail included.
//!
//! Note on provenance: the tgoskits reference this port started from
//! (`drivers/blk/dwmmc-host`) is **IDMAC-only** — its FIFO/PIO fallback was
//! removed upstream, and RK3588 SD boot was restored there through the
//! IDMAC path (plus tolerating an unsupported CMD6). PIO here is this
//! port's own path, so it is validated against Linux `dw_mci`, not against
//! the reference. Both paths failed the same way on this board (response
//! start-bit/CRC errors on the first wide data phase), which is why the
//! clock facts below are stated so loudly.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use driver_base::{CoherentMem, IoMap};
use kspin::SpinNoPreempt;
use subsys_mmc::{BusWidth, DataDir, MmcCommand, MmcError, MmcHost, MmcIos, MmcResponse, RespKind};

// ---------------------------------------------------------------------------
// Register map — Linux `drivers/mmc/host/dw_mmc.h`.
// ---------------------------------------------------------------------------
const SDMMC_CTRL: usize = 0x000;
const SDMMC_PWREN: usize = 0x004;
const SDMMC_CLKDIV: usize = 0x008;
const SDMMC_CLKSRC: usize = 0x00c;
const SDMMC_CLKENA: usize = 0x010;
const SDMMC_TMOUT: usize = 0x014;
const SDMMC_CTYPE: usize = 0x018;
const SDMMC_BLKSIZ: usize = 0x01c;
const SDMMC_BYTCNT: usize = 0x020;
const SDMMC_INTMASK: usize = 0x024;
const SDMMC_CMDARG: usize = 0x028;
const SDMMC_CMD: usize = 0x02c;
const SDMMC_RESP0: usize = 0x030;
const SDMMC_RINTSTS: usize = 0x044;
const SDMMC_STATUS: usize = 0x048;
const SDMMC_FIFOTH: usize = 0x04c;
const SDMMC_CDETECT: usize = 0x050;
const SDMMC_TCBCNT: usize = 0x05c;
const SDMMC_TBBCNT: usize = 0x060;
const SDMMC_VERID: usize = 0x06c;
const SDMMC_HCON: usize = 0x070;
// Internal DMA controller (Linux `dw_mci_idmac_*`).
const SDMMC_BMOD: usize = 0x080;
const SDMMC_PLDMND: usize = 0x084;
const SDMMC_DBADDR: usize = 0x088;
const SDMMC_IDSTS: usize = 0x08c;
const SDMMC_IDINTEN: usize = 0x090;
/// Engine progress pointers: descriptor address being fetched and buffer
/// address being written (32-bit descriptor mode).
const SDMMC_DSCADDR: usize = 0x094;
const SDMMC_BUFADDR: usize = 0x098;
/// FIFO data port: pre-2.40a at 0x100, 2.40a+ at 0x200 (Linux
/// `DATA_OFFSET` / `DATA_240A_OFFSET`).
const SDMMC_DATA_OFFSET: usize = 0x100;
/// Card-read threshold (2.40a+; at 0x100 the older IP has the FIFO port).
const SDMMC_CDTHRCTL: usize = 0x100;
const SDMMC_DATA_240A_OFFSET: usize = 0x200;
/// VERID register value that moved the data port to 0x200.
const DW_MMC_240A: u32 = 0x240a;

// CTRL bits. The interrupt line stays off for good (this driver is
// poll-only), so `CTRL_INT_ENABLE` (bit 4) is never set anywhere below.
const CTRL_DMA_ENABLE: u32 = 1 << 5;
const CTRL_DMA_RESET: u32 = 1 << 2;
const CTRL_FIFO_RESET: u32 = 1 << 1;
const CTRL_RESET: u32 = 1 << 0;
const CTRL_USE_IDMAC: u32 = 1 << 25;
/// Abort an in-flight read data phase (databook `abort_read_data`, pulse
/// bit). A FIFO reset alone does not clear a data state machine that is
/// mid-transfer.
const CTRL_ABORT_READ_DATA: u32 = 1 << 8;
const CTRL_ALL_RESET: u32 = CTRL_RESET | CTRL_FIFO_RESET | CTRL_DMA_RESET;

// CTYPE bits.
const CTYPE_4BIT: u32 = 1 << 0;

// INT/RINTSTS bits.
const INT_RESP_ERR: u32 = 1 << 1;
const INT_CMD_DONE: u32 = 1 << 2;
const INT_DATA_OVER: u32 = 1 << 3;
const INT_RCRC: u32 = 1 << 6;
const INT_DCRC: u32 = 1 << 7;
const INT_RTO: u32 = 1 << 8;
const INT_DRTO: u32 = 1 << 9;
const INT_HTO: u32 = 1 << 10;
const INT_FRUN: u32 = 1 << 11;
const INT_HLE: u32 = 1 << 12;
const INT_SBE: u32 = 1 << 13;
const INT_ACD: u32 = 1 << 14;
const INT_EBE: u32 = 1 << 15;
/// Data-phase error vocabulary (Linux `dw_mci_adjust_fifoth`-era set).
const INT_ERRORS: u32 = INT_EBE
    | INT_SBE
    | INT_HTO
    | INT_FRUN
    | INT_HLE
    | INT_DCRC
    | INT_RCRC
    | INT_RTO
    | INT_DRTO
    | INT_RESP_ERR;
const INT_DATA_ERRORS: u32 =
    INT_EBE | INT_SBE | INT_HTO | INT_FRUN | INT_HLE | INT_DCRC | INT_RCRC | INT_RTO | INT_DRTO;

// CMD bits.
const CMD_START: u32 = 1 << 31;
const CMD_USE_HOLD_REG: u32 = 1 << 29;
const CMD_UPD_CLK: u32 = 1 << 21;
const CMD_INIT: u32 = 1 << 15;
const CMD_STOP: u32 = 1 << 14;
const CMD_SEND_AUTO_STOP: u32 = 1 << 12;
const CMD_PRV_DAT_WAIT: u32 = 1 << 13;
const CMD_DAT_WR: u32 = 1 << 10;
const CMD_DAT_EXP: u32 = 1 << 9;
const CMD_RESP_CRC: u32 = 1 << 8;
const CMD_RESP_LONG: u32 = 1 << 7;
const CMD_RESP_EXP: u32 = 1 << 6;
/// The CMD register index field is bits [5:0] — six bits. The 0x1F mask
/// in Linux dw_mmc.h is a macro that driver itself does not apply
/// (`cmdr = cmd->opcode` verbatim); masking to five bits silently
/// rewrites ACMD41 (41) into CMD9.
const CMD_INDX_MASK: u32 = 0x3F;

// CLKENA bits.
const CLKEN_ENABLE: u32 = 1 << 0;

// BMOD bits (Linux `SDMMC_IDMAC_*`).
const BMOD_SWRESET: u32 = 1 << 0;
const BMOD_FB: u32 = 1 << 1;
const BMOD_ENABLE: u32 = 1 << 7;

// IDMAC descriptor word 0 (Linux `IDMAC_DESC_*`).
const DESC_OWN: u32 = 1 << 31;
const DESC_CH: u32 = 1 << 4;
const DESC_FS: u32 = 1 << 3;
const DESC_LD: u32 = 1 << 2;
const DESC_DIC: u32 = 1 << 1;

// IDSTS summary bits; `IDINTEN` stays 0 because completion is polled, but
// the status bits are mask-independent.
const IDMAC_INT_TI: u32 = 1 << 0;
const IDMAC_INT_RI: u32 = 1 << 1;
const IDMAC_INT_FBE: u32 = 1 << 2;
const IDMAC_INT_DU: u32 = 1 << 4;
const IDMAC_INT_CES: u32 = 1 << 5;
const IDMAC_INT_NI: u32 = 1 << 8;
const IDMAC_INT_AI: u32 = 1 << 9;
const IDMAC_INT_ERRORS: u32 = IDMAC_INT_FBE | IDMAC_INT_DU | IDMAC_INT_CES | IDMAC_INT_AI;
const IDMAC_INT_CLEAR: u32 = IDMAC_INT_ERRORS | IDMAC_INT_NI | IDMAC_INT_RI | IDMAC_INT_TI;
/// What `IDINTEN` enables. It stays enabled even though completion is polled:
/// on this controller the `IDSTS` completion bits do not latch with
/// `IDINTEN == 0` (the reference driver writes this unconditionally, and
/// Linux never runs IDMAC with it cleared). `CTRL.INT_ENABLE` stays 0 and
/// `INTMASK` stays 0, so nothing reaches the interrupt line.
const IDMAC_INT_ENABLE: u32 = IDMAC_INT_NI | IDMAC_INT_RI | IDMAC_INT_TI | IDMAC_INT_ERRORS;

/// One IDMAC descriptor (Linux `struct idmac_desc`), 16 bytes, 16-byte
/// aligned; the DMA engine walks `des3` from `DBADDR`.
#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
struct IdmacDesc {
    des0: u32,
    des1: u32,
    des2: u32,
    des3: u32,
}

const IDMAC_DESC_SIZE: usize = core::mem::size_of::<IdmacDesc>();
/// Descriptor payload cap. The length field is 13 bits, but Linux programs
/// 4 KiB per descriptor and never cross a 4 KiB boundary with one
/// descriptor — the engine's burst handling assumes it.
const IDMAC_DESC_MAX_BYTES: usize = 4096;
pub(crate) const IDMAC_RING_BYTES: usize = 4096;
const IDMAC_RING_DESCS: usize = IDMAC_RING_BYTES / IDMAC_DESC_SIZE;
/// Bounce window: every transfer is staged through it, which is what keeps
/// the descriptor addresses inside the 32-bit IDMAC limit and the cache
/// maintenance out of the block path.
pub(crate) const IDMAC_WINDOW_BYTES: usize = 1024 * 1024;
/// Smallest transfer worth handing to the engine. Below one block the setup
/// cost dominates and the transfer is a register read (SCR 8 B, CMD6 64 B), so
/// those stay on PIO — card detection then never depends on the DMA path.
const IDMAC_MIN_BYTES: usize = 512;

/// STATUS register card-busy indicator (DAT0 driven low).
const STATUS_CARD_BUSY: u32 = 1 << 9;
/// STATUS FIFO count field [28:17], in FIFO **entries** (the entry width
/// is `HCON.HDATA_WIDTH`, not necessarily 32 bits — Linux
/// `SDMMC_GET_FCNT` + `host->data_shift`).
const STATUS_FCNT_SHIFT: u32 = 17;

/// HCON FIFO-width field [9:7] (Linux `SDMMC_GET_HDATA_WIDTH`):
/// 0 = 16-bit, 2 = 64-bit, any other value means 32-bit.
const HCON_HDATA_WIDTH_SHIFT: u32 = 7;

/// FIFO entry width in bytes from `HCON.HDATA_WIDTH`. The width fixes
/// both the FIFO access size and the unit of `STATUS.FCNT`.
const fn fifo_width_bytes(hcon: u32) -> u32 {
    match (hcon >> HCON_HDATA_WIDTH_SHIFT) & 0x7 {
        0 => 2,
        2 => 8,
        _ => 4,
    }
}

/// R1 card-status bits that mean the card refused or failed a command
/// (SD spec 4.10.1): bits 31..26 and 24..16. `READY_FOR_DATA` (8) and
/// `CURRENT_STATE` (12:9) are status, not errors, and stay out of it.
const R1_ERROR_BITS: u32 = 0xFDFF_0000;

/// R1 `CURRENT_STATE` field names (`(r1 >> 9) & 0xF`).
const CARD_STATES: [&str; 9] = [
    "idle", "ready", "ident", "stby", "tran", "data", "rcv", "prg", "dis",
];

// FIFOTH layout: MSIZE [30:28], RX_WMARK [27:16], TX_WMARK [11:0].
const fn fifoth(msize: u32, rx_wmark: u32, tx_wmark: u32) -> u32 {
    ((msize & 0x7) << 28) | ((rx_wmark & 0xFFF) << 16) | (tx_wmark & 0xFFF)
}

/// Command-issue poll budget (START clear + clock-inform frames).
const CMD_POLL_LIMIT: u32 = 1 << 21;
/// Data-phase poll budget — a full 512-byte block at 400 kHz worst case
/// needs ~11 ms; the bound covers it many times over.
const DATA_POLL_LIMIT: u32 = 1 << 26;
/// Engine-completion budget: far shorter than the PIO one, because a stalled
/// DMA engine is not going to finish and the caller has a fallback to run.
const IDMAC_POLL_LIMIT: u32 = 1 << 20;

/// Bounded poll iterator with a spin hint (bring-up-grade waits).
fn spin_budget(limit: u32) -> impl Iterator<Item = u32> {
    (0..limit).inspect(|_| core::hint::spin_loop())
}

/// The DW MMC host slot state (Linux `struct dw_mci`).
pub struct DwMmcHost {
    /// Mapped register block (probe-scoped `IoMap`).
    regs: IoMap,
    /// CIU input clock in Hz (board fact — see the rockchip glue).
    bus_hz: u32,
    /// FIFO depth in entries (DT `fifo-depth`).
    fifo_depth: u32,
    /// One FIFO entry in bytes (`HCON.HDATA_WIDTH`): fixes both the FIFO
    /// access size and the unit of `STATUS.FCNT` (Linux `data_shift`).
    fifo_width: usize,
    /// Offset of the FIFO data register (VERID-dependent).
    fifo_off: usize,
    /// Requested card clock ceiling (Linux `mmc->f_max`): the lower of the
    /// DT `max-frequency` and the board's `MMC_MAX_CARD_HZ` (0 = no cap).
    max_card_hz: u32,
    /// Widest data bus this slot drives (Linux `MMC_CAP_4_BIT_DATA`), a
    /// board fact: unusable DAT1..DAT3 lines fail every data phase.
    max_width: BusWidth,
    /// Host slot index (`mmc_host->index`).
    slot: u8,
    /// Current configured bus clock (0 = gated).
    current_clock: AtomicU32,
    /// Serializes command and data phases on this slot.
    ///
    /// One card is reachable through several disks (`mmcblk0p1`,
    /// `mmcblk0p2`) and from several tasks at once; a second phase started
    /// while one is in flight re-writes the descriptor ring, stops the
    /// engine and clears the routing under the first, which then times out
    /// with `TCBCNT` frozen. Preemption is what gets disabled rather than
    /// IRQs: a transfer is milliseconds long and IRQs must keep running.
    transfer: SpinNoPreempt<()>,
    /// Descriptor ring + bounce window for the internal DMA engine. `None`
    /// when the probe could not provide DMA-safe memory, which leaves the
    /// FIFO-polled PIO path in charge.
    idmac: Option<IdmacBuffers>,
    /// Whether the data path still uses IDMAC. Latches off on the first
    /// engine failure so a bad engine degrades to PIO instead of failing
    /// every request.
    idmac_ok: AtomicBool,
}

/// The two DMA allocations the engine needs (`dmam_alloc_coherent` in
/// Linux terms). Both must be addressable by the 32-bit descriptor format.
pub(crate) struct IdmacBuffers {
    ring: CoherentMem,
    window: CoherentMem,
}

impl IdmacBuffers {
    /// Descriptor ring and bounce window, plus the 32-bit reachability
    /// check the descriptor format imposes.
    pub(crate) fn new(ring: CoherentMem, window: CoherentMem) -> Option<Self> {
        let descriptor_limit = u64::try_from(IDMAC_RING_BYTES).unwrap_or(u64::MAX);
        let window_limit = u64::try_from(IDMAC_WINDOW_BYTES).unwrap_or(u64::MAX);
        let fits = |bus: u64, len: u64| bus.saturating_add(len) <= u64::from(u32::MAX) + 1;
        if ring.len() < IDMAC_RING_BYTES
            || window.len() < IDMAC_WINDOW_BYTES
            || !fits(ring.bus_addr(), descriptor_limit)
            || !fits(window.bus_addr(), window_limit)
        {
            return None;
        }
        Some(Self { ring, window })
    }
}

// SAFETY: `regs` is a probe-owned MMIO mapping; all accessors go through
// `&self` with no shared mutable state outside the slot's own command
// sequence (single-threaded per slot by the model's discipline).
unsafe impl Sync for DwMmcHost {}
// SAFETY: no thread-local state; see the `Sync` note.
unsafe impl Send for DwMmcHost {}

impl DwMmcHost {
    /// Controller bring-up (Linux `dw_mci_probe` PIO subset + U-Boot
    /// `dwmci_init` order): power, all-reset, status clear, full line
    /// mask (poll-only), timeouts, IDMAC routing disabled, FIFO
    /// thresholds, VERID-dependent FIFO port, CIU clock gated.
    ///
    /// `max_card_hz` is the requested card-clock ceiling (Linux
    /// `mmc->f_max`): every `set_ios` request is clamped to it, which is
    /// how a board declares that its SD clock domain cannot tolerate the
    /// nominal mode rate. 0 means "no cap".
    pub(crate) fn new(
        regs: IoMap,
        bus_hz: u32,
        fifo_depth: u32,
        slot: u8,
        max_card_hz: u32,
        max_width: BusWidth,
        idmac: Option<IdmacBuffers>,
    ) -> Result<Self, MmcError> {
        if bus_hz == 0 || fifo_depth < 2 {
            return Err(MmcError::Unsupported);
        }
        let idmac_ok = kbuild_config::MMC_IDMAC && idmac.is_some();
        let mut host = Self {
            regs,
            bus_hz,
            fifo_depth,
            fifo_width: 4,
            slot,
            fifo_off: SDMMC_DATA_240A_OFFSET,
            max_card_hz,
            max_width,
            current_clock: AtomicU32::new(0),
            transfer: SpinNoPreempt::new(()),
            idmac,
            idmac_ok: AtomicBool::new(idmac_ok),
        };

        // Power-cycle the card BEFORE the controller reset (Linux
        // `mmc_power_up`: off, >=1 ms, on). `PWREN` gates the card's vmmc rail
        // on switchable boards, and the boot loader may have left the card
        // selected in 4-bit high-speed mode — in which case the first
        // initialisation runs against a card whose state machine is not where
        // CMD0 assumes, which is exactly the shape this board showed: the
        // first attempt after power-on failed the first 4-bit data phase, and
        // an identical second attempt worked.
        host.writel(SDMMC_PWREN, 0);
        host.power_delay();
        host.writel(SDMMC_PWREN, 1);
        host.power_delay();
        // Global + FIFO + DMA reset, each bit self-clearing.
        host.writel(SDMMC_CTRL, CTRL_ALL_RESET);
        for _ in spin_budget(10_000) {
            if host.readl(SDMMC_CTRL) & CTRL_ALL_RESET == 0 {
                break;
            }
        }
        // Clear all raw status; the line stays masked for good — this
        // driver is poll-only by design.
        host.writel(SDMMC_RINTSTS, 0xFFFF_FFFF);
        host.writel(SDMMC_INTMASK, 0);
        host.writel(SDMMC_TMOUT, 0xFFFF_FFFF);
        // Route the FIFO away from the IDMAC: the CTRL reset pulses
        // DMA_RESET but does NOT clear USE_IDMAC/DMA_ENABLE, and a stale
        // routing makes every PIO read hang (verified driver's note).
        let ctrl = host.readl(SDMMC_CTRL) & !(CTRL_USE_IDMAC | CTRL_DMA_ENABLE);
        host.writel(SDMMC_CTRL, ctrl);
        // FIFO thresholds: the baseline Linux programs at probe
        // (`dw_mci_probe`). The per-transfer geometry in
        // `dw_mci_adjust_fifoth` is DMA-only — it returns early when PIO
        // is in use — so it is deliberately not applied here.
        host.writel(SDMMC_FIFOTH, host.baseline_fifoth());
        // Reset the internal DMA engine as well (Linux `dw_mci_idmac_init`):
        // the `CTRL` reset pulses the DMA reset but never touches `BMOD`, so a
        // bootloader that left the engine enabled keeps it enabled across our
        // probe — observed on this board as a stale `BMOD.DE | FB` with a stale
        // `DBADDR`, and as "the first init attempt after power-on always fails,
        // the second (after the engine was stopped) works".
        host.writel(SDMMC_IDSTS, IDMAC_INT_CLEAR);
        host.writel(SDMMC_BMOD, host.readl(SDMMC_BMOD) | BMOD_SWRESET);
        for _ in spin_budget(10_000) {
            if host.readl(SDMMC_BMOD) & BMOD_SWRESET == 0 {
                break;
            }
        }
        host.writel(SDMMC_BMOD, BMOD_FB);
        host.writel(SDMMC_IDINTEN, 0);
        // Gate CIU clock before the first setup_bus.
        host.writel(SDMMC_CLKENA, 0);
        host.writel(SDMMC_CLKSRC, 0);

        // VERID selects the FIFO data-port offset.
        let verid = host.readl(SDMMC_VERID) & 0xFFFF;
        host.fifo_off = if verid < DW_MMC_240A {
            SDMMC_DATA_OFFSET
        } else {
            SDMMC_DATA_240A_OFFSET
        };
        // Controller identity on the record: the IP revision and the FIFO
        // geometry decide the data-port offset, the FIFO access width and
        // the unit of `STATUS.FCNT`, so they are the first thing to check
        // when a data phase misbehaves.
        let hcon = host.readl(SDMMC_HCON);
        host.fifo_width = fifo_width_bytes(hcon) as usize;
        log::info!(
            "dwmmc: slot {} verid {verid:#06x} hcon {hcon:#010x} fifo {fifo_depth} x {}B, \
             data-port {:#x}, ciu {bus_hz} Hz, card-clock ceiling {max_card_hz} Hz, data path {}",
            host.slot,
            host.fifo_width,
            host.fifo_off,
            if host.idmac_ok.load(Ordering::Acquire) {
                "idmac"
            } else {
                "pio (fifo-polled)"
            },
        );
        Ok(host)
    }

    /// Coarse wait covering the card power-up/power-down minimums (~1 ms each
    /// at any rate this kernel runs). Boot/probe context only; a timer would be
    /// the precise tool, but probe has no parking guarantee.
    fn power_delay(&self) {
        for _ in spin_budget(1_000_000) {
            core::hint::spin_loop();
        }
    }

    /// The host slot index (disk naming + bus-instance label).
    pub const fn slot(&self) -> u8 {
        self.slot
    }

    fn readl(&self, off: usize) -> u32 {
        self.regs.read32(off)
    }

    fn writel(&self, off: usize, value: u32) {
        self.regs.write32(off, value)
    }

    /// One FIFO entry read at the width `HCON` advertises (Linux
    /// `dw_mci_pull_data16/32/64`). `entry` is zeroed beyond the width.
    fn fifo_read_entry(&self, entry: &mut [u8; 8]) {
        match self.fifo_width {
            2 => entry[..2].copy_from_slice(&self.regs.read16(self.fifo_off).to_le_bytes()),
            8 => entry[..8].copy_from_slice(&self.regs.read64(self.fifo_off).to_le_bytes()),
            _ => entry[..4].copy_from_slice(&self.readl(self.fifo_off).to_le_bytes()),
        }
    }

    /// One FIFO entry write at the width `HCON` advertises.
    fn fifo_write_entry(&self, entry: &[u8; 8]) {
        let mut word = [0u8; 8];
        word.copy_from_slice(&entry[..self.fifo_width]);
        match self.fifo_width {
            2 => self
                .regs
                .write16(self.fifo_off, u16::from_le_bytes([word[0], word[1]])),
            8 => self.regs.write64(self.fifo_off, u64::from_le_bytes(word)),
            _ => self.regs.write32(
                self.fifo_off,
                u32::from_le_bytes([word[0], word[1], word[2], word[3]]),
            ),
        }
    }

    /// Bytes currently held in the FIFO (`FCNT` counts FIFO entries).
    fn fifo_bytes(&self) -> usize {
        let fcnt = (self.readl(SDMMC_STATUS) >> STATUS_FCNT_SHIFT) & 0x1FFF;
        (fcnt as usize) * self.fifo_width
    }

    /// Baseline FIFO thresholds (Linux `fifoth_val`): what the probe
    /// programs and what PIO runs on.
    fn baseline_fifoth(&self) -> u32 {
        let half = self.fifo_depth / 2;
        fifoth(0x2, half - 1, half)
    }

    /// Per-transfer thresholds for the DMA burst (Linux
    /// `dw_mci_adjust_fifoth`): the largest burst that divides both the
    /// block depth and the TX-mark inverse, RX watermark one burst behind.
    /// DMA-only — the same function returns early for PIO upstream.
    fn dma_fifoth(&self, block_size: u32) -> u32 {
        const MSIZE_WORDS: [u32; 8] = [1, 4, 8, 16, 32, 64, 128, 256];
        let tx_wmark = self.fifo_depth / 2;
        let tx_inverse = self.fifo_depth - tx_wmark;
        let mut msize = 0u32;
        let mut rx_wmark = 1u32;
        let fifo_width = u32::try_from(self.fifo_width).unwrap_or(4);
        if block_size.is_multiple_of(fifo_width) {
            let block_depth = block_size / fifo_width;
            for index in (1..MSIZE_WORDS.len()).rev() {
                let burst = MSIZE_WORDS[index];
                if block_depth.is_multiple_of(burst) && tx_inverse.is_multiple_of(burst) {
                    msize = index as u32;
                    rx_wmark = burst - 1;
                    break;
                }
            }
        }
        fifoth(msize, rx_wmark, tx_wmark)
    }

    /// Issue one command frame and poll the START bit (the controller
    /// clears it when the CIU accepts the command — Linux `mci_send_cmd`).
    /// `wait_busy` mirrors `dw_mci_wait_while_busy`: with PRV_DAT_WAIT the
    /// software also waits for DAT0 to release before writing START.
    fn ciu_cmd(&self, cmdr: u32, arg: u32, wait_busy: bool) -> Result<(), MmcError> {
        self.writel(SDMMC_CMDARG, arg);
        if wait_busy {
            for _ in spin_budget(CMD_POLL_LIMIT) {
                if self.readl(SDMMC_STATUS) & STATUS_CARD_BUSY == 0 {
                    break;
                }
            }
        }
        self.writel(SDMMC_CMD, cmdr | CMD_START);
        for _ in spin_budget(CMD_POLL_LIMIT) {
            if self.readl(SDMMC_CMD) & CMD_START == 0 {
                return Ok(());
            }
        }
        Err(MmcError::Timeout)
    }

    /// Clock sequencing (Linux `dw_mci_setup_bus`): gate → inform CIU →
    /// divider → inform CIU → enable → inform CIU → CTYPE.
    fn setup_bus(&self, clock_hz: u32, force: bool) -> Result<(), MmcError> {
        if clock_hz == self.current_clock.load(Ordering::Acquire) && !force {
            return Ok(());
        }
        let div: u32 = match clock_hz {
            0 => 0,
            _ if self.bus_hz == clock_hz => 0,
            _ => {
                let mut div = self.bus_hz / clock_hz;
                if !self.bus_hz.is_multiple_of(clock_hz) && self.bus_hz > clock_hz {
                    div += 1;
                }
                div.div_ceil(2)
            }
        };
        // The protocol asks for a mode's nominal rate; the divider decides what
        // the card actually sees. Log the programmed result, because a request
        // that the host cannot reach (source slower than the mode) otherwise
        // reads in the log as if the mode had been applied.
        let programmed = if div == 0 {
            self.bus_hz
        } else {
            self.bus_hz / (div * 2)
        };
        log::info!(
            "dwmmc: slot {} clock: requested {clock_hz} Hz, programmed {programmed} Hz (clkdiv \
             {div})",
            self.slot
        );
        let inform = CMD_UPD_CLK | CMD_PRV_DAT_WAIT;
        self.writel(SDMMC_CLKENA, 0);
        self.ciu_cmd(inform, 0, false)?;
        self.writel(SDMMC_CLKDIV, div);
        self.ciu_cmd(inform, 0, false)?;
        self.writel(SDMMC_CLKENA, CLKEN_ENABLE);
        self.ciu_cmd(inform, 0, false)?;
        Ok(())
    }

    /// Command register encoding (Linux `dw_mci_prepare_command`): USE_HOLD
    /// and WAIT_PRVDATA (non-CMD13 data commands) always on, response bits
    /// by kind, INIT/AUTO_STOP by request.
    fn cmdr_of(&self, cmd: &MmcCommand, dir: Option<DataDir>, bytes: usize) -> u32 {
        let mut cmdr = u32::from(cmd.opcode) & CMD_INDX_MASK;
        match cmd.resp {
            RespKind::None => {}
            RespKind::R2 => cmdr |= CMD_RESP_EXP | CMD_RESP_LONG | CMD_RESP_CRC,
            // OCR-shaped and ShortNoCrc responses carry undefined CRC bits
            // — enabling the CRC check flags every transfer.
            RespKind::R3 | RespKind::ShortNoCrc => cmdr |= CMD_RESP_EXP,
            RespKind::R1 | RespKind::R1b => cmdr |= CMD_RESP_EXP | CMD_RESP_CRC,
        }
        if let Some(dir) = dir {
            cmdr |= CMD_DAT_EXP;
            if dir == DataDir::Write {
                cmdr |= CMD_DAT_WR;
            }
            // Auto-stop rides the data command (CMD18 multi-block reads);
            // u16::from(CMD12) for the explicit stop variant.
            if cmd.auto_stop && bytes > 512 {
                cmdr |= CMD_SEND_AUTO_STOP;
            }
        }
        if cmd.init_clocks {
            cmdr |= CMD_INIT;
        }
        if cmd.stop {
            // STOP/ABORT frame. Linux sets it for GO_IDLE (CMD0),
            // GO_INACTIVE (CMD15) and STOP_TRANSMISSION (CMD12); without
            // it the controller sends those as ordinary commands.
            cmdr |= CMD_STOP;
        }
        // Hold register always on (verified driver default; RK3588 does
        // not declare NO_USE_HOLD), and data commands wait for previous
        // data to finish.
        cmdr |= CMD_USE_HOLD_REG;
        if dir.is_some() {
            cmdr |= CMD_PRV_DAT_WAIT;
        }
        cmdr
    }

    /// Read the response registers (Linux `dw_mci_get_resp`): RESP0 holds
    /// the most-significant word `{127:96}`, matching the raw layout
    /// [`MmcResponse::bits`] indexes.
    /// Read the response registers (Linux `dw_mci_command_complete`): for a
    /// long response `RESP0` holds the **least**-significant word — Linux
    /// stores `resp[3] = RESP0 … resp[0] = RESP3` — so the {@link
    /// MmcResponse::raw} layout, which indexes `{127:96}` first, needs the
    /// registers reversed.
    fn read_response(&self, cmd: &MmcCommand) -> MmcResponse {
        match cmd.resp {
            RespKind::None => MmcResponse { raw: [0; 4] },
            RespKind::R2 => MmcResponse {
                raw: [
                    self.readl(SDMMC_RESP0 + 0xC), // {127:96}
                    self.readl(SDMMC_RESP0 + 8),   // {95:64}
                    self.readl(SDMMC_RESP0 + 4),   // {63:32}
                    self.readl(SDMMC_RESP0),       // {31:0}
                ],
            },
            _ => MmcResponse {
                raw: [self.readl(SDMMC_RESP0), 0, 0, 0],
            },
        }
    }

    /// Human-readable names for a raw **RINTSTS** snapshot (diagnostics).
    ///
    /// The bit vocabulary here is the interrupt/raw-status one; it must
    /// never be applied to `STATUS`, whose layout is unrelated (FIFO
    /// count, data state machine, response index).
    fn rintsts_names(status: u32) -> alloc::string::String {
        const NAMES: [(u32, &str); 11] = [
            (INT_RESP_ERR, "RESP_ERR"),
            (INT_CMD_DONE, "CMD_DONE"),
            (INT_DATA_OVER, "DATA_OVER"),
            (INT_RCRC, "RCRC"),
            (INT_DCRC, "DCRC"),
            (INT_RTO, "RTO"),
            (INT_DRTO, "DRTO"),
            (INT_HTO, "HTO"),
            (INT_SBE, "SBE"),
            (INT_EBE, "EBE"),
            (INT_FRUN, "FRUN"),
        ];
        let names: alloc::vec::Vec<&str> = NAMES
            .iter()
            .filter(|(bit, _)| status & bit != 0)
            .map(|(_, name)| *name)
            .collect();
        alloc::format!("{status:#x} [{}]", names.join("|"))
    }

    /// Human-readable R1 card-status decode (diagnostics): error bits plus
    /// the card state, which is what separates "the card refused the
    /// command" from "the controller dropped the data".
    fn r1_names(r1: u32) -> alloc::string::String {
        const NAMES: [(u32, &str); 15] = [
            (1 << 31, "OUT_OF_RANGE"),
            (1 << 30, "ADDRESS_ERROR"),
            (1 << 29, "BLOCK_LEN_ERROR"),
            (1 << 28, "ERASE_SEQ_ERROR"),
            (1 << 27, "ERASE_PARAM"),
            (1 << 26, "WP_VIOLATION"),
            (1 << 24, "LOCK_UNLOCK_FAILED"),
            (1 << 23, "COM_CRC_ERROR"),
            (1 << 22, "ILLEGAL_COMMAND"),
            (1 << 21, "CARD_ECC_FAILED"),
            (1 << 20, "CC_ERROR"),
            (1 << 19, "ERROR"),
            (1 << 18, "UNDERRUN"),
            (1 << 17, "OVERRUN"),
            (1 << 16, "CSD_OVERWRITE"),
        ];
        let names: alloc::vec::Vec<&str> = NAMES
            .iter()
            .filter(|(bit, _)| r1 & bit != 0)
            .map(|(_, name)| *name)
            .collect();
        let state = usize::try_from((r1 >> 9) & 0xF)
            .ok()
            .and_then(|index| CARD_STATES.get(index))
            .copied()
            .unwrap_or("reserved");
        let ready = if r1 & (1 << 8) != 0 { "ready" } else { "busy" };
        alloc::format!(
            "{r1:#010x} state={state} {ready}{}",
            if names.is_empty() {
                alloc::string::String::new()
            } else {
                alloc::format!(" [{}]", names.join("|"))
            }
        )
    }

    /// Drain whatever the FIFO holds into `buf` (`FCNT`-bounded, one FIFO
    /// entry per access), Linux `dw_mci_read_data_pio` inner loop. A tail
    /// shorter than one entry is legal: the controller holds one entry of
    /// which only the leading bytes belong to the transfer. Returns bytes
    /// drained.
    fn drain_rx(&self, out: &mut [u8]) -> usize {
        let width = self.fifo_width;
        let limit = self.fifo_bytes().min(out.len());
        let mut drained = 0usize;
        while drained < limit {
            let mut entry = [0u8; 8];
            self.fifo_read_entry(&mut entry);
            let take = (limit - drained).min(width);
            out[drained..drained + take].copy_from_slice(&entry[..take]);
            drained += take;
        }
        drained
    }

    /// Full controller snapshot for a failed data phase: the geometry
    /// registers plus the byte counters (`TCBCNT` card↔CIU, `TBBCNT`
    /// host↔FIFO) show whether the CIU moved any bytes at all for the
    /// phase, which separates "card never sent data" from "controller
    /// dropped it".
    fn dump_state(&self, tag: &str, rintsts: u32) {
        log::warn!(
            "dwmmc: {tag}: ctrl={:#010x} status={:#010x} rintsts={} resp0={} intmask={:#x} \
             clkena={:#x} clksrc={:#x} clkdiv={:#x} ctype={:#x} blksiz={:#x} bytcnt={:#x} \
             tcbcnt={} tbbcnt={}",
            self.readl(SDMMC_CTRL),
            self.readl(SDMMC_STATUS),
            Self::rintsts_names(rintsts),
            Self::r1_names(self.readl(SDMMC_RESP0)),
            self.readl(SDMMC_INTMASK),
            self.readl(SDMMC_CLKENA),
            self.readl(SDMMC_CLKSRC),
            self.readl(SDMMC_CLKDIV),
            self.readl(SDMMC_CTYPE),
            self.readl(SDMMC_BLKSIZ),
            self.readl(SDMMC_BYTCNT),
            self.readl(SDMMC_TCBCNT),
            self.readl(SDMMC_TBBCNT),
        );
    }

    /// Clear a fired RINTSTS error set and map it (Linux
    /// `dw_mci_command_complete` error decode).
    fn take_errors(&self, pending: u32) -> MmcError {
        self.writel(SDMMC_RINTSTS, pending & INT_ERRORS);
        if pending & (INT_RTO | INT_RCRC | INT_RESP_ERR) != 0 {
            MmcError::Crc
        } else if pending & (INT_DRTO | INT_HTO) != 0 {
            MmcError::Timeout
        } else {
            MmcError::Io
        }
    }

    /// Unwedge the data path: abort an in-flight read (pulse
    /// `ABORT_READ_DATA`), wait for DAT0 to release, reset the FIFO, then
    /// force-stop the card with CMD12 and clear raw status.
    ///
    /// Order matters and was verified the hard way on this board: FIFO
    /// reset + CMD12 alone leave `STATUS.DATA_STATE_MC_BUSY` set, and the
    /// next data phase then completes instantly with an empty FIFO.
    /// Unwedge the data path: abort an in-flight read (pulse
    /// `ABORT_READ_DATA`), wait for DAT0 to release, reset the FIFO and
    /// clear raw status. `stop_card` additionally force-stops the card with
    /// CMD12.
    ///
    /// `stop_card` is only for the error path, where a data phase was armed
    /// and the card has to be taken out of the data state. A CMD12 sent at a
    /// phase boundary is illegal in TRAN and the card says so
    /// (`R1.ILLEGAL_COMMAND`) — which then has to be told apart from a real
    /// refusal later, so the two cases are not folded together.
    fn reset_data_path(&self, stop_card: bool) {
        self.writel(SDMMC_CTRL, self.readl(SDMMC_CTRL) | CTRL_ABORT_READ_DATA);
        // The abort pulse clears itself; then DAT0 (card busy) must release
        // before the card is addressable again.
        for _ in spin_budget(10_000) {
            if self.readl(SDMMC_CTRL) & CTRL_ABORT_READ_DATA == 0 {
                break;
            }
        }
        for _ in spin_budget(CMD_POLL_LIMIT) {
            if self.readl(SDMMC_STATUS) & STATUS_CARD_BUSY == 0 {
                break;
            }
        }
        self.writel(SDMMC_CTRL, self.readl(SDMMC_CTRL) | CTRL_FIFO_RESET);
        for _ in spin_budget(10_000) {
            if self.readl(SDMMC_CTRL) & CTRL_FIFO_RESET == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        if stop_card {
            let stop = CMD_STOP | CMD_RESP_EXP | CMD_RESP_CRC | CMD_USE_HOLD_REG | 12;
            let _ = self.ciu_cmd(stop, 0, true);
        }
        self.writel(SDMMC_RINTSTS, 0xFFFF_FFFF);
    }

    /// Recovery after a failed data transfer (Linux error path). Without
    /// this a card parked mid-data-state poisons every later command.
    fn recover_after_data_error(&self, failed_cmd: &MmcCommand, rintsts: u32) {
        self.reset_data_path(true);
        log::warn!(
            "dwmmc: recovered after data error on cmd {} (rintsts {})",
            failed_cmd.opcode,
            Self::rintsts_names(rintsts),
        );
    }
}

impl MmcHost for DwMmcHost {
    fn recover(&self) {
        // Phase-boundary hygiene only: the protocol core owns the card's
        // state here, and a data phase that ran to `DATA_OVER` leaves the
        // card in TRAN by itself. `STATUS.DATA_STATE_MC_BUSY` is not a
        // reliable "transfer in flight" signal (it stays asserted after a
        // completed phase), so it must not be used to decide whether to
        // send a stop command.
        let _slot = self.transfer.lock();
        self.reset_data_path(false);
    }

    fn send_cmd(&self, cmd: &MmcCommand) -> Result<MmcResponse, MmcError> {
        let _slot = self.transfer.lock();
        // Fallback diagnostics: EVERY command failure names itself here.
        match self.send_cmd_inner(cmd) {
            Ok(resp) => Ok(resp),
            Err(err) => {
                log::warn!(
                    "dwmmc: cmd {} arg {:#x} resp {:?} -> {err:?}",
                    cmd.opcode,
                    cmd.arg,
                    cmd.resp
                );
                Err(err)
            }
        }
    }

    fn read_data_cmd(
        &self,
        cmd: &MmcCommand,
        bytes: usize,
        buf: &mut [u8],
    ) -> Result<(), MmcError> {
        let _slot = self.transfer.lock();
        if self.idmac_wants(bytes) {
            match self.data_cmd_read_idmac(cmd, bytes, buf) {
                Ok(()) => return Ok(()),
                Err(err) => self.degrade_to_pio(cmd, err),
            }
        }
        self.data_cmd_read(cmd, bytes, buf)
    }

    fn write_data_cmd(&self, cmd: &MmcCommand, bytes: usize, buf: &[u8]) -> Result<(), MmcError> {
        let _slot = self.transfer.lock();
        if self.idmac_wants(bytes) {
            match self.data_cmd_write_idmac(cmd, bytes, buf) {
                Ok(()) => return Ok(()),
                Err(err) => self.degrade_to_pio(cmd, err),
            }
        }
        self.data_cmd_write(cmd, bytes, buf)
    }

    fn current_clock_hz(&self) -> u32 {
        self.current_clock.load(Ordering::Acquire)
    }

    fn max_transfer_bytes(&self) -> usize {
        if self.uses_idmac() {
            self.idmac_max_bytes()
        } else {
            usize::MAX
        }
    }

    fn set_ios(&self, ios: &MmcIos) -> Result<(), MmcError> {
        // A clock or width change while a phase is in flight corrupts it.
        let _slot = self.transfer.lock();
        self.writel(SDMMC_PWREN, u32::from(ios.power_on));
        // Host-side ceiling (Linux `mmc->f_max`): the SD clock source rate
        // is a board fact this driver cannot measure, so the board may
        // declare a lower ceiling than the mode nominally allows.
        let clock_hz = if self.max_card_hz == 0 {
            ios.clock_hz
        } else {
            ios.clock_hz.min(self.max_card_hz)
        };
        self.setup_bus(clock_hz, false)?;
        self.current_clock.store(clock_hz, Ordering::Release);
        let ctype = match ios.bus_width {
            BusWidth::Bit1 => 0,
            _ if self.max_width == BusWidth::Bit1 => 0,
            BusWidth::Bit4 => CTYPE_4BIT,
        };
        self.writel(SDMMC_CTYPE, ctype);
        Ok(())
    }

    fn card_present(&self) -> bool {
        // CDETECT bit n set == card absent (active low).
        self.readl(SDMMC_CDETECT) & 1 == 0
    }

    fn max_bus_width(&self) -> BusWidth {
        self.max_width
    }
}

impl DwMmcHost {
    /// The no-data command path: issue, wait CMD_DONE via RINTSTS
    /// (polled — the line stays masked), decode the response, wait out
    /// R1b busy.
    fn send_cmd_inner(&self, cmd: &MmcCommand) -> Result<MmcResponse, MmcError> {
        let cmdr = self.cmdr_of(cmd, None, 0);
        let wait_busy = cmd.resp == RespKind::R1b;
        self.ciu_cmd(cmdr, cmd.arg, wait_busy)?;
        let mut response = MmcResponse { raw: [0; 4] };
        let mut done = false;
        for _ in spin_budget(CMD_POLL_LIMIT) {
            let pending = self.readl(SDMMC_RINTSTS);
            if pending & INT_ERRORS != 0 {
                let err = self.take_errors(pending);
                return Err(err);
            }
            if pending & INT_CMD_DONE != 0 {
                self.writel(SDMMC_RINTSTS, INT_CMD_DONE);
                response = self.read_response(cmd);
                done = true;
                break;
            }
        }
        if !done {
            return Err(MmcError::Timeout);
        }
        // A completed exchange only means the response arrived: the card
        // still reports refusals inside R1. Without this a declined
        // ACMD6/CMD16/CMD7 looks exactly like a controller fault later on.
        // CMD3 (SEND_RELATIVE_ADDR) answers R6, whose upper half is the new
        // RCA rather than card status, so it is checked only for presence.
        if matches!(cmd.resp, RespKind::R1 | RespKind::R1b)
            && cmd.opcode != 3
            && response.word() & R1_ERROR_BITS != 0
        {
            log::warn!(
                "dwmmc: cmd {} arg {:#x} answered with an error: {}",
                cmd.opcode,
                cmd.arg,
                Self::r1_names(response.word())
            );
        }
        Ok(response)
    }

    /// Read data phase: FIFO-polled drain into `buf` (Linux
    /// `dw_mci_read_data_pio` polling shape).
    fn data_cmd_read(
        &self,
        cmd: &MmcCommand,
        bytes: usize,
        buf: &mut [u8],
    ) -> Result<(), MmcError> {
        if buf.len() < bytes {
            return Err(MmcError::Io);
        }
        self.start_data_phase(cmd, DataDir::Read, bytes)?;
        let mut drained = 0usize;
        for _ in spin_budget(DATA_POLL_LIMIT) {
            let pending = self.readl(SDMMC_RINTSTS);
            if pending & INT_DATA_ERRORS != 0 {
                let err = self.take_errors(pending);
                self.dump_state(&alloc::format!("data-read cmd {}", cmd.opcode), pending);
                self.recover_after_data_error(cmd, pending);
                return Err(err);
            }
            if drained < bytes {
                drained += self.drain_rx(&mut buf[drained..]);
            }
            if pending & INT_DATA_OVER != 0 {
                self.writel(SDMMC_RINTSTS, INT_DATA_OVER);
                if drained < bytes {
                    drained += self.drain_rx(&mut buf[drained..]);
                }
                // Card-side status: RESP0 holds the R1 word of the data
                // command. A clean `tran/ready` answer is the normal case and
                // is only interesting while bringing the path up; a refusal
                // is surfaced through `send_cmd_inner` and the failure dump
                // (`dump_state` prints `resp0`), so this stays at trace to
                // keep one line per 512-byte block out of the console.
                log::trace!(
                    "dwmmc: data resp R1={}",
                    Self::r1_names(self.readl(SDMMC_RESP0))
                );
                break;
            }
        }
        if drained < bytes {
            log::warn!(
                "dwmmc: cmd {} short read {drained}/{bytes} (rintsts {})",
                cmd.opcode,
                Self::rintsts_names(self.readl(SDMMC_RINTSTS)),
            );
            self.dump_state(
                &alloc::format!("short read cmd {}", cmd.opcode),
                self.readl(SDMMC_RINTSTS),
            );
            return Err(MmcError::Io);
        }
        // Auto-stop command-done (ACD) arrives after DATA_OVER on
        // multi-block reads; clear it if present.
        let pending = self.readl(SDMMC_RINTSTS);
        if pending & INT_ACD != 0 {
            self.writel(SDMMC_RINTSTS, INT_ACD);
        }
        Ok(())
    }

    /// Write data phase: FIFO-polled fill from `buf`, then DATA_OVER.
    fn data_cmd_write(&self, cmd: &MmcCommand, bytes: usize, buf: &[u8]) -> Result<(), MmcError> {
        if buf.len() < bytes {
            return Err(MmcError::Io);
        }
        self.start_data_phase(cmd, DataDir::Write, bytes)?;
        let mut filled = 0usize;
        while filled < bytes {
            let pending = self.readl(SDMMC_RINTSTS);
            if pending & INT_DATA_ERRORS != 0 {
                let err = self.take_errors(pending);
                self.dump_state(&alloc::format!("data-write cmd {}", cmd.opcode), pending);
                self.recover_after_data_error(cmd, pending);
                return Err(err);
            }
            let free =
                (self.fifo_depth as usize * self.fifo_width).saturating_sub(self.fifo_bytes());
            let entries = free.min((bytes - filled).div_ceil(self.fifo_width));
            for _ in 0..entries {
                let mut entry = [0u8; 8];
                let take = (bytes - filled).min(self.fifo_width);
                entry[..take].copy_from_slice(&buf[filled..filled + take]);
                self.fifo_write_entry(&entry);
                filled += take;
            }
            if entries == 0 {
                core::hint::spin_loop();
            }
        }
        for _ in spin_budget(DATA_POLL_LIMIT) {
            let pending = self.readl(SDMMC_RINTSTS);
            if pending & INT_DATA_OVER != 0 {
                self.writel(SDMMC_RINTSTS, INT_DATA_OVER);
                return Ok(());
            }
            if pending & INT_DATA_ERRORS != 0 {
                let err = self.take_errors(pending);
                self.dump_state(
                    &alloc::format!("data-write tail cmd {}", cmd.opcode),
                    pending,
                );
                self.recover_after_data_error(cmd, pending);
                return Err(err);
            }
        }
        self.dump_state(
            &alloc::format!("data-write timeout cmd {}", cmd.opcode),
            self.readl(SDMMC_RINTSTS),
        );
        Err(MmcError::Timeout)
    }

    // -----------------------------------------------------------------
    // Internal DMA engine (Linux `dw_mci_idmac_*` + `dw_mci_submit_data`)
    // -----------------------------------------------------------------

    /// Whether IDMAC is still the data path for this slot.
    fn uses_idmac(&self) -> bool {
        self.idmac_ok.load(Ordering::Acquire)
    }

    /// Bytes one IDMAC transfer can carry: the bounce window, capped by what
    /// one descriptor chain can describe.
    fn idmac_max_bytes(&self) -> usize {
        IDMAC_WINDOW_BYTES.min(usize::from(u16::MAX) * 512)
    }

    /// Whether this transfer should go through the engine.
    fn idmac_wants(&self, bytes: usize) -> bool {
        self.uses_idmac() && (IDMAC_MIN_BYTES..=self.idmac_max_bytes()).contains(&bytes)
    }

    /// Program the descriptor chain for `bytes` at `window_bus`, returning
    /// the chain base. Writes go through the raw CPU pointer of the coherent
    /// ring, so the whole ring is re-zeroed per transfer (the engine reads
    /// `des0` before anything else).
    fn idmac_prepare(&self, window_bus: u64, bytes: usize) -> Option<u64> {
        let buffers = self.idmac.as_ref()?;
        let ring_bus = buffers.ring.bus_addr();
        // Descriptors per transfer: 4 KiB each, never crossing a 4 KiB
        // boundary (Linux `dw_mci_idmac_start_dma`).
        let mut count = 0usize;
        let mut offset = 0usize;
        while offset < bytes {
            let addr = window_bus.checked_add(offset as u64)?;
            let boundary = IDMAC_DESC_MAX_BYTES - (addr as usize % IDMAC_DESC_MAX_BYTES);
            offset += (bytes - offset).min(IDMAC_DESC_MAX_BYTES).min(boundary);
            count += 1;
        }
        if count == 0 || count > IDMAC_RING_DESCS {
            return None;
        }

        let ring = buffers.ring.as_ptr().cast::<IdmacDesc>();
        // SAFETY: the ring is a probe-owned coherent allocation of
        // `IDMAC_RING_BYTES`, so zeroing `IDMAC_RING_DESCS` descriptors stays
        // inside it. No other CPU touches the ring while a slot transfer is
        // in flight (one transfer per slot at a time).
        unsafe { core::ptr::write_bytes(ring, 0, IDMAC_RING_DESCS) };
        let mut offset = 0usize;
        for index in 0..count {
            let addr = window_bus + offset as u64;
            let boundary = IDMAC_DESC_MAX_BYTES - (addr as usize % IDMAC_DESC_MAX_BYTES);
            let chunk = (bytes - offset).min(IDMAC_DESC_MAX_BYTES).min(boundary);
            let is_last = index + 1 == count;
            let mut des0 = DESC_OWN;
            if !is_last {
                des0 |= DESC_CH | DESC_DIC;
            }
            if index == 0 {
                des0 |= DESC_FS;
            }
            if is_last {
                des0 |= DESC_LD;
            }
            let next = if is_last {
                0
            } else {
                (ring_bus + ((index + 1) * IDMAC_DESC_SIZE) as u64) as u32
            };
            let desc = IdmacDesc {
                des0,
                des1: chunk as u32,
                des2: addr as u32,
                des3: next,
            };
            // SAFETY: `index < count <= IDMAC_RING_DESCS`, so the slot
            // is inside the ring allocation and `IdmacDesc`-aligned.
            unsafe { core::ptr::write_volatile(ring.wrapping_add(index), desc) };
            offset += chunk;
        }
        // The engine fetches descriptors over the bus: the writes above must
        // be visible before `PLDMND`.
        core::sync::atomic::fence(Ordering::Release);
        Some(ring_bus)
    }

    /// Two-stage engine reset (Linux `dw_mci_idmac_reset`): the `CTRL` pulse
    /// and the `BMOD` software reset, both polled to self-clear. A stale
    /// `SWRESET` holds the engine in reset and silently swallows `PLDMND`.
    fn idmac_reset(&self) {
        self.writel(SDMMC_CTRL, self.readl(SDMMC_CTRL) | CTRL_DMA_RESET);
        for _ in spin_budget(10_000) {
            if self.readl(SDMMC_CTRL) & CTRL_DMA_RESET == 0 {
                break;
            }
        }
        self.writel(SDMMC_BMOD, self.readl(SDMMC_BMOD) | BMOD_SWRESET);
        for _ in spin_budget(10_000) {
            if self.readl(SDMMC_BMOD) & BMOD_SWRESET == 0 {
                break;
            }
        }
    }

    /// Arm the engine for one data phase: statuses, geometry, ring, routing,
    /// then the poll-demand kick that finally starts the descriptor fetch.
    fn idmac_start(&self, window_bus: u64, bytes: usize) -> Option<u64> {
        let chain = self.idmac_prepare(window_bus, bytes)?;
        self.writel(SDMMC_RINTSTS, 0xFFFF_FFFF);
        self.writel(SDMMC_IDSTS, IDMAC_INT_CLEAR);
        self.idmac_reset();
        self.writel(SDMMC_DBADDR, chain as u32);
        // `INT_ENABLE` stays 0 so the interrupt line cannot assert (no handler
        // is registered); `IDINTEN` is still enabled because the completion
        // bits in `IDSTS` do not latch without it.
        let ctrl = self.readl(SDMMC_CTRL) | CTRL_USE_IDMAC | CTRL_DMA_ENABLE;
        self.writel(SDMMC_CTRL, ctrl);
        self.writel(SDMMC_IDINTEN, IDMAC_INT_ENABLE);
        self.writel(SDMMC_BMOD, BMOD_FB | BMOD_ENABLE);
        // Linux `dw_mci_idmac_start_dma`: the poll demand write is what makes
        // the engine fetch the first descriptor. Without it `IDSTS` never
        // moves and the data phase hangs with a clean status.
        self.writel(SDMMC_PLDMND, 1);
        Some(chain)
    }

    /// Stop the engine and take the controller back to PIO routing
    /// (Linux `dw_mci_idmac_stop`).
    fn idmac_stop(&self) {
        let mut bmod = self.readl(SDMMC_BMOD);
        bmod &= !BMOD_ENABLE;
        let mut ctrl = self.readl(SDMMC_CTRL);
        ctrl &= !(CTRL_USE_IDMAC | CTRL_DMA_ENABLE);
        self.writel(SDMMC_BMOD, bmod);
        self.writel(SDMMC_CTRL, ctrl | CTRL_DMA_RESET);
        for _ in spin_budget(10_000) {
            if self.readl(SDMMC_CTRL) & CTRL_DMA_RESET == 0 {
                break;
            }
        }
        self.writel(SDMMC_IDSTS, IDMAC_INT_CLEAR);
    }

    /// Drain a completed read transfer out of the bounce window.
    fn idmac_window_read(&self, bytes: usize, out: &mut [u8]) {
        let Some(buffers) = self.idmac.as_ref() else {
            return;
        };
        // SAFETY: the window is a probe-owned coherent allocation of
        // `IDMAC_WINDOW_BYTES` and `bytes` is bounded by `idmac_max_bytes`.
        let window = unsafe { core::slice::from_raw_parts(buffers.window.as_ptr(), bytes) };
        out[..bytes].copy_from_slice(window);
    }

    /// Stage a write transfer into the bounce window.
    fn idmac_window_write(&self, bytes: usize, input: &[u8]) {
        let Some(buffers) = self.idmac.as_ref() else {
            return;
        };
        // SAFETY: see `idmac_window_read`; the slice is the window and the
        // copy length is bounded by `idmac_max_bytes`.
        let window = unsafe { core::slice::from_raw_parts_mut(buffers.window.as_ptr(), bytes) };
        window.copy_from_slice(&input[..bytes]);
        core::sync::atomic::fence(Ordering::Release);
    }

    /// Give up on the internal DMA engine for this slot: stop it and let the
    /// caller retry the same transfer on the FIFO-polled path. The failure
    /// site already unwedged the card (it knows whether a data phase was
    /// armed); a dead engine must degrade, not fail I/O.
    fn degrade_to_pio(&self, cmd: &MmcCommand, err: MmcError) {
        self.idmac_ok.store(false, Ordering::Release);
        self.idmac_stop();
        log::warn!(
            "dwmmc: IDMAC transfer on cmd {} failed ({err:?}); this slot now uses FIFO-polled PIO",
            cmd.opcode
        );
    }

    /// Poll one IDMAC data phase to completion (polled form of
    /// `dw_mci_idmac_complete_dma`): controller `DATA_OVER`, engine `RI`/`TI`
    /// and both error sets. Errors first — a data error never raises
    /// `DATA_OVER`, and the engine reports its own faults in `IDSTS`.
    fn idmac_wait(&self, cmd: &MmcCommand) -> Result<(), MmcError> {
        // A stalled engine must degrade quickly: the PIO budget is sized for a
        // 512-byte block at 400 kHz, which is far too long to spend waiting
        // for a transfer the engine is never going to complete.
        for _ in spin_budget(IDMAC_POLL_LIMIT) {
            let idsts = self.readl(SDMMC_IDSTS);
            let pending = self.readl(SDMMC_RINTSTS);
            if idsts & IDMAC_INT_ERRORS != 0 || pending & INT_DATA_ERRORS != 0 {
                let err = self.take_errors(pending);
                self.writel(SDMMC_IDSTS, IDMAC_INT_CLEAR);
                self.idmac_dump(if idsts & IDMAC_INT_ERRORS != 0 {
                    "engine error"
                } else {
                    "data error"
                });
                self.dump_state(&alloc::format!("idmac cmd {}", cmd.opcode), pending);
                return Err(err);
            }
            if pending & INT_DATA_OVER != 0 && idsts & (IDMAC_INT_RI | IDMAC_INT_TI) != 0 {
                self.writel(SDMMC_RINTSTS, INT_DATA_OVER);
                self.writel(SDMMC_IDSTS, IDMAC_INT_CLEAR);
                return Ok(());
            }
        }
        self.idmac_dump("timeout");
        self.dump_state(
            &alloc::format!("idmac timeout cmd {}", cmd.opcode),
            self.readl(SDMMC_RINTSTS),
        );
        Err(MmcError::Timeout)
    }

    /// Engine-side picture for a failure: `IDSTS` plus the two progress
    /// pointers. `DSCADDR` still at the ring base means the engine never
    /// fetched a descriptor (kick/reset/routing); a `BUFADDR` inside the
    /// window means it ran and stopped mid-transfer.
    fn idmac_dump(&self, tag: &str) {
        log::warn!(
            "dwmmc: idmac {tag}: idsts={:#x} bmod={:#x} dbaddr={:#x} dscaddr={:#x} bufaddr={:#x}",
            self.readl(SDMMC_IDSTS),
            self.readl(SDMMC_BMOD),
            self.readl(SDMMC_DBADDR),
            self.readl(SDMMC_DSCADDR),
            self.readl(SDMMC_BUFADDR),
        );
    }

    /// IDMAC read phase: arm the engine, issue the command, wait, copy out.
    ///
    /// Ordering is the whole point: the engine is armed *before* the command
    /// goes out, because the card starts streaming as soon as it accepts it
    /// and an engine reset afterwards would drop the first block.
    fn data_cmd_read_idmac(
        &self,
        cmd: &MmcCommand,
        bytes: usize,
        buf: &mut [u8],
    ) -> Result<(), MmcError> {
        if buf.len() < bytes {
            return Err(MmcError::Io);
        }
        let Some(buffers) = self.idmac.as_ref() else {
            return Err(MmcError::Unsupported);
        };
        let window_bus = buffers.window.bus_addr();
        let cmdr = self.prepare_data_phase(cmd, DataDir::Read, bytes, true);
        if self.idmac_start(window_bus, bytes).is_none() {
            // Nothing was asked of the card yet: controller hygiene only.
            self.idmac_stop();
            self.reset_data_path(false);
            return Err(MmcError::Io);
        }
        if let Err(err) = self.issue_data_cmd(cmdr, cmd.arg) {
            self.idmac_stop();
            self.reset_data_path(false);
            return Err(err);
        }
        if let Err(err) = self.idmac_wait(cmd) {
            self.idmac_stop();
            // The authoritative bits are in the dump above; the register is
            // already cleared by `take_errors` at this point.
            self.recover_after_data_error(cmd, self.readl(SDMMC_RINTSTS));
            return Err(err);
        }
        // Coherent memory: `alloc_coherent` maps the window UNCACHED, so the
        // engine's writes are already visible to the CPU.
        self.idmac_window_read(bytes, buf);
        Ok(())
    }

    /// IDMAC write phase: stage into the window, arm, then issue the command.
    fn data_cmd_write_idmac(
        &self,
        cmd: &MmcCommand,
        bytes: usize,
        buf: &[u8],
    ) -> Result<(), MmcError> {
        if buf.len() < bytes {
            return Err(MmcError::Io);
        }
        let Some(buffers) = self.idmac.as_ref() else {
            return Err(MmcError::Unsupported);
        };
        let window_bus = buffers.window.bus_addr();
        // The staged bytes must be in memory before the engine can fetch
        // them, so the copy precedes the poll-demand kick.
        self.idmac_window_write(bytes, buf);
        let cmdr = self.prepare_data_phase(cmd, DataDir::Write, bytes, true);
        if self.idmac_start(window_bus, bytes).is_none() {
            self.idmac_stop();
            self.reset_data_path(false);
            return Err(MmcError::Io);
        }
        if let Err(err) = self.issue_data_cmd(cmdr, cmd.arg) {
            self.idmac_stop();
            self.reset_data_path(false);
            return Err(err);
        }
        if let Err(err) = self.idmac_wait(cmd) {
            self.idmac_stop();
            self.recover_after_data_error(cmd, self.readl(SDMMC_RINTSTS));
            return Err(err);
        }
        Ok(())
    }

    /// Program thresholds and geometry for the upcoming data phase and
    /// return the encoded command **without issuing it**: the PIO path issues
    /// it directly, the IDMAC path has to arm the engine first (the card
    /// starts streaming as soon as the command is accepted).
    fn prepare_data_phase(&self, cmd: &MmcCommand, dir: DataDir, bytes: usize, idmac: bool) -> u32 {
        let blksz: u32 = if bytes.is_multiple_of(512) {
            512
        } else {
            bytes as u32
        };
        // The thresholds follow the mechanism that actually moves *this*
        // transfer, not the slot's capability: `uses_idmac()` is global, while
        // a transfer can still go through the FIFO (below `IDMAC_MIN_BYTES`,
        // or after the engine degraded). PIO runs on the probe-time baseline —
        // the CPU drains by polling `FCNT`, so there is no burst to match and
        // the DMA geometry costs it the known-good configuration.
        self.writel(
            SDMMC_FIFOTH,
            if idmac {
                self.dma_fifoth(blksz)
            } else {
                self.baseline_fifoth()
            },
        );
        self.writel(SDMMC_BLKSIZ, blksz);
        self.writel(SDMMC_BYTCNT, bytes as u32);
        if idmac && self.fifo_off == SDMMC_DATA_240A_OFFSET {
            // 0x100 is the card-read threshold on 2.40a+ (on older IP it is
            // the FIFO port, which `fifo_off` already says). Linux leaves it
            // disabled below HS200/UHS, and a stale bootloader value would
            // gate the DMA read stream.
            self.writel(SDMMC_CDTHRCTL, 0);
        }
        // Phase hygiene: nothing from an earlier phase may be mistaken for
        // this one's completion (`DATA_OVER`) or error set. DAT0 release is
        // handled by PRV_DAT_WAIT on the command itself.
        self.writel(SDMMC_RINTSTS, 0xFFFF_FFFF);
        self.cmdr_of(cmd, Some(dir), bytes)
    }

    /// Issue the data command and wait for the CIU to accept it.
    fn issue_data_cmd(&self, cmdr: u32, arg: u32) -> Result<(), MmcError> {
        self.ciu_cmd(cmdr, arg, true)
    }

    /// PIO data-phase head: threshold/geometry programming plus the command.
    fn start_data_phase(
        &self,
        cmd: &MmcCommand,
        dir: DataDir,
        bytes: usize,
    ) -> Result<(), MmcError> {
        let cmdr = self.prepare_data_phase(cmd, dir, bytes, false);
        self.issue_data_cmd(cmdr, cmd.arg)
    }
}
