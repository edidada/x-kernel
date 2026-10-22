// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DesignWare Mobile Storage Host controller core (Linux
//! `drivers/mmc/host/dw_mmc.c`, the PIO-mode subset verified on
//! RK3588-class boards): register map, clock sequencing, and the
//! FIFO-polled data path the SoC glue ([`crate::dw_mmc_rockchip`])
//! wires up.
//!
//! Data path is **PIO** — the same shape as the board-verified
//! reference driver: the data phase is issued with `DATA_EXPECT` and
//! the transfer drains/fills the FIFO data port, FCNT-gated, until
//! `DATA_OVER`. The IDMAC engine (descriptor chains, coherent rings)
//! is deliberately not used here; enabling it without a fully ported
//! descriptor implementation routes the FIFO to an idle engine and
//! hangs every transfer. Controller completion is observed by polling
//! RINTSTS; the device interrupt stays masked (`INTMASK = 0`).

use core::sync::atomic::{AtomicU32, Ordering};

use driver_base::{CoherentMem, IoMap};
use subsys_mmc::{BusWidth, DataDir, MmcCommand, MmcError, MmcHost, MmcIos, MmcResponse, RespKind};

// ---------------------------------------------------------------------------
// Register map — Linux `drivers/mmc/host/dw_mmc.h`.
// ---------------------------------------------------------------------------
const SDMMC_CTRL: usize = 0x000;
const SDMMC_PWREN: usize = 0x004;
const SDMMC_CLKDIV: usize = 0x008;
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
const SDMMC_VERID: usize = 0x06c;
const SDMMC_CLKSRC: usize = 0x00c;
/// FIFO data port: pre-2.40a at 0x100, 2.40a+ at 0x200 (Linux
/// `DATA_OFFSET` / `DATA_240A_OFFSET`).
const SDMMC_DATA_OFFSET: usize = 0x100;
const SDMMC_DATA_240A_OFFSET: usize = 0x200;
const SDMMC_BMOD: usize = 0x080;
const SDMMC_PLDMND: usize = 0x084;
const SDMMC_DBADDR: usize = 0x088;
const SDMMC_IDSTS: usize = 0x08c;
const SDMMC_IDINTEN: usize = 0x090;

// CTRL bits.
const CTRL_INT_ENABLE: u32 = 1 << 4;
const CTRL_DMA_ENABLE: u32 = 1 << 5;
const CTRL_DMA_RESET: u32 = 1 << 2;
const CTRL_FIFO_RESET: u32 = 1 << 1;
const CTRL_RESET: u32 = 1 << 0;
const CTRL_USE_IDMAC: u32 = 1 << 25;
const CTRL_ALL_RESET: u32 = CTRL_RESET | CTRL_FIFO_RESET | CTRL_DMA_RESET;

// CTYPE bits.
const CTYPE_4BIT: u32 = 1 << 0;

// INT/RINTSTS bits.
const INT_RESP_ERR: u32 = 1 << 1;
const INT_CMD_DONE: u32 = 1 << 2;
const INT_DATA_OVER: u32 = 1 << 3;
const INT_TXDR: u32 = 1 << 4;
const INT_RXDR: u32 = 1 << 5;
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
/// Controller error vocabulary this driver maps onto [`MmcError`].
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

// CMD bits.
const CMD_START: u32 = 1 << 31;
const CMD_USE_HOLD_REG: u32 = 1 << 29;
const CMD_UPD_CLK: u32 = 1 << 21;
const CMD_INIT: u32 = 1 << 15;
const CMD_SEND_AUTO_STOP: u32 = 1 << 12;
const CMD_PRV_DAT_WAIT: u32 = 1 << 13;
const CMD_DAT_WR: u32 = 1 << 10;
const CMD_DAT_EXP: u32 = 1 << 9;
const CMD_RESP_CRC: u32 = 1 << 8;
const CMD_RESP_LONG: u32 = 1 << 7;
const CMD_RESP_EXP: u32 = 1 << 6;
const CMD_INDX_MASK: u32 = 0x3F;

