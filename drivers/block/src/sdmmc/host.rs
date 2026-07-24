// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DesignWare Mobile Storage Host Controller (dw_mshc) controller layer.
//!
//! Transcribed from the upstream Linux driver logic in
//! `drivers/mmc/host/dw_mmc.c`:
//!   * controller bring-up sequence — `dw_mci_probe`
//!   * CIU clock setup — `dw_mci_setup_bus`
//!   * command issue + `wait_while_busy` + START-clear — `mci_send_cmd`
//!   * command-flag construction — `dw_mci_prepare_command`
//!   * response extraction — `dw_mci_command_complete`
//!   * PIO FIFO transfer — `dw_mci_read_data_pio` / `dw_mci_write_data_pio`
//!
//! Execution model difference from Linux: x-kernel has no mmc core, no generic
//! IRQ/threaded-completion state machine, and no `readl_poll_timeout`. We run
//! the controller in **synchronous polling mode** (INTMASK left at 0; command
//! completion is awaited by polling RINTSTS). This mirrors the U-Boot dw_mmc
//! driver and is sufficient for a minimal block driver. See the gap report for
//! what this leaves out (IRQ-driven state machine, IDMAC, tuning).

use driver_base::{DriverError, DriverResult};

use super::regs::{
    self, clkena,
    cmd::{self},
    hcon, int, off, set_fifoth, status, verid,
};

/// Maximum time (ns) to wait for a controller bit to clear / an interrupt to
/// fire. Mirrors Linux's `500 * USEC_PER_MSEC` poll timeout.
const POLL_TIMEOUT_NS: u64 = 500_000_000;

/// Poll `cond` until it returns `true` or the timeout (ns) elapses. Returns
/// `false` on timeout. Uses the monotonic hardware tick counter so it works in
/// any context without the scheduler.
fn wait_until_ns<F: FnMut() -> bool>(timeout_ns: u64, mut cond: F) -> bool {
    let start = khal::time::monotonic_time_nanos();
    loop {
        if cond() {
            return true;
        }
        if khal::time::monotonic_time_nanos().wrapping_sub(start) >= timeout_ns {
            return false;
        }
        core::hint::spin_loop();
    }
}

/// The DesignWare MMC controller. Owns the MMIO window (mapped by the glue
/// layer) and drives it with polling.
pub struct DwMciHost {
    /// Kernel virtual base address of the controller register window.
    base: usize,
    /// Offset of the data FIFO register (0x100 or 0x200, set from VERID).
    fifo_off: usize,
    /// FIFO depth in FIFO-words.
    fifo_depth: u32,
    /// log2 of the FIFO item size in bytes (we use 32-bit access → 2).
    data_shift: u32,
    /// Card interface clock rate the bootloader established (Hz). Source of
    /// truth for CLKDIV math. **Gap**: real value should be read from the DT
    /// `clock-frequency` property via a clk provider (none yet); we take a
    /// caller-supplied default.
    bus_hz: u32,
    /// Controller version ID.
    verid: u32,
}

impl DwMciHost {
    /// Construct a handle around an already-mapped MMIO window.
    ///
    /// # Safety
    ///
    /// `base` must be a valid, exclusively-owned kernel virtual address mapping
    /// the full dw_mshc register block (at least through 0x138), alive for the
    /// lifetime of this host. The caller (glue layer) obtains it via
    /// `iomap_first_mmio`.
    pub unsafe fn new(base: usize, bus_hz: u32) -> Self {
        Self {
            base,
            fifo_off: off::DATA_240A_OFFSET,
            fifo_depth: 0,
            data_shift: 2,
            bus_hz,
            verid: 0,
        }
    }

    #[inline]
    fn reg_ptr(&self, reg: usize) -> *mut u32 {
        (self.base + reg) as *mut u32
    }

    #[inline]
    fn readl(&self, reg: usize) -> u32 {
        // SAFETY: `base` maps the register block (see `new`'s safety contract);
        // all accessed offsets are 4-byte aligned 32-bit registers.
        unsafe { self.reg_ptr(reg).read_volatile() }
    }

    #[inline]
    fn writel(&self, reg: usize, val: u32) {
        // SAFETY: same as `readl`.
        unsafe { self.reg_ptr(reg).write_volatile(val) };
    }

    #[inline]
    fn fifo_read32(&self) -> u32 {
        // SAFETY: fifo_off points at the data FIFO within the same mapping.
        unsafe { self.reg_ptr(self.fifo_off).read_volatile() }
    }

