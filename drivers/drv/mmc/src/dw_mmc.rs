// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DesignWare Mobile Storage Host controller core (Linux
//! `drivers/mmc/host/dw_mmc.c`): register map, clock sequencing, the
//! internal DMA (IDMAC) descriptor engine, and the completion signaling
//! the SoC glue ([`crate::dw_mmc_rockchip`]) wires up.
//!
//! Data path: a coherent bounce window + 16-descriptor IDMAC ring
//! (≤ 64 KiB per transfer, chunked by the descriptor chain). Callers'
//! buffers are copied through the window rather than DMA'd directly —
//! kernel-heap virtual addresses have no device-visible bus address
//! without a virt→phys walk, and a coherent window keeps the first slice
//! free of cache-maintenance subtleties. The 32-bit IDMAC addressing mode
//! bounds ring/window bus addresses below 4 GiB; the constructor refuses
//! anything else (64-bit-descriptor mode is a follow-up).
//!
//! Completion: the IRQ handler publishes a status word into
//! [`XferStatus`] and wakes; task-context callers park through the
//! [`XferSignal`], probe-context callers (SCR/switch reads during card
//! detection) fall back to a bounded poll of the same slot.

use alloc::sync::Arc;
use core::{
    mem::size_of,
    sync::atomic::{AtomicU32, Ordering},
};

use driver_base::{CoherentMem, IoMap};
use subsys_mmc::{
    BusWidth, DataDir, MmcCommand, MmcError, MmcHost, MmcIos, MmcResponse, RespKind,
    xfer::{XFER_DONE, XferStatus},
};

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
const SDMMC_BMOD: usize = 0x080;
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
const CMD_STOP: u32 = 1 << 14;
const CMD_PRV_DAT_WAIT: u32 = 1 << 13;
const CMD_DAT_WR: u32 = 1 << 10;
const CMD_DAT_EXP: u32 = 1 << 9;
const CMD_RESP_CRC: u32 = 1 << 8;
const CMD_RESP_LONG: u32 = 1 << 7;
const CMD_RESP_EXP: u32 = 1 << 6;
/// The CMD register index field is bits [5:0] — six bits, not five.
/// Masking with 0x1F (a macro in Linux dw_mmc.h that the mainline driver
/// itself never applies: dw_mci_prepare_command uses `cmdr = cmd->opcode`
/// verbatim) silently rewrites every opcode >= 32: ACMD41 becomes CMD9,
/// ACMD55 becomes CMD23 — cards then never report busy and detection
/// times out in the ACMD41 poll.
const CMD_INDX_MASK: u32 = 0x3F;

// CLKENA bits.
const CLKEN_ENABLE: u32 = 1 << 0;

/// STATUS register card-busy indicator (DAT0 driven low).
const STATUS_CARD_BUSY: u32 = 1 << 9;

// FIFOTH layout: MSIZE [30:28], RX_WMARK [27:16], TX_WMARK [11:0].
const fn fifoth(msize: u32, rx_wmark: u32, tx_wmark: u32) -> u32 {
    ((msize & 0x7) << 28) | ((rx_wmark & 0xFFF) << 16) | (tx_wmark & 0xFFF)
}

// IDMAC descriptor bits (DES0).
const DES0_OWN: u32 = 1 << 31;
const DES0_LD: u32 = 1 << 2;
const DES0_FD: u32 = 1 << 3;
const DES0_ER: u32 = 1 << 5;
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
pub const DESC_DATA_SIZE: usize = 0x1000;
/// Descriptor ring: one 4 KiB coherent page of 16 entries.
pub const RING_PAGE: usize = 4096;
pub(crate) const RING_DESCRIPTORS: usize = RING_PAGE / size_of::<IdmacDesc>();
/// The data bounce window: the largest single transfer this host accepts.
pub const DMA_WINDOW_BYTES: usize = DESC_DATA_SIZE * RING_DESCRIPTORS;
/// Response/command timeout in controller poll iterations (bring-up-grade
/// bounded spinning; a few ms at any sane clock).
const CMD_POLL_LIMIT: u32 = 1 << 21;
/// Data-transfer poll limit for non-task context — one full 64 KiB
/// window at 400 kHz worst case needs seconds; the bound covers it.
const DATA_POLL_LIMIT: u32 = 1 << 28;
/// The 32-bit IDMAC addressing mode cannot reach above 4 GiB.
const IDMAC_ADDR_LIMIT: u64 = 1 << 32;

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

