// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! SD card protocol layer + [`SdmmcDriver`] block device.
//!
//! x-kernel has no `mmc core` subsystem, so the SD card identification and
//! read/write command sequences that Linux performs in `drivers/mmc/core/sd.c`
//! are re-implemented here in a minimal, synchronous form on top of
//! [`DwMciHost`]. CSD capacity decoding mirrors `mmc_decode_csd` (SD v2 / SDHC).
//!
//! Command semantics follow the SD Physical Layer Simplified Specification and
//! the upstream Linux mmc core; controller-level mechanics are in `host.rs`.

mod host;
mod regs;

use driver_base::{Device, DeviceKind, DriverError, DriverResult};
pub use host::{DwMciHost, RespKind, bus_width, clock};
use kspin::SpinNoIrq;

use crate::BlockDevice;

/// Logical block size for SD block access.
pub const BLOCK_SIZE: usize = 512;

/// SD/MMC command opcodes used here (SD spec / Linux mmc headers).
mod op {
    pub const GO_IDLE: u32 = 0; // CMD0
    pub const ALL_SEND_CID: u32 = 2; // CMD2, R2
    pub const SEND_RELATIVE_ADDR: u32 = 3; // CMD3, R6
    pub const SELECT_CARD: u32 = 7; // CMD7, R1b
    pub const SEND_IF_COND: u32 = 8; // CMD8, R7
    pub const SEND_CSD: u32 = 9; // CMD9, R2
    pub const READ_SINGLE_BLOCK: u32 = 17; // CMD17, R1
    pub const WRITE_SINGLE_BLOCK: u32 = 24; // CMD24, R1
    pub const APP_CMD: u32 = 55; // CMD55, R1
    pub const SD_SEND_OP_COND: u32 = 41; // ACMD41, R3
}

/// ACMD41 argument: 3.2–3.4 V window (bits 20:21) + SDHC/SDXC support (bit 30).
/// Matches Linux `MMC_VDD_32_33 | MMC_VDD_33_34 | MMC_SDHC_CAPACITY_HCS`.
const OCR_ARG: u32 = 0x4030_0000;
/// Power-up done bit in the OCR (R3) returned by ACMD41.
const OCR_POWER_UP: u32 = 1 << 31;

/// An SD card driven by a dw_mshc controller, exposed as a block device.
pub struct SdmmcDriver {
    inner: SpinNoIrq<DwMciHost>,
    num_blocks: u64,
}

impl SdmmcDriver {
    /// Map-independent constructor used by the glue layer. Brings the
    /// controller up and enumerates the card.
    ///
    /// # Safety
    ///
    /// `base` must be a valid, exclusively-mapped kernel virtual address of the
    /// controller register window (see [`DwMciHost::new`]).
    pub unsafe fn new(base: usize, bus_hz: u32) -> DriverResult<Self> {
        // SAFETY: forwarded from the glue layer's iomap; same contract.
        let mut host = unsafe { DwMciHost::new(base, bus_hz) };
        host.init()?;

        let mut me = Self {
            inner: SpinNoIrq::new(host),
            num_blocks: 0,
        };
        me.init_card()?;
        Ok(me)
    }

    /// Send a (non-data) command and return its 4-word response.
    fn cmd(&self, opcode: u32, arg: u32, resp: RespKind) -> DriverResult<[u32; 4]> {
        let host = self.inner.lock();
        let (flags, _) = DwMciHost::build_cmd_flags(opcode, arg, resp, false, false, false);
        host.send_command(flags, arg)
    }

    /// Issue APP_CMD (CMD55) then an application command (ACMD*).
    fn app_cmd(
        &self,
        acmd_opcode: u32,
        arg: u32,
        rca: u32,
        resp: RespKind,
    ) -> DriverResult<[u32; 4]> {
        let host = self.inner.lock();
        let (f55, _) = DwMciHost::build_cmd_flags(
            op::APP_CMD,
            rca << 16,
            RespKind::Short,
            false,
            false,
            false,
        );
        host.send_command(f55, rca << 16)?;

        let (facmd, _) = DwMciHost::build_cmd_flags(acmd_opcode, arg, resp, false, false, false);
        host.send_command(facmd, arg)
    }

