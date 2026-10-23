// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DesignWare Mobile Storage Host controller core — the **PIO-mode**
//! subset of Linux `drivers/mmc/host/dw_mmc.c`, transcribed from the
//! board-verified reference (`drivers/blk/dwmmc-host` in the tgoskits
//! tree): register map, clock sequencing, and the FIFO-polled data path.
//!
//! The data phase is PIO by deliberate choice (the verified reference is
//! PIO too, and it documents the trap: with IDMAC routing left armed,
//! the FIFO feeds the DMA engine and `DATA_OVER` never fires). The data
//! phase is issued with `DATA_EXPECT`, then the loop polls RINTSTS —
//! **error bits first** (a data error never raises DATA_OVER) — draining
//! the FIFO by FCNT until the transfer completes, tail words included.
//! The internal-DMA engine (IDMAC) is a follow-up optimization.

use core::sync::atomic::{AtomicU32, Ordering};

use driver_base::IoMap;
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
const SDMMC_MINTSTS: usize = 0x040;
const SDMMC_RINTSTS: usize = 0x044;
const SDMMC_STATUS: usize = 0x048;
const SDMMC_FIFOTH: usize = 0x04c;
const SDMMC_CDETECT: usize = 0x050;
const SDMMC_VERID: usize = 0x06c;
const SDMMC_CLKSRC_HI: usize = 0x078; // unused placeholder; CLKSRC at 0x0c
/// FIFO data port: pre-2.40a at 0x100, 2.40a+ at 0x200 (Linux
/// `DATA_OFFSET` / `DATA_240A_OFFSET`).
const SDMMC_DATA_OFFSET: usize = 0x100;
const SDMMC_DATA_240A_OFFSET: usize = 0x200;
/// VERID register value that moved the data port to 0x200.
const DW_MMC_240A: u32 = 0x240a;

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

/// STATUS register card-busy indicator (DAT0 driven low).
const STATUS_CARD_BUSY: u32 = 1 << 9;
/// STATUS FIFO count field [28:17] (32-bit words).
const STATUS_FCNT_SHIFT: u32 = 17;

// FIFOTH layout: MSIZE [30:28], RX_WMARK [27:16], TX_WMARK [11:0].
const fn fifoth(msize: u32, rx_wmark: u32, tx_wmark: u32) -> u32 {
    ((msize & 0x7) << 28) | ((rx_wmark & 0xFFF) << 16) | (tx_wmark & 0xFFF)
}