    #[inline]
    fn fifo_write32(&self, val: u32) {
        // SAFETY: same as `fifo_read32`.
        unsafe { self.reg_ptr(self.fifo_off).write_volatile(val) };
    }

    /// Reset the controller/FIFO/DMA logic. Mirrors `dw_mci_ctrl_reset`.
    /// Returns `false` if a reset bit never self-clears.
    fn ctrl_reset(&self, flags: u32) -> bool {
        let ctrl = self.readl(off::CTRL) | flags;
        self.writel(off::CTRL, ctrl);
        wait_until_ns(POLL_TIMEOUT_NS, || self.readl(off::CTRL) & flags == 0)
    }

    /// Controller bring-up. Mirrors the relevant PIO-mode subset of
    /// `dw_mci_probe` (resets, interrupt clear, FIFOTH, clock-gate, VERID).
    pub fn init(&mut self) -> DriverResult {
        // Power on the card BEFORE reset. Mirrors U-Boot `dwmci_init`
        // (`dwmci_writel(DWMCI_PWREN, 1)` as its second line) and Linux
        // `dw_mci_set_ios` MMC_POWER_UP (`PWREN |= 1 << slot->id`). The dw_mshc
        // `PWREN` output gates the card's vmmc rail — on Orange Pi 5 Plus the
        // `mmc@fe2c0000` node has `vmmc-supply`, so the rail is switchable.
        // Leaving PWREN=0 means the card never powers up and every
        // identification command times out (RTO). CTRL reset does not clear
        // PWREN, so writing it first (like both reference drivers) is safe.
        self.writel(off::PWREN, 1);

        if !self.ctrl_reset(regs::ctrl::ALL_RESET_FLAGS) {
            log::error!("sdmmc: controller reset timed out");
            return Err(DriverError::Io);
        }

        // Clear raw interrupts and mask all (polling mode keeps IRQs off).
        self.writel(off::RINTSTS, 0xFFFF_FFFF);
        self.writel(off::INTMASK, 0);
        self.writel(off::TMOUT, 0xFFFF_FFFF);

        // Force PIO mode. U-Boot drove this controller through the internal
        // DMAC (HCON TRANS_MODE = IDMAC on RK3588), leaving CTRL.USE_IDMAC
        // (bit 25) and CTRL.DMA_ENABLE (bit 5) set. `ctrl_reset` above pulses
        // the DMA-reset bit but does NOT clear those config bits, so without
        // this the FIFO output stays routed to the (here-unconfigured) DMA
        // engine — PIO reads of the data port then never pop the FIFO and
        // DATA_OVER never fires (exactly the real-HW hang). Linux clears both
        // when submitting a PIO transfer (dw_mci_submit_data); mirror that
        // once at init.
        let ctrl = self.readl(off::CTRL) & !(regs::ctrl::USE_IDMAC | regs::ctrl::DMA_ENABLE);
        self.writel(off::CTRL, ctrl);
        log::info!(
            "sdmmc: CTRL={:#x} (USE_IDMAC/DMA_ENABLE cleared for PIO)",
            self.readl(off::CTRL)
        );

        // FIFO depth: read from FIFOTH RX_WMark (probe does this when DT
        // doesn't give fifo-depth). RK3588 sdmmc reports 256.
        let fifoth = self.readl(off::FIFOTH);
        self.fifo_depth = regs::fifo_depth_from_fifoth(fifoth);
        self.data_shift = 2; // 32-bit FIFO access (see gap report)

        // FIFOTH: MSIZE=8 (0x2), RX_WMark=depth/2-1, TX_WMark=depth/2.
        let rx = self.fifo_depth / 2 - 1;
        let tx = self.fifo_depth / 2;
        self.writel(off::FIFOTH, set_fifoth(0x2, rx, tx));

        // Gate CIU clock before first setup_bus().
        self.writel(off::CLKENA, 0);
        self.writel(off::CLKSRC, 0);

        // VERID selects the FIFO data-register offset.
        self.verid = verid(self.readl(off::VERID));
        self.fifo_off = if self.verid < regs::DW_MMC_240A {
            off::DATA_OFFSET
        } else {
            off::DATA_240A_OFFSET
        };

        let hcon_val = self.readl(off::HCON);
        log::info!(
            "sdmmc: dw_mshc verid={:#06x} fifo_depth={} hdata_width={} trans_mode={} bus_hz={}",
            self.verid,
            self.fifo_depth,
            hcon::hdata_width(hcon_val),
            hcon::trans_mode(hcon_val),
            self.bus_hz,
        );
        Ok(())
    }