    /// Run SD card identification: reset → voltage check → OCR → CID → RCA →
    /// CSD → select → switch to 4-bit / 25 MHz. Mirrors `mmc_sd_init_card`.
    fn init_card(&mut self) -> DriverResult {
        {
            let host = self.inner.lock();
            if !host.card_present() {
                log::warn!("sdmmc: no card detected (CDETECT)");
                return Err(DriverError::Io);
            }
            host.setup_bus(clock::CARD_IDENT, bus_width::WIDTH_1BIT)?;
        }

        // CMD0 GO_IDLE_STATE with INIT clock (80 init clocks). No response.
        {
            let host = self.inner.lock();
            let (f0, _) =
                DwMciHost::build_cmd_flags(op::GO_IDLE, 0, RespKind::None, false, false, true);
            host.send_command(f0 | regs::cmd::INIT, 0)?;
        }

        // CMD8 SEND_IF_COND: SD v2.0 check, expect 0x1AA echoed.
        let r8 = self.cmd(op::SEND_IF_COND, 0x1aa, RespKind::ShortNoCrc)?;
        if r8[0] & 0xfff != 0x1aa {
            log::warn!(
                "sdmmc: CMD8 mismatch {:#x}; assuming non-SDHC card",
                r8[0] & 0xfff
            );
        }

        // ACMD41 SD_SEND_OP_COND: poll OCR until power-up complete.
        let mut ocr = 0u32;
        for _ in 0..1024 {
            let r = self.app_cmd(op::SD_SEND_OP_COND, OCR_ARG, 0, RespKind::ShortNoCrc)?;
            ocr = r[0];
            if ocr & OCR_POWER_UP != 0 {
                break;
            }
        }
        if ocr & OCR_POWER_UP == 0 {
            log::error!("sdmmc: ACMD41 power-up never completed (ocr={:#x})", ocr);
            return Err(DriverError::Io);
        }
        let sdhc = ocr & (1 << 30) != 0; // card reports HCS support

        // CMD2 ALL_SEND_CID (ignored) then CMD3 SEND_RELATIVE_ADDR.
        self.cmd(op::ALL_SEND_CID, 0, RespKind::Long)?;
        let r3 = self.cmd(op::SEND_RELATIVE_ADDR, 0, RespKind::Short)?;
        let rca = (r3[0] >> 16) & 0xffff;

        // CMD9 SEND_CSD with RCA → capacity.
        let csd = self.cmd(op::SEND_CSD, rca << 16, RespKind::Long)?;
        self.num_blocks = decode_csd_capacity(&csd);

        // CMD7 SELECT_CARD (R1b).
        self.cmd(op::SELECT_CARD, rca << 16, RespKind::ShortBusy)?;

        // Switch to 4-bit, default-speed 25 MHz.
        {
            let host = self.inner.lock();
            host.setup_bus(clock::CARD_DEFAULT, bus_width::WIDTH_4BIT)?;
        }
        self.app_cmd(6, 2, rca, RespKind::Short)?; // ACMD6 width=4-bit

        log::info!(
            "sdmmc: card ready sdhc={} rca={:#x} capacity={} blocks ({} MiB)",
            sdhc,
            rca,
            self.num_blocks,
            self.num_blocks / 2 / 1024
        );
        Ok(())
    }
}

impl Device for SdmmcDriver {
    fn name(&self) -> &str {
        "sdmmc"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }
}