/// Command-issue poll budget (START clear + clock-inform frames).
const CMD_POLL_LIMIT: u32 = 1 << 21;
/// Data-phase poll budget — a full 512-byte block at 400 kHz worst case
/// needs ~11 ms; the bound covers it many times over.
const DATA_POLL_LIMIT: u32 = 1 << 26;

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
    /// FIFO depth in 32-bit entries (DT `fifo-depth`).
    fifo_depth: u32,
    /// Offset of the FIFO data register (VERID-dependent).
    fifo_off: usize,
    /// Host slot index (`mmc_host->index`).
    slot: u8,
    /// Current configured bus clock (0 = gated).
    current_clock: AtomicU32,
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
    pub fn new(regs: IoMap, bus_hz: u32, fifo_depth: u32, slot: u8) -> Result<Self, MmcError> {
        if bus_hz == 0 || fifo_depth < 2 {
            return Err(MmcError::Unsupported);
        }
        let mut host = Self {
            regs,
            bus_hz,
            fifo_depth,
            slot,
            fifo_off: SDMMC_DATA_240A_OFFSET,
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
        // FIFO thresholds: baseline burst 8, RX at half-minus-1, TX at
        // half (Linux `dw_mci_adjust_fifoth` fallback shape).
        let half = host.fifo_depth / 2;
        host.writel(SDMMC_FIFOTH, fifoth(0x2, half - 1, half));
        // Gate CIU clock before the first setup_bus.
        host.writel(SDMMC_CLKENA, 0);
        host.writel(SDMMC_CLKSRC, 0);

        // VERID selects the FIFO data-port offset.
        let verid = host.readl(SDMMC_VERID) & 0xFFFF;
        host.fifo_off = if verid < 0x240a {
            SDMMC_DATA_OFFSET
        } else {
            SDMMC_DATA_240A_OFFSET
        };
        Ok(host)
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
            _ => MmcResponse {
                raw: [self.readl(SDMMC_RESP0), 0, 0, 0],
            },
        }
    }

    /// Map a raw RINTSTS error word onto the error vocabulary.
    fn status_error(status: u32) -> MmcError {
        if status & (INT_RTO | INT_RCRC | INT_RESP_ERR) != 0 {
            MmcError::Crc
        } else if status & (INT_DRTO | INT_HTO) != 0 {
            MmcError::Timeout
        } else {
            MmcError::Io
        }
    }

    /// Human-readable names for a raw RINTSTS status word (diagnostics).
    fn status_names(status: u32) -> alloc::string::String {
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

    /// Drain whatever the FIFO holds into `buf` (FCNT-bounded, word
    /// granularity), Linux `dw_mci_read_data_pio` inner loop. Returns
    /// bytes drained.
    fn drain_rx(&self, out: &mut [u8]) -> usize {
        let available = self.fifo_bytes();
        let n = available.min(out.len()) & !0x3;
        for i in (0..n).step_by(4) {
            let word = self.fifo_read32();
            out[i..i + 4].copy_from_slice(&word.to_le_bytes());
        }
        n
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

    /// Recovery after a failed data transfer: FIFO reset, force-stop with
    /// CMD12, full raw-status clear (Linux error path). Without this a
    /// card parked mid-data-state poisons every later command.
    fn recover_after_data_error(&self, failed_cmd: &MmcCommand) {
        self.writel(SDMMC_CTRL, self.readl(SDMMC_CTRL) | CTRL_FIFO_RESET);
        for _ in spin_budget(10_000) {
            if self.readl(SDMMC_CTRL) & CTRL_FIFO_RESET == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        let stop = CMD_STOP | CMD_RESP_EXP | CMD_RESP_CRC | CMD_USE_HOLD_REG | 12;
        let _ = self.ciu_cmd(stop, 0, true);
        self.writel(SDMMC_RINTSTS, 0xFFFF_FFFF);
        log::warn!(
            "dwmmc: recovered after data error on cmd {} ({})",
            failed_cmd.opcode,
            Self::status_names(self.readl(SDMMC_STATUS)),
        );
    }
}

impl MmcHost for DwMmcHost {
    fn send_cmd(&self, cmd: &MmcCommand) -> Result<MmcResponse, MmcError> {
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
        self.data_cmd_read(cmd, bytes, buf)
    }

    fn write_data_cmd(&self, cmd: &MmcCommand, bytes: usize, buf: &[u8]) -> Result<(), MmcError> {
        self.data_cmd_write(cmd, bytes, buf)
    }

    fn set_ios(&self, ios: &MmcIos) -> Result<(), MmcError> {
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
                self.recover_after_data_error(cmd);
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
                // command — an ILLEGAL_COMMAND/error bit here means the
                // card accepted the command but refused the data phase.
                let r1 = self.readl(SDMMC_RESP0);
                log::info!("dwmmc: data resp R1={r1:#010x}");
                break;
            }
        }
        if drained < bytes {
            log::warn!(
                "dwmmc: cmd {} short read {drained}/{bytes} (rintsts {:#x})",
                cmd.opcode,
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
                self.recover_after_data_error(cmd);
                return Err(err);
            }
            let free = (self.fifo_depth as usize * 4).saturating_sub(self.fifo_bytes());
            let chunk = (bytes - filled).min(free) & !0x3;
            for i in (0..chunk).step_by(4) {
                let mut word = [0u8; 4];
                word.copy_from_slice(&buf[filled + i..filled + i + 4]);
                self.fifo_write32(u32::from_le_bytes(word));
            }
            filled += chunk;
            if chunk == 0 {
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
                self.recover_after_data_error(cmd);
                return Err(err);
            }
        }
        Err(MmcError::Timeout)
    }

    /// Shared data-phase head: thresholds, geometry, command issue.
    fn start_data_phase(
        &self,
        cmd: &MmcCommand,
        dir: DataDir,
        bytes: usize,
    ) -> Result<(), MmcError> {
        // Per-transfer FIFO thresholds (Linux dw_mci_adjust_fifoth): the
        // burst must match the block geometry or FIFO and DMA burst (here:
        // the CPU drain) desynchronize.
        let half = self.fifo_depth / 2;
        self.writel(SDMMC_FIFOTH, fifoth(0x2, half - 1, half));
        let blksz: u32 = if bytes.is_multiple_of(512) {
            512
        } else {
            bytes as u32
        };
        self.writel(SDMMC_BLKSIZ, blksz);
        self.writel(SDMMC_BYTCNT, bytes as u32);
        let cmdr = self.cmdr_of(cmd, Some(dir), bytes);
        self.ciu_cmd(cmdr, cmd.arg, true)
    }
}