    /// Issue an update-clock command (CMD with UPD_CLK|PRV_DAT_WAIT, no
    /// response). Mirrors `mci_send_cmd(slot, UPD_CLK|PRV_DAT_WAIT, 0)` used
    /// inside `dw_mci_setup_bus`.
    fn send_upd_clk(&self) -> DriverResult {
        self.send_cmd_raw(cmd::UPD_CLK | cmd::PRV_DAT_WAIT, 0)
    }

    /// Configure CIU clock divider + enable, and the bus width. Mirrors
    /// `dw_mci_setup_bus` (the clock!=0 path) plus the CTYPE write at its end.
    pub fn setup_bus(&self, clock_hz: u32, width: u32) -> DriverResult {
        if clock_hz == 0 || self.bus_hz == 0 {
            self.writel(off::CLKENA, 0);
            self.send_upd_clk()?;
            return Ok(());
        }

        // div computation mirrors dw_mci_setup_bus exactly.
        let mut div = self.bus_hz / clock_hz;
        if !self.bus_hz.is_multiple_of(clock_hz) && self.bus_hz > clock_hz {
            div += 1;
        }
        div = if self.bus_hz != clock_hz {
            div.div_ceil(2)
        } else {
            0
        };

        // disable clock
        self.writel(off::CLKENA, 0);
        self.writel(off::CLKSRC, 0);
        self.send_upd_clk()?;
        // set divider
        self.writel(off::CLKDIV, div);
        self.send_upd_clk()?;
        // enable clock (slot 0). LOW_PWR kept on for non-SDIO card.
        self.writel(off::CLKENA, clkena::ENABLE | clkena::LOW_PWR);
        self.send_upd_clk()?;

        self.writel(off::CTYPE, width);
        Ok(())
    }

    /// Wait while the card is busy on the data lines (mirrors
    /// `dw_mci_wait_while_busy`, keyed on PRV_DAT_WAIT).
    fn wait_while_busy(&self, cmd_flags: u32) {
        if cmd_flags & cmd::PRV_DAT_WAIT != 0 {
            wait_until_ns(POLL_TIMEOUT_NS, || {
                self.readl(off::STATUS) & status::BUSY == 0
            });
        }
    }

    /// Lowest-level command issue: write CMDARG, wait-while-busy, write
    /// CMD=START|flags, poll until START self-clears. Mirrors `mci_send_cmd`.
    fn send_cmd_raw(&self, cmd_flags: u32, arg: u32) -> DriverResult {
        self.writel(off::CMDARG, arg);
        self.wait_while_busy(cmd_flags);
        self.writel(off::CMD, cmd::START | cmd_flags);
        if !wait_until_ns(POLL_TIMEOUT_NS, || self.readl(off::CMD) & cmd::START == 0) {
            log::error!("sdmmc: CMD start never cleared (flags={:#x})", cmd_flags);
            return Err(DriverError::Io);
        }
        Ok(())
    }

    /// Set block size / byte count for the upcoming data transfer. Mirrors
    /// `__dw_mci_start_request` data setup.
    pub fn set_block_params(&self, blksz: u32, bytcnt: u32) {
        self.writel(off::BLKSIZ, blksz);
        self.writel(off::BYTCNT, bytcnt);
    }

    /// Poll the raw interrupt status until any of `mask` bits fire (or
    /// timeout). Clears the fired bits.
    fn wait_int(&self, mask: u32) -> Option<u32> {
        let fired = wait_until_ns(POLL_TIMEOUT_NS, || self.readl(off::RINTSTS) & mask != 0);
        if !fired {
            return None;
        }
        let v = self.readl(off::RINTSTS) & mask;
        self.writel(off::RINTSTS, v); // clear
        Some(v)
    }