// CLKENA bits.
const CLKEN_ENABLE: u32 = 1 << 0;

/// STATUS register card-busy indicator (DAT0 driven low).
const STATUS_CARD_BUSY: u32 = 1 << 9;
/// STATUS FIFO count field [29:17] (entries).
const STATUS_FCNT_SHIFT: u32 = 17;

// FIFOTH layout: MSIZE [30:28], RX_WMARK [27:16], TX_WMARK [11:0].
const fn fifoth(msize: u32, rx_wmark: u32, tx_wmark: u32) -> u32 {
    ((msize & 0x7) << 28) | ((rx_wmark & 0xFFF) << 16) | (tx_wmark & 0xFFF)
}

/// Command-issue poll budget (START clear + clock-inform frames).
const CMD_POLL_LIMIT: u32 = 1 << 21;
/// Data-transfer poll budget for non-task context.
const DATA_POLL_LIMIT: u32 = 1 << 28;

/// Bounded poll iterator with a spin hint (bring-up-grade waits; a few
/// ms at any sane clock).
fn spin_budget(limit: u32) -> impl Iterator<Item = u32> {
    (0..limit).inspect(|_| core::hint::spin_loop())
}

// ---------------------------------------------------------------------------
// Internal DMA (IDMAC) — Linux `dw_mci_idmac_*`, shape verified against
// the board-running tgoskits dwmmc-host implementation.
// ---------------------------------------------------------------------------
// IDMAC descriptor bits (DES0).
const DESC_OWN: u32 = 1 << 31;
const DESC_LD: u32 = 1 << 2;
const DESC_FS: u32 = 1 << 3;
const DESC_CH: u32 = 1 << 4;
const DESC_DIC: u32 = 1 << 1;
const DESC_ER: u32 = 1 << 5;
// IDMAC interrupt bits (IDSTS).
const IDINT_TI: u32 = 1 << 0;
const IDINT_RI: u32 = 1 << 1;
const IDINT_FBE: u32 = 1 << 2;
const IDINT_DU: u32 = 1 << 4;
const IDINT_CES: u32 = 1 << 5;
const IDINT_NI: u32 = 1 << 8;
const IDINT_AI: u32 = 1 << 9;
const IDINT_ERRORS: u32 = IDINT_FBE | IDINT_DU | IDINT_CES | IDINT_AI;
const IDINT_CLR_ALL: u32 =
    IDINT_AI | IDINT_NI | IDINT_CES | IDINT_DU | IDINT_FBE | IDINT_RI | IDINT_TI;
// BMOD bits.
const IDMAC_ENABLE: u32 = 1 << 7;
const IDMAC_FB: u32 = 1 << 1;
const IDMAC_SWRESET: u32 = 1 << 0;

/// Per-descriptor data payload cap (Linux `DW_MCI_DESC_DATA_SIZE`).
const DESC_MAX_BYTES: usize = 0x1000;
/// Descriptor ring: one 4 KiB coherent page of 16 entries.
pub const RING_PAGE: usize = 4096;
const RING_DESC_COUNT: usize = RING_PAGE / 16;
/// The data bounce window: the largest single transfer this host accepts.
pub const DMA_WINDOW_BYTES: usize = DESC_MAX_BYTES * RING_DESC_COUNT;
/// IDMAC descriptors and buffers must stay below the 4 GiB line (the
/// 32-bit descriptor address fields).
const IDMAC_ADDR_LIMIT: u64 = 1 << 32;
/// A descriptor's data may not straddle a 4 KiB boundary (verified
/// driver splits chunks on it — DesignWare internal-DMA behavior).
const DESC_BOUNDARY_BYTES: u64 = 4096;

/// One IDMAC descriptor (Linux `struct idmac_desc`, 32-bit addressing).
#[repr(C)]
struct IdmacDesc {
    des0: u32,
    des1: u32,
    /// Buffer bus address.
    des2: u32,
    /// Next-descriptor bus address (ring-linked).
    des3: u32,
}

impl IdmacDesc {
    const fn zeroed() -> Self {
        Self {
            des0: 0,
            des1: 0,
            des2: 0,
            des3: 0,
        }
    }
}