/// Poll iterator with a spin hint; the bring-up-grade wait primitive.
fn spin_budget(limit: u32) -> impl Iterator<Item = u32> {
    (0..limit).inspect(|_| core::hint::spin_loop())
}

/// Byte slice over a coherent allocation (window staging copies).
///
/// # Safety
///
/// `ptr` must point at a live coherent allocation of at least `len`
/// bytes, and the resulting slice must not alias another live borrow of
/// the same allocation.
unsafe fn coherent_slice<'a>(ptr: *mut u8, len: usize) -> &'a mut [u8] {
    // SAFETY: the caller guarantees the allocation bounds and uniqueness
    // of this borrow; the memory is plain DRAM behind a coherent mapping.
    unsafe { core::slice::from_raw_parts_mut(ptr, len) }
}

/// The DW MMC host slot state (Linux `struct dw_mci`).
pub struct DwMmcHost {
    /// Mapped register block (probe-scoped `IoMap`).
    regs: IoMap,
    /// CIU input clock in Hz (from the clk framework).
    ciu_hz: u32,
    /// FIFO depth in 32-bit entries (DT `fifo-depth`).
    fifo_depth: u32,
    /// Host slot index (`mmc_host->index`).
    slot: u8,
    /// Descriptor ring (coherent): OWNED by the host, because card
    /// consumers hold `Arc` clones that can outlive the probe Data — the
    /// mappings must live exactly as long as the last host reference.
    ring: CoherentMem,
    /// Data bounce window (coherent, owned — see `ring`).
    window: CoherentMem,
    /// Current configured bus clock (0 = gated). `AtomicU32` because the
    /// host trait methods take `&self` while `set_ios` updates it.
    current_clock: AtomicU32,
    /// Transfer completion status slot (IRQ → waiter).
    xfer: XferStatus,
}

// SAFETY: `DwMmcHost` is a probe-owned slot whose state is register MMIO
// (through `IoMap`, itself Sync) plus lock-free atomics; no `&self` method
// admits a data race, and ownership never crosses threads except through
// `Arc` clones that keep the DMA mappings alive.
unsafe impl Sync for DwMmcHost {}
// SAFETY: no thread-local state; see the `Sync` note.
unsafe impl Send for DwMmcHost {}

impl DwMmcHost {
    /// Controller bring-up (Linux `dw_mci_probe` tail): all-reset, FIFO
    /// thresholds, timeouts, IDMAC ring, interrupt masks.
    ///
    /// `ring` must be `RING_PAGE` bytes and `window` at least
    /// `DMA_WINDOW_BYTES`; both must sit below the 32-bit IDMAC limit.
    pub fn new(
        regs: IoMap,
        ciu_hz: u32,
        fifo_depth: u32,
        slot: u8,
        ring: CoherentMem,
        window: CoherentMem,
        signal: Arc<dyn subsys_mmc::XferSignal>,
    ) -> Result<Self, MmcError> {
        if ciu_hz == 0 || fifo_depth < 2 {
            return Err(MmcError::Unsupported);
        }
        if ring.len() < RING_PAGE || window.len() < DMA_WINDOW_BYTES {
            return Err(MmcError::Unsupported);
        }
        let top = u64::max(ring.bus_addr(), window.bus_addr())
            .saturating_add(u64::try_from(DMA_WINDOW_BYTES).unwrap_or(u64::MAX));
        if top >= IDMAC_ADDR_LIMIT {
            // The allocator handed out memory the 32-bit descriptor mode
            // cannot reach; refusing beats silent corruption.
            return Err(MmcError::Unsupported);
        }
        let host = Self {
            regs,
            ciu_hz,
            fifo_depth,
            slot,
            ring,
            window,
            current_clock: AtomicU32::new(0),
            xfer: XferStatus::new(signal),
        };

        // Global + FIFO + DMA reset.
        host.writel(SDMMC_CTRL, CTRL_ALL_RESET);
        host.writel(SDMMC_PWREN, 1);
        // Full response/data timeout windows (Linux: 0xFFFFFFFF).
        host.writel(SDMMC_TMOUT, 0xFFFF_FFFF);
        // FIFO burst marks: burst 8, RX at half-minus-1, TX at half.
        let half = host.fifo_depth / 2;
        host.writel(SDMMC_FIFOTH, fifoth(0x2, half - 1, half));

        // IDMAC: software reset, ring link, interrupt enable, base address.
        // SWRESET is self-clearing; the databook requires it to complete
        // before any further IDMAC register access.
        host.writel(SDMMC_BMOD, IDMAC_SWRESET);
        for _ in spin_budget(10_000) {
            if host.readl(SDMMC_BMOD) & IDMAC_SWRESET == 0 {
                break;
            }
        }
        host.reset_ring();
        host.writel(SDMMC_IDSTS, IDINT_CLR_ALL);
        host.writel(SDMMC_IDINTEN, IDINT_NI | IDINT_RI | IDINT_TI | IDINT_ERRORS);
        host.writel(SDMMC_DBADDR, host.ring_base());
        // Route data completion through the device interrupt; pure
        // commands are polled via RINTSTS and stay unmasked.
        // Data-completion interrupts stay MASKED until the first parked
        // transfer (`arm_irq_masks`): card detection runs fully polled,
        // and an unmasked line would let the IRQ handler clear RINTSTS
        // bits out from under the polling loops.
        host.writel(SDMMC_INTMASK, 0);
        host.writel(
            SDMMC_CTRL,
            CTRL_INT_ENABLE | CTRL_DMA_ENABLE | CTRL_USE_IDMAC,
        );
        Ok(host)
    }