    /// If `rintsts` carries a data-transfer error, clear those bits, log the
    /// dominant cause (full `rintsts` + `status` for triage), and return
    /// `Err(Io)`; otherwise return `Ok(())`. This is the data-path analogue of
    /// the error decode in `send_command`/`dw_mci_command_complete`. A data
    /// error sets an error bit and **never** sets `DATA_OVER`, so the PIO loops
    /// must consult this on every iteration — without it they spin forever.
    /// That silent infinite spin was the real-HW hang (see the walkthrough's P6
    /// retrospective: every error path must dump the status registers). `dir`
    /// is "read"/"write" for the log line.
    fn data_err_abort(&self, rintsts: u32, dir: &str) -> DriverResult<()> {
        if rintsts & int::DATA_ERROR_FLAGS == 0 {
            return Ok(());
        }
        self.writel(off::RINTSTS, rintsts & int::DATA_ERROR_FLAGS);
        log::error!(
            "sdmmc: {} data error rintsts={:#x} status={:#x} ({})",
            dir,
            rintsts,
            self.readl(off::STATUS),
            data_err_name(rintsts),
        );
        Err(DriverError::Io)
    }

    /// Issue a command and synchronously await completion. `opcode` is used
    /// only for diagnostics. `flags` are the `cmd::*` bits from
    /// [`Self::build_cmd_flags`]. Returns the 4-word response (only [0] valid
    /// for short responses).
    pub fn send_command(&self, opcode: u8, flags: u32, arg: u32) -> DriverResult<[u32; 4]> {
        self.send_cmd_raw(flags, arg)?;

        let want_resp = flags & cmd::RESP_EXP != 0;
        let wait_mask = if want_resp {
            int::CMD_DONE | int::CMD_ERROR_FLAGS
        } else {
            int::CMD_DONE
        };
        let status = match self.wait_int(wait_mask) {
            Some(s) => s,
            None => {
                log::error!(
                    "sdmmc: CMD{} no completion (timeout) flags={:#x} rintsts={:#x} status={:#x}",
                    opcode,
                    flags,
                    self.readl(off::RINTSTS),
                    self.readl(off::STATUS)
                );
                return Err(DriverError::Io);
            }
        };

        if status & int::CMD_ERROR_FLAGS != 0 {
            // Mirror dw_mci_command_complete error decoding.
            log::error!(
                "sdmmc: CMD{} error rintsts={:#x} status={:#x} ({})",
                opcode,
                status,
                self.readl(off::STATUS),
                cmd_err_name(status, flags & cmd::RESP_CRC),
            );
            return Err(DriverError::Io);
        }

        let resp = [
            self.readl(off::RESP0),
            self.readl(off::RESP1),
            self.readl(off::RESP2),
            self.readl(off::RESP3),
        ];
        Ok(resp)
    }

    /// Build CMD-register flags from an SD/MMC opcode + response/data kind.
    /// Mirrors `dw_mci_prepare_command`.
    pub fn build_cmd_flags(
        opcode: u32,
        arg: u32,
        resp_kind: RespKind,
        data: bool,
        write: bool,
        stop: bool,
    ) -> (u32, u32) {
        let mut f = cmd::indx(opcode);

        // STOP / abort commands (CMD12, CMD0, CMD15). Mirrors prepare_command.
        if stop {
            f |= cmd::STOP;
        } else if opcode != 13 && data {
            f |= cmd::PRV_DAT_WAIT;
        }

        match resp_kind {
            RespKind::None => {}
            RespKind::Short => f |= cmd::RESP_EXP | cmd::RESP_CRC,
            RespKind::ShortBusy => f |= cmd::RESP_EXP | cmd::RESP_CRC,
            RespKind::Long => f |= cmd::RESP_EXP | cmd::RESP_CRC | cmd::RESP_LONG,
            // R3 (OCR) / R7: response present, no CRC check.
            RespKind::ShortNoCrc => f |= cmd::RESP_EXP,
        }

        if data {
            f |= cmd::DAT_EXP;
            if write {
                f |= cmd::DAT_WR;
            }
        }
        // RK3588 uses the hold register; mirror USE_HOLD_REG (card flag NO_USE
        // not set).
        f |= cmd::USE_HOLD_REG;

        let _ = arg;
        (f, arg)
    }

    /// Drain whole 32-bit words from the RX FIFO into `out`, copying at most
    /// `out.len()` bytes and never more than the FIFO currently holds (FCNT).
    /// Reading the data register with an empty FIFO is undefined on this IP —
    /// it inflates the byte count with phantom words and desyncs the data state
    /// machine — so every drain is strictly FCNT-bounded. Returns bytes copied.
    fn drain_rx(&self, out: &mut [u8]) -> usize {
        let fcnt_bytes = (status::fcnt(self.readl(off::STATUS)) << self.data_shift) as usize;
        let n = fcnt_bytes.min(out.len()) & !0x3;
        for i in (0..n).step_by(4) {
            let w = self.fifo_read32();
            out[i..i + 4].copy_from_slice(&w.to_le_bytes());
        }
        n
    }