impl BlockDevice for SdmmcDriver {
    fn num_blocks(&self) -> u64 {
        self.num_blocks
    }

    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        let host = self.inner.lock();
        host.set_block_params(BLOCK_SIZE as u32, BLOCK_SIZE as u32);
        let (flags, _) = DwMciHost::build_cmd_flags(
            op::READ_SINGLE_BLOCK,
            block_id as u32,
            RespKind::Short,
            true,
            false,
            false,
        );
        host.send_command(flags, block_id as u32)?;
        host.pio_read(buf)
    }

    fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult {
        let host = self.inner.lock();
        host.set_block_params(BLOCK_SIZE as u32, BLOCK_SIZE as u32);
        let (flags, _) = DwMciHost::build_cmd_flags(
            op::WRITE_SINGLE_BLOCK,
            block_id as u32,
            RespKind::Short,
            true,
            true,
            false,
        );
        host.send_command(flags, block_id as u32)?;
        host.pio_write(buf)
    }

    fn flush(&self) -> DriverResult {
        Ok(())
    }
}

/// Decode a 128-bit CSD (R2 response) into a capacity in 512-byte blocks.
/// Mirrors Linux `mmc_decode_csd` for SD v1.0 and v2.0 (SDHC/SDXC).
///
/// `resp` is the raw `[RESP0, RESP1, RESP2, RESP3]` from the controller. We
/// re-pack into the Linux convention `[RESP3, RESP2, RESP1, RESP0]` (highest
/// word first) before bit-unstuffing.
fn decode_csd_capacity(resp: &[u32; 4]) -> u64 {
    // CSD word order: index 0 holds bits [127:96].
    let csd = [resp[3], resp[2], resp[1], resp[0]];
    let structure = unstuff_bits(&csd, 126, 2);

    if structure == 1 {
        // CSD v2.0 (SDHC/SDXC): C_SIZE [48:69], capacity = (C_SIZE+1) * 1024 blocks.
        let csize = unstuff_bits(&csd, 48, 22);
        (csize as u64 + 1) << 10
    } else {
        // CSD v1.0: capacity from READ_BL_LEN + C_SIZE + C_SIZE_MULT.
        let read_bl_len = unstuff_bits(&csd, 80, 4);
        let csize = unstuff_bits(&csd, 62, 12);
        let csize_mult = unstuff_bits(&csd, 47, 3);
        let block_len = 1u64 << read_bl_len;
        let mult = 1u64 << (csize_mult + 2);
        ((csize as u64 + 1) * mult * block_len) / BLOCK_SIZE as u64
    }
}

/// Extract `size` bits starting at bit offset `start` from a 4-word (128-bit)
/// CSD/CID, highest word at index 0. Mirrors Linux `UNSTUFF_BITS`.
fn unstuff_bits(resp: &[u32; 4], start: u32, size: u32) -> u32 {
    let off = 3 - (start / 32);
    let shft = start & 31;
    let mut res = resp[off as usize] >> shft;
    if size + shft > 32 {
        res |= resp[(off - 1) as usize] << ((32 - shft) % 32);
    }
    let mask = if size < 32 {
        (1u32 << size) - 1
    } else {
        u32::MAX
    };
    res & mask
}

#[cfg(unittest)]
mod tests {
    use super::*;

    #[unittest::def_test]
    fn unstuff_bits_csdiv2_csize() {
        // CSD v2.0 with C_SIZE = 0xE67F (a 32 GB card). Place it at [48:69]:
        // bits 48..63 live in word index 2 (shift 16); bits 64..69 in word 1.
        let csize: u32 = 0xE67F;
        let mut csd = [0u32; 4];
        csd[2] |= (csize & 0xFFFF) << 16; // bits 48..63
        csd[1] |= (csize >> 16) & 0x3F; // bits 64..69
        csd[0] |= 1 << 30; // structure = 1 at [126:127]
        assert_eq!(unstuff_bits(&csd, 48, 22), csize);
        assert_eq!(
            decode_csd_capacity(&[csd[3], csd[2], csd[1], csd[0]]),
            (csize as u64 + 1) << 10
        );
    }
}