    /// Unmask the data-completion interrupts (called once, before the
    /// first parked transfer — block I/O).
    pub fn arm_irq_masks(&self) {
        self.writel(SDMMC_INTMASK, INT_DATA_OVER | INT_ACD | INT_ERRORS);
    }

    /// The host slot index (disk naming + bus-instance label).
    pub const fn slot(&self) -> u8 {
        self.slot
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
            // Publish the raw controller error vocabulary; the waiter
            // maps it through [`Self::status_error`].
            self.xfer.publish(interesting | id_error);
        } else {
            self.xfer.publish(XFER_DONE);
        }
        true
    }

    fn readl(&self, off: usize) -> u32 {
        self.regs.read32(off)
    }

    fn writel(&self, off: usize, value: u32) {
        self.regs.write32(off, value)
    }

    /// The ring base as the 32-bit descriptor address.
    fn ring_base(&self) -> u32 {
        u32::try_from(self.ring.bus_addr()).unwrap_or(0)
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

    /// Relink the descriptor ring: entries chained head→tail, the last
    /// flagged end-of-ring pointing back at the head (Linux idmac ring
    /// init).
    fn reset_ring(&self) {
        // SAFETY: `ring` is a coherent, probe-owned `RING_PAGE` page
        // holding exactly `RING_DESCRIPTORS` descriptors.
        let descs = unsafe {
            core::slice::from_raw_parts_mut(self.ring.as_ptr() as *mut IdmacDesc, RING_DESCRIPTORS)
        };
        let base = self.ring.bus_addr();
        let stride = size_of::<IdmacDesc>() as u64;
        for (i, desc) in descs.iter_mut().enumerate() {
            *desc = IdmacDesc::zeroed();
            desc.des3 = u32::try_from(base + stride * (i as u64 + 1)).unwrap_or(0);
        }
        if let Some(last) = descs.last_mut() {
            last.des3 = u32::try_from(base).unwrap_or(0);
            last.des0 = DES0_ER;
        }
    }

    /// Program the descriptor chain over the first `bytes` of the bounce
    /// window and point DBADDR at the ring head.
    fn arm_descriptors(&self, bytes: usize) {
        let chunks = bytes.div_ceil(DESC_DATA_SIZE);
        // SAFETY: same coherent page discipline as `reset_ring`.
        let descs = unsafe {
            core::slice::from_raw_parts_mut(self.ring.as_ptr() as *mut IdmacDesc, RING_DESCRIPTORS)
        };
        let window = self.window.bus_addr();
        for (i, desc) in descs.iter_mut().take(chunks).enumerate() {
            let off = i * DESC_DATA_SIZE;
            let len = usize::min(DESC_DATA_SIZE, bytes - off);
            let first = i == 0;
            let last = i == chunks - 1;
            let mut flags = DES0_OWN;
            if first {
                flags |= DES0_FD;
            }
            if last {
                flags |= DES0_LD;
            }
            desc.des0 = flags;
            desc.des1 = len as u32 & 0x1FFF;
            desc.des2 = u32::try_from(window + off as u64).unwrap_or(0);
        }
        self.writel(SDMMC_DBADDR, self.ring_base());
    }

    /// Stop and reset the DMA engine after a transfer (Linux
    /// `dw_mci_idmac_stop_dma`).
    fn idmac_stop(&self) {
        let bmod = self.readl(SDMMC_BMOD);
        self.writel(
            SDMMC_BMOD,
            (bmod & !(IDMAC_ENABLE | IDMAC_FB)) | IDMAC_SWRESET,
        );
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
        if cmd.stop {
            // GO_IDLE / abort class (Linux marks GO_IDLE this way, and the
            // verified driver sends CMD0 with STOP | INIT).
            cmdr |= CMD_STOP;
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
        // The hold register is set on EVERY command (the verified driver
        // defaults it on; RK3588 integrations do not declare NO_USE_HOLD).
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
            RespKind::R1 | RespKind::R1b | RespKind::R3 | RespKind::ShortNoCrc => MmcResponse {
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

    /// The completion predicate + fallible state read, carrying the
    /// published status word for failure diagnostics.
    fn xfer_result(&self) -> Option<Result<u32, MmcError>> {
        let status = self.xfer.load();
        if status == 0 {
            return None;
        }
        Some(if status == XFER_DONE {
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
            // Non-task context (card detection in probe): bounded poll.
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

    /// Read data phase: stage through the coherent window into `buf`.
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
        outcome.map(|_| ())?;
        // SAFETY: the window holds `bytes` staged bytes from the DMA just
        // completed; bounded by the guard above, and no other borrow of
        // the window is live inside `&self` methods.
        let window = unsafe { coherent_slice(self.window.as_ptr(), bytes) };
        buf[..bytes].copy_from_slice(window);
        Ok(())
    }

    /// Write data phase: stage `buf` through the coherent window out to
    /// the card.
    fn data_cmd_write(&self, cmd: &MmcCommand, bytes: usize, buf: &[u8]) -> Result<(), MmcError> {
        if bytes > DMA_WINDOW_BYTES || buf.len() < bytes {
            return Err(MmcError::Io);
        }
        {
            // SAFETY: same coherent window bounds as the read path; the
            // staging borrow is scoped to this copy.
            let window = unsafe { coherent_slice(self.window.as_ptr(), bytes) };
            window.copy_from_slice(&buf[..bytes]);
        }
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

    /// Shared pre-command sequencing: descriptors, geometry, status
    /// reset, DMA enable, command issue.
    fn start_data_phase(
        &self,
        cmd: &MmcCommand,
        dir: DataDir,
        bytes: usize,
    ) -> Result<(), MmcError> {
        self.arm_irq_masks();
        self.reset_ring();
        self.arm_descriptors(bytes);
        // The DMA engine fetches the descriptor chain over the bus: the
        // chain (and the staged data in the window) must be visible to it
        // before ENABLE.
        karch::dsb_ishst();
        let blksz: u32 = if bytes.is_multiple_of(512) {
            512
        } else {
            bytes as u32
        };
        self.writel(SDMMC_BLKSIZ, blksz);
        self.writel(SDMMC_BYTCNT, bytes as u32);
        self.xfer.reset();

        let bmod = self.readl(SDMMC_BMOD) & !IDMAC_SWRESET;
        self.writel(SDMMC_BMOD, bmod | IDMAC_ENABLE | IDMAC_FB);
        let cmdr = self.cmdr_of(cmd, Some(dir), bytes);
        self.ciu_cmd(cmdr, cmd.arg)
    }

    /// The polling command path (no data phase): issue, wait CMD_DONE
    /// via RINTSTS, decode the response. INTMASK-independent by design.
    fn send_cmd_inner(&self, cmd: &MmcCommand) -> Result<MmcResponse, MmcError> {
        let cmdr = self.cmdr_of(cmd, None, 0);
        self.ciu_cmd(cmdr, cmd.arg)?;
        // Responses complete with CMD_DONE (polled via RINTSTS — the
        // INTMASK only routes data bits to the line).
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