    /// Read `buf.len()` bytes from the FIFO into `buf` during a read data
    /// transfer. Mirrors `dw_mci_read_data_pio` (polling form): drain whatever
    /// the FIFO holds (FCNT-bounded) until `DATA_OVER` fires, then drain the
    /// residual. Aborts — clearing the error bits and dumping `rintsts`/
    /// `status` — on any data error or on `POLL_TIMEOUT_NS`. A data error sets
    /// an error bit and never DATA_OVER; without these checks the loop spins
    /// forever (the real-HW hang). The caller sizes the transfer via `BYTCNT`
    /// (`set_block_params`), so `DATA_OVER` fires exactly when the requested
    /// bytes have arrived.
    pub fn pio_read(&self, buf: &mut [u8]) -> DriverResult {
        let mut off_b = 0usize;
        let start = khal::time::monotonic_time_nanos();
        loop {
            // A data error never raises DATA_OVER — check it first or the
            // drain loop spins forever (the real-HW hang).
            let rintsts = self.readl(off::RINTSTS);
            self.data_err_abort(rintsts, "read")?;

            if off_b < buf.len() {
                off_b += self.drain_rx(&mut buf[off_b..]);
            }
            if rintsts & int::DATA_OVER != 0 {
                // Transfer complete; drain any words still in the FIFO.
                if off_b < buf.len() {
                    self.drain_rx(&mut buf[off_b..]);
                }
                self.writel(off::RINTSTS, int::DATA_OVER);
                break;
            }

            if khal::time::monotonic_time_nanos().wrapping_sub(start) >= POLL_TIMEOUT_NS {
                log::error!(
                    "sdmmc: read data timeout rintsts={:#x} status={:#x} (no DATA_OVER/error \
                     within {}ms; drained {}/{})",
                    self.readl(off::RINTSTS),
                    self.readl(off::STATUS),
                    POLL_TIMEOUT_NS / 1_000_000,
                    off_b,
                    buf.len(),
                );
                return Err(DriverError::Io);
            }
            core::hint::spin_loop();
        }
        Ok(())
    }

    /// Write `buf` to the FIFO during a write data transfer. Mirrors
    /// `dw_mci_write_data_pio` (polling form): fill FIFO by free-word count
    /// until DATA_OVER fires. Both the fill loop and the completion wait are
    /// error-checked and timeout-bounded (see `pio_read`): a data error or a
    /// transfer that never progresses otherwise spins forever.
    pub fn pio_write(&self, buf: &[u8]) -> DriverResult {
        let mut off_b = 0usize;
        let start = khal::time::monotonic_time_nanos();
        while off_b + 4 <= buf.len() {
            self.data_err_abort(self.readl(off::RINTSTS), "write")?;
            let free = ((self.fifo_depth - status::fcnt(self.readl(off::STATUS)))
                << self.data_shift) as usize;
            if free >= 4 {
                let mut w = [0u8; 4];
                w.copy_from_slice(&buf[off_b..off_b + 4]);
                self.fifo_write32(u32::from_le_bytes(w));
                off_b += 4;
            }
            if khal::time::monotonic_time_nanos().wrapping_sub(start) >= POLL_TIMEOUT_NS {
                log::error!(
                    "sdmmc: write data (fill) timeout rintsts={:#x} status={:#x} (filled {}/{})",
                    self.readl(off::RINTSTS),
                    self.readl(off::STATUS),
                    off_b,
                    buf.len(),
                );
                return Err(DriverError::Io);
            }
            core::hint::spin_loop();
        }
        // Tail bytes: pad to a word.
        if off_b < buf.len() {
            let mut w = [0u8; 4];
            w[..buf.len() - off_b].copy_from_slice(&buf[off_b..]);
            self.fifo_write32(u32::from_le_bytes(w));
        }
        // Wait for the transfer to fully complete (error-checked + bounded).
        loop {
            let rintsts = self.readl(off::RINTSTS);
            self.data_err_abort(rintsts, "write")?;
            if rintsts & int::DATA_OVER != 0 {
                self.writel(off::RINTSTS, int::DATA_OVER);
                break;
            }
            if khal::time::monotonic_time_nanos().wrapping_sub(start) >= POLL_TIMEOUT_NS {
                log::error!(
                    "sdmmc: write data (complete) timeout rintsts={:#x} status={:#x}",
                    rintsts,
                    self.readl(off::STATUS),
                );
                return Err(DriverError::Io);
            }
            core::hint::spin_loop();
        }
        Ok(())
    }