/// The DW MMC host slot state (Linux `struct dw_mci`).
pub struct DwMmcHost {
    /// Mapped register block (probe-scoped `IoMap`).
    regs: IoMap,
    /// CIU input clock in Hz (board fact — see the rockchip glue).
    ciu_hz: u32,
    /// FIFO depth in 32-bit entries (DT `fifo-depth`).
    fifo_depth: u32,
    /// Host slot index (`mmc_host->index`).
    slot: u8,
    /// Offset of the FIFO data register (VERID-dependent).
    fifo_off: usize,
    /// Descriptor ring (coherent, owned — card consumers hold Arc clones
    /// of this host and may outlive the probe Data).
    ring: CoherentMem,
    /// Data bounce window (coherent, owned — see `ring`).
    window: CoherentMem,
    /// Transfer completion status slot (IRQ → waiter).
    xfer: subsys_mmc::xfer::XferStatus,
    /// Current configured bus clock (0 = gated).
    current_clock: AtomicU32,
}

// SAFETY: `regs` is a probe-owned MMIO mapping; all accessors go through
// `&self` with only lock-free atomics shared across contexts.
unsafe impl Sync for DwMmcHost {}
// SAFETY: no thread-local state; see the `Sync` note.
unsafe impl Send for DwMmcHost {}

impl DwMmcHost {
    /// Controller bring-up (Linux `dw_mci_probe`, PIO subset): power,
    /// all-reset, interrupt clear + full mask, timeouts, IDMAC routing
    /// disabled, FIFO thresholds, VERID-dependent FIFO port.
    pub fn new(
        regs: IoMap,
        ciu_hz: u32,
        fifo_depth: u32,
        slot: u8,
        ring: CoherentMem,
        window: CoherentMem,
        signal: alloc::sync::Arc<dyn subsys_mmc::XferSignal>,
    ) -> Result<Self, MmcError> {
        if ciu_hz == 0 || fifo_depth < 2 {
            return Err(MmcError::Unsupported);
        }
        if ring.len() < RING_PAGE || window.len() < 4096 {
            return Err(MmcError::Unsupported);
        }
        // 32-bit descriptor/buffer addresses (see IDMAC_ADDR_LIMIT).
        let top = u64::max(ring.bus_addr(), window.bus_addr()).saturating_add(4096);
        if top >= IDMAC_ADDR_LIMIT {
            return Err(MmcError::Unsupported);
        }
        let host = Self {
            regs,
            ciu_hz,
            fifo_depth,
            slot,
            fifo_off: SDMMC_DATA_240A_OFFSET,
            ring,
            window,
            xfer: subsys_mmc::xfer::XferStatus::new(signal),
            current_clock: AtomicU32::new(0),
        };

        // Power on the card BEFORE reset (U-Boot `dwmci_init` order):
        // PWREN gates the card's vmmc rail on switchable boards.
        host.writel(SDMMC_PWREN, 1);
        // Global + FIFO + DMA reset, each bit self-clearing.
        host.writel(SDMMC_CTRL, CTRL_ALL_RESET);
        for _ in spin_budget(10_000) {
            if host.readl(SDMMC_CTRL) & CTRL_ALL_RESET == 0 {
                break;
            }
        }
        // Clear all raw status; the data-completion interrupt line is
        // armed per transfer (see start_data_phase).
        host.writel(SDMMC_RINTSTS, 0xFFFF_FFFF);
        host.writel(SDMMC_INTMASK, 0);
        host.writel(SDMMC_TMOUT, 0xFFFF_FFFF);
        // FIFO burst marks: burst 8, RX at half-minus-1, TX at half.
        let half = host.fifo_depth / 2;
        host.writel(SDMMC_FIFOTH, fifoth(0x2, half - 1, half));
        // Gate CIU clock before the first setup_bus.
        host.writel(SDMMC_CLKENA, 0);
        host.writel(SDMMC_CLKSRC, 0);

        // IDMAC: reset the engine, link the descriptor ring, program the
        // base address. Per-transfer engine setup (CTRL.IDMAC routing,
        // IDINTEN, BMOD, PLDMND) happens in start_data_phase — the
        // CTRL.DMA_RESET/BMOD.SWR pulses above and below clear those
        // registers, so they cannot be one-time init.
        host.reset_dma_engine();
        host.reset_ring();
        host.writel(SDMMC_IDSTS, IDINT_CLR_ALL);
        host.writel(SDMMC_DBADDR, host.ring_base());
        Ok(host)
    }