    /// True if a card is physically present (CDETECT bit 0 == 0). Source:
    /// `dw_mci_get_cd`.
    pub fn card_present(&self) -> bool {
        self.readl(off::CDETECT) & 0x1 == 0
    }

    /// Dump the clock/power programming for bring-up triage. Compare these
    /// against U-Boot's values (`md.l fe2c0008`, `md.l fe2c0010`, ...) on the
    /// same board — the first mismatch localizes the bug. See the skill's
    /// Phase 6 §6.4 "compare against known-good reference".
    pub fn log_clock_state(&self, tag: &str) {
        log::info!(
            "sdmmc: {} clock state: CLKENA={:#x} CLKDIV={:#x}({}) CLKSRC={:#x} CTYPE={:#x} \
             PWREN={:#x} UHS_REG={:#x} STATUS={:#x}",
            tag,
            self.readl(off::CLKENA),
            self.readl(off::CLKDIV),
            self.readl(off::CLKDIV) & 0xff,
            self.readl(off::CLKSRC),
            self.readl(off::CTYPE),
            self.readl(off::PWREN),
            self.readl(off::UHS_REG),
            self.readl(off::STATUS),
        );
    }
}

/// Name the dominant cause of a command-complete error from its RINTSTS bits.
/// Mirrors the decode order in Linux `dw_mci_command_complete`.
fn cmd_err_name(status: u32, crc_expected: u32) -> &'static str {
    if status & int::RTO != 0 {
        "response timeout (RTO) — card absent / unpowered / wrong clock"
    } else if crc_expected != 0 && status & int::RCRC != 0 {
        "response CRC error (RCRC)"
    } else if status & int::RESP_ERR != 0 {
        "response error (RESP_ERR)"
    } else if status & int::HLE != 0 {
        "hardware locked (HLE) — command issued while CIU busy"
    } else {
        "unknown command error"
    }
}

/// Name the dominant cause of a data-transfer error from its RINTSTS bits. The
/// full `rintsts` is logged alongside, so this is only the headline cause.
/// Mirrors the decode priority in Linux `dw_mci_data_complete`.
fn data_err_name(status: u32) -> &'static str {
    if status & int::SBE != 0 {
        "start-bit error (SBE) — bus width mismatch (card vs controller)"
    } else if status & int::DCRC != 0 {
        "data CRC error (DCRC) — wrong clock / width / tuning"
    } else if status & int::DRTO != 0 {
        "data read timeout (DRTO) — card did not send data"
    } else if status & int::FRUN != 0 {
        "FIFO underrun/overrun (FRUN) — drain/fill too slow or depth wrong"
    } else if status & int::HTO != 0 {
        "host starvation timeout (HTO)"
    } else if status & int::EBE != 0 {
        "end-bit error (EBE)"
    } else if status & int::HLE != 0 {
        "hardware locked (HLE) — data command issued while CIU busy"
    } else {
        "unknown data error"
    }
}

/// Response kind, mirroring Linux MMC_RSP_* classification.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RespKind {
    /// No response (CMD0... when not expecting one).
    None,
    /// R1/R5/R6: 48-bit, CRC checked.
    Short,
    /// R1b: 48-bit with busy (treated like Short at the controller level).
    ShortBusy,
    /// R2 (CID/CSD): 136-bit.
    Long,
    /// R3 (OCR) / R7: 48-bit, CRC NOT checked.
    ShortNoCrc,
}

/// Common SD bus widths usable with `DwMciHost::setup_bus`. Values are the
/// CTYPE register encodings (bit 0 = 4-bit, bit 16 = 8-bit, 0 = 1-bit).
pub mod bus_width {
    /// 1-bit bus (CTYPE = 0).
    pub const WIDTH_1BIT: u32 = 0;
    /// 4-bit bus (CTYPE bit 0).
    pub const WIDTH_4BIT: u32 = 1 << 0;
    /// 8-bit bus (CTYPE bit 16).
    pub const WIDTH_8BIT: u32 = 1 << 16;
}

/// Card-identification clock and base data-transfer clock (legacy default
/// speed). Mirrors mmc core's f_min (400 kHz) and the default-speed 25 MHz.
pub mod clock {
    pub const CARD_IDENT: u32 = 400_000;
    pub const CARD_DEFAULT: u32 = 25_000_000;
}