    /// Two-stage DMA engine reset (verified driver `reset_dma_engine`):
    /// CTRL.DMA_RESET polled to self-clear, then BMOD.SWR polled to
    /// self-clear.
    fn reset_dma_engine(&self) {
        self.writel(SDMMC_CTRL, self.readl(SDMMC_CTRL) | CTRL_DMA_RESET);
        for _ in spin_budget(10_000) {
            if self.readl(SDMMC_CTRL) & CTRL_DMA_RESET == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        self.writel(SDMMC_BMOD, self.readl(SDMMC_BMOD) | IDMAC_SWRESET);
        for _ in spin_budget(10_000) {
            if self.readl(SDMMC_BMOD) & IDMAC_SWRESET == 0 {
                return;
            }
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

    /// One 32-bit read of the FIFO data port.
    fn fifo_read32(&self) -> u32 {
        self.readl(self.fifo_off)
    }

    /// One 32-bit write of the FIFO data port.
    fn fifo_write32(&self, value: u32) {
        self.writel(self.fifo_off, value)
    }

    /// Bytes currently held in the FIFO.
    fn fifo_bytes(&self) -> usize {
        let fcnt = (self.readl(SDMMC_STATUS) >> STATUS_FCNT_SHIFT) & 0x1FFF;
        (fcnt as usize) * 4
    }

    /// Issue one command frame and poll the START bit (the controller
    /// clears it when the CIU accepts the command — Linux `mci_send_cmd`).
    fn ciu_cmd(&self, cmdr: u32, arg: u32) -> Result<(), MmcError> {
        self.writel(SDMMC_CMDARG, arg);
        self.writel(SDMMC_CMD, cmdr | CMD_START);
        for _ in spin_budget(CMD_POLL_LIMIT) {
            if self.readl(SDMMC_CMD) & CMD_START == 0 {
                return Ok(());
            }
        }
        Err(MmcError::Timeout)
    }

    /// Clock sequencing (Linux `dw_mci_setup_bus`): gate → inform CIU →
    /// divider → inform CIU → enable → inform CIU.
    fn setup_bus(&self, clock_hz: u32, force: bool) -> Result<(), MmcError> {
        if clock_hz == self.current_clock.load(Ordering::Acquire) && !force {
            return Ok(());
        }
        let div: u32 = match clock_hz {
            0 => 0,
            _ if self.ciu_hz == clock_hz => 0,
            _ => {
                let mut div = self.ciu_hz / clock_hz;
                if !self.ciu_hz.is_multiple_of(clock_hz) && self.ciu_hz > clock_hz {
                    div += 1;
                }
                div.div_ceil(2)
            }
        };
        let inform = CMD_UPD_CLK | CMD_PRV_DAT_WAIT;
        self.writel(SDMMC_CLKENA, 0);
        self.ciu_cmd(inform, 0)?;
        self.writel(SDMMC_CLKDIV, div);
        self.ciu_cmd(inform, 0)?;
        self.writel(SDMMC_CLKENA, CLKEN_ENABLE);
        self.ciu_cmd(inform, 0)?;
        Ok(())
    }

    /// Command register encoding (Linux `dw_mci_prepare_command`).
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
            if cmd.auto_stop && bytes > 512 {
                cmdr |= CMD_SEND_AUTO_STOP;
            }
        }
        if cmd.init_clocks {
            cmdr |= CMD_INIT;
        }
        // Hold register on every command (the verified driver defaults it
        // on; RK3588 does not declare NO_USE_HOLD).
        cmdr |= CMD_USE_HOLD_REG;
        cmdr
    }

    /// Read the response registers (Linux `dw_mci_get_resp`): RESP0 holds
    /// the most-significant word `{127:96}`, matching the raw layout
    /// [`MmcResponse::bits`] indexes.
    fn read_response(&self, cmd: &MmcCommand) -> MmcResponse {
        match cmd.resp {
            RespKind::None => MmcResponse { raw: [0; 4] },
            RespKind::R2 => MmcResponse {
                raw: [
                    self.readl(SDMMC_RESP0),
                    self.readl(SDMMC_RESP0 + 4),
                    self.readl(SDMMC_RESP0 + 8),
                    self.readl(SDMMC_RESP0 + 0xC),
                ],
            },
            RespKind::R3 | RespKind::R1 | RespKind::R1b | RespKind::ShortNoCrc => MmcResponse {
                raw: [self.readl(SDMMC_RESP0), 0, 0, 0],
            },
        }
    }

    /// Map a published status word (already known not to be DONE) onto
    /// the error vocabulary.
    fn status_error(status: u32) -> MmcError {
        if status & (INT_RTO | INT_RCRC | INT_RESP_ERR) != 0 {
            MmcError::Crc
        } else if status & (INT_DRTO | INT_HTO) != 0 {
            MmcError::Timeout
        } else {
            MmcError::Io
        }
    }

    /// Drain whatever the FIFO holds into `buf` (FCNT-bounded), Linux
    /// `dw_mci_read_data_pio` polling shape. Returns bytes drained.
    fn drain_rx(&self, out: &mut [u8]) -> usize {
        let available = self.fifo_bytes();
        let n = available.min(out.len()) & !0x3;
        for i in (0..n).step_by(4) {
            let word = self.fifo_read32();
            out[i..i + 4].copy_from_slice(&word.to_le_bytes());
        }
        n
    }

    /// Two-stage DMA reset (see `reset_dma_engine`), then re-link and
    /// program the descriptor chain over the bounce window for `bytes`.
    fn prepare_idmac(&self, bytes: usize) {
        self.reset_dma_engine();
        self.reset_ring();
        self.arm_descriptors(bytes);
        self.writel(SDMMC_IDSTS, IDINT_CLR_ALL);
        self.writel(SDMMC_DBADDR, self.ring_base());
    }

    /// Stage `bytes` into the coherent bounce window (write direction).
    fn stage_out(&self, bytes: usize, buf: &[u8]) {
        // SAFETY: the window is a coherent, host-owned allocation of at
        // least `bytes` (bounded by the caller's window check).
        let window = unsafe { core::slice::from_raw_parts_mut(self.window.as_ptr(), bytes) };
        window.copy_from_slice(&buf[..bytes]);
    }

    /// Collect `bytes` from the coherent bounce window (read direction).
    fn stage_in(&self, bytes: usize, buf: &mut [u8]) {
        // SAFETY: same bounds as `stage_out`.
        let window = unsafe { core::slice::from_raw_parts_mut(self.window.as_ptr(), bytes) };
        buf[..bytes].copy_from_slice(window);
    }

    /// IDMAC data phase (verified driver `submit_idmac_transfer_mapped`
    /// shape): engine reset → descriptors → status clear → geometry →
    /// fence → DBADDR → CTRL routing → IDINTEN → BMOD → PLDMND → command.
    fn start_data_phase(
        &self,
        cmd: &MmcCommand,
        dir: DataDir,
        bytes: usize,
    ) -> Result<(), MmcError> {
        self.arm_irq_masks();
        self.prepare_idmac(bytes);
        let blksz: u32 = if bytes.is_multiple_of(512) {
            512
        } else {
            bytes as u32
        };
        self.writel(SDMMC_BLKSIZ, blksz);
        self.writel(SDMMC_BYTCNT, bytes as u32);
        self.xfer.reset();

        // Fence: the descriptor chain must be visible to the DMA engine
        // before it is enabled.
        karch::dsb_ishst();
        self.writel(SDMMC_DBADDR, self.ring_base());
        let ctrl = self.readl(SDMMC_CTRL) | CTRL_USE_IDMAC | CTRL_DMA_ENABLE | CTRL_INT_ENABLE;
        self.writel(SDMMC_CTRL, ctrl);
        self.writel(SDMMC_IDINTEN, IDINT_NI | IDINT_RI | IDINT_TI | IDINT_ERRORS);
        self.writel(SDMMC_BMOD, IDMAC_FB | IDMAC_ENABLE);
        // Poll demand: nudge the engine to start fetching the chain.
        self.writel(SDMMC_PLDMND, 1);

        let cmdr = self.cmdr_of(cmd, Some(dir), bytes);
        self.ciu_cmd(cmdr, cmd.arg)
    }

    /// The completion predicate + fallible state read, carrying the
    /// published status word for failure diagnostics.
    fn xfer_result(&self) -> Option<Result<u32, MmcError>> {
        let status = self.xfer.load();
        if status == 0 {
            return None;
        }
        Some(if status == subsys_mmc::xfer::XFER_DONE {
            Ok(status)
        } else {
            Err(Self::status_error(status))
        })
    }

    /// Wait for a started data transfer: park via the signal when this is
    /// a task, fall back to a bounded poll otherwise (probe context).
    /// Returns the published status word on completion (XFER_DONE or the
    /// raw error bitset) and Timeout when nothing fired within budget.
    fn wait_transfer(&self) -> Result<u32, MmcError> {
        let signal = self.xfer.signal().clone();
        let parked = signal.wait_until(&mut || self.xfer_result().is_some());
        if parked.is_err() {
            for _ in spin_budget(DATA_POLL_LIMIT) {
                if self.xfer_result().is_some() {
                    break;
                }
            }
        }
        match self.xfer_result() {
            Some(Ok(status)) => Ok(status),
            Some(Err(err)) => Err(err),
            None => Err(MmcError::Timeout),
        }
    }

    /// The IRQ hook: sample, clear, publish, wake. Handler context —
    /// register reads and lock-free atomics only.
    pub fn on_irq(&self) -> bool {
        let rintsts = self.readl(SDMMC_RINTSTS);
        let interesting = rintsts & (INT_ERRORS | INT_DATA_OVER | INT_ACD);
        if interesting == 0 {
            return false;
        }
        self.writel(SDMMC_RINTSTS, interesting);
        let idsts = self.readl(SDMMC_IDSTS);
        if idsts & (IDINT_RI | IDINT_TI | IDINT_ERRORS) != 0 {
            self.writel(SDMMC_IDSTS, idsts & IDINT_CLR_ALL);
        }
        let id_error = idsts & IDINT_ERRORS;
        if interesting & INT_ERRORS != 0 || id_error != 0 {
            self.xfer.publish(interesting | id_error);
        } else {
            self.xfer.publish(subsys_mmc::xfer::XFER_DONE);
        }
        true
    }

    /// Unmask the data-completion interrupts (called once, before the
    /// first parked transfer — block I/O).
    pub fn arm_irq_masks(&self) {
        self.writel(SDMMC_INTMASK, INT_DATA_OVER | INT_ACD | INT_ERRORS);
    }

    /// Boot-time data-integrity probe: read block 0 with the interrupt
    /// line temporarily masked so the polling loop exclusively owns
    /// RINTSTS (an armed line lets the handler clear bits out from under
    /// the poll — the exact failure this closes). Safe in any context.
    pub fn polled_lba0_head(&self) -> Option<[u8; 32]> {
        self.writel(SDMMC_INTMASK, 0);
        let result = (|| {
            let mut block = [0u8; 512];
            let cmd = MmcCommand::with(17, 0, RespKind::R1);
            let () = self.data_cmd_read(&cmd, 512, &mut block).ok()?;
            let mut head = [0u8; 32];
            head.copy_from_slice(&block[..32]);
            Some(head)
        })();
        self.arm_irq_masks();
        let head = result?;
        let hex: alloc::vec::Vec<alloc::string::String> =
            head.iter().map(|b| alloc::format!("{b:02x}")).collect();
        log::info!("dwmmc: LBA0 head: {}", hex.join(" "));
        Some(head)
    }

    /// Read data phase: IDMAC into the bounce window, then collect into
    /// `buf`.
    fn data_cmd_read(
        &self,
        cmd: &MmcCommand,
        bytes: usize,
        buf: &mut [u8],
    ) -> Result<(), MmcError> {
        if bytes > DMA_WINDOW_BYTES || buf.len() < bytes {
            return Err(MmcError::Io);
        }
        self.start_data_phase(cmd, DataDir::Read, bytes)?;
        let outcome = self.wait_transfer();
        self.idmac_stop();
        if let Err(err) = &outcome {
            log::warn!(
                "dwmmc: data-read cmd {} bytes {} failed: {err:?} status={:#x} (rintsts {:#x} \
                 idsts {:#x})",
                cmd.opcode,
                bytes,
                self.xfer.load(),
                self.readl(SDMMC_RINTSTS),
                self.readl(SDMMC_IDSTS),
            );
        }
        outcome?;
        self.stage_in(bytes, buf);
        Ok(())
    }

    /// Write data phase: stage `buf` through the bounce window out to the
    /// card.
    fn data_cmd_write(&self, cmd: &MmcCommand, bytes: usize, buf: &[u8]) -> Result<(), MmcError> {
        if bytes > DMA_WINDOW_BYTES || buf.len() < bytes {
            return Err(MmcError::Io);
        }
        self.stage_out(bytes, buf);
        self.start_data_phase(cmd, DataDir::Write, bytes)?;
        let outcome = self.wait_transfer();
        self.idmac_stop();
        if let Err(err) = &outcome {
            log::warn!(
                "dwmmc: data-write cmd {} bytes {} failed: {err:?} status={:#x} (rintsts {:#x} \
                 idsts {:#x})",
                cmd.opcode,
                bytes,
                self.xfer.load(),
                self.readl(SDMMC_RINTSTS),
                self.readl(SDMMC_IDSTS),
            );
        }
        outcome.map(|_| ())
    }

    /// Stop and reset the DMA engine after a transfer (verified driver
    /// `disable_idmac` shape).
    fn idmac_stop(&self) {
        let ctrl = self.readl(SDMMC_CTRL) & !(CTRL_USE_IDMAC | CTRL_DMA_ENABLE);
        self.writel(SDMMC_CTRL, ctrl);
        self.writel(SDMMC_BMOD, 0);
    }

    /// The no-data command path: issue, wait CMD_DONE via RINTSTS
    /// (polled — the INTMASK only routes data bits to the line), decode
    /// the response, wait out R1b busy.
    fn send_cmd_inner(&self, cmd: &MmcCommand) -> Result<MmcResponse, MmcError> {
        let cmdr = self.cmdr_of(cmd, None, 0);
        self.ciu_cmd(cmdr, cmd.arg)?;
        let mut response = MmcResponse { raw: [0; 4] };
        let mut done = false;
        for _ in spin_budget(CMD_POLL_LIMIT) {
            let pending = self.readl(SDMMC_RINTSTS);
            if pending & INT_ERRORS != 0 {
                self.writel(SDMMC_RINTSTS, pending & INT_ERRORS);
                return Err(if pending & (INT_RTO | INT_RCRC | INT_RESP_ERR) != 0 {
                    MmcError::Crc
                } else {
                    MmcError::Io
                });
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
        // R1b: wait out the card-side busy (DAT0 low) with a budget.
        if cmd.resp == RespKind::R1b {
            for _ in spin_budget(CMD_POLL_LIMIT) {
                if self.readl(SDMMC_STATUS) & STATUS_CARD_BUSY == 0 {
                    break;
                }
            }
        }
        Ok(response)
    }

    /// The ring base as the 32-bit descriptor address.
    fn ring_base(&self) -> u32 {
        u32::try_from(self.ring.bus_addr()).unwrap_or(0)
    }

    /// Relink the descriptor ring: entries chained head→tail, the last
    /// flagged end-of-ring pointing back at the head.
    fn reset_ring(&self) {
        // SAFETY: `ring` is a coherent, host-owned `RING_PAGE` page
        // holding exactly `RING_DESC_COUNT` descriptors.
        let descs = unsafe {
            core::slice::from_raw_parts_mut(self.ring.as_ptr() as *mut IdmacDesc, RING_DESC_COUNT)
        };
        let base = self.ring.bus_addr();
        let stride = 16u64;
        for (i, desc) in descs.iter_mut().enumerate() {
            *desc = IdmacDesc::zeroed();
            desc.des3 = u32::try_from(base + stride * (i as u64 + 1)).unwrap_or(0);
        }
        if let Some(last) = descs.last_mut() {
            last.des3 = u32::try_from(base).unwrap_or(0);
            last.des0 = DESC_ER;
        }
    }

    /// Program the descriptor chain for `bytes` over the bounce window,
    /// splitting chunks on 4 KiB boundaries (verified driver
    /// `prepare_idmac_descriptors`). Non-final descriptors carry
    /// CH | DIC; the final one carries FD | LD.
    fn arm_descriptors(&self, bytes: usize) {
        // SAFETY: same coherent page discipline as `reset_ring`.
        let descs = unsafe {
            core::slice::from_raw_parts_mut(self.ring.as_ptr() as *mut IdmacDesc, RING_DESC_COUNT)
        };
        let window_base = self.window.bus_addr();
        let mut remaining = bytes;
        let mut offset = 0usize;
        let mut index = 0usize;
        while remaining > 0 && index < RING_DESC_COUNT {
            let dma_addr = window_base + offset as u64;
            let boundary_left = (DESC_BOUNDARY_BYTES - dma_addr % DESC_BOUNDARY_BYTES) as usize;
            let chunk = remaining.min(DESC_MAX_BYTES).min(boundary_left);
            let first = index == 0;
            let last = remaining == chunk;
            let mut des0 = DESC_OWN;
            if !last {
                des0 |= DESC_CH | DESC_DIC;
            }
            if first {
                des0 |= DESC_FS;
            }
            if last {
                des0 |= DESC_LD;
            }
            let d = &mut descs[index];
            d.des0 = des0;
            d.des1 = chunk as u32 & 0x1FFF;
            d.des2 = u32::try_from(dma_addr).unwrap_or(0);
            index += 1;
            remaining -= chunk;
            offset += chunk;
        }
        // Terminate: the last used descriptor ends the chain (LD already
        // set); restore the end-of-ring marker on the next entry.
        if index < RING_DESC_COUNT {
            let d = &mut descs[index];
            d.des0 = DESC_ER;
        }
        self.writel(SDMMC_DBADDR, self.ring_base());
    }
}

impl MmcHost for DwMmcHost {
    fn send_cmd(&self, cmd: &MmcCommand) -> Result<MmcResponse, MmcError> {
        // Fallback diagnostics: EVERY command failure names itself here,
        // so a silent protocol-layer failure can never hide behind a
        // missing step() wrapper.
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
        self.data_cmd_read(cmd, bytes, buf)
    }

    fn write_data_cmd(&self, cmd: &MmcCommand, bytes: usize, buf: &[u8]) -> Result<(), MmcError> {
        self.data_cmd_write(cmd, bytes, buf)
    }

    fn set_ios(&self, ios: &MmcIos) -> Result<(), MmcError> {
        // Power (Linux `__dw_mci_set_ios` PWREN handling).
        self.writel(SDMMC_PWREN, u32::from(ios.power_on));
        self.setup_bus(ios.clock_hz, false)?;
        self.current_clock.store(ios.clock_hz, Ordering::Release);
        let ctype = match ios.bus_width {
            BusWidth::Bit1 => 0,
            BusWidth::Bit4 => CTYPE_4BIT,
        };
        self.writel(SDMMC_CTYPE, ctype);
        Ok(())
    }

    fn card_present(&self) -> bool {
        // CDETECT bit n set == card absent (active low).
        self.readl(SDMMC_CDETECT) & 1 == 0
    }
}
