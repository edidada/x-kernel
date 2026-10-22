// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! MMC/SD subsystem — the Linux `drivers/mmc/core` analogue.
//!
//! The subsystem owns three things, split per the L0-b rule (contracts
//! host-free, services subsystem-owned):
//!
//! * the **host contract** — [`MmcHost`], the per-slot operations a host
//!   controller driver implements (Linux `struct mmc_host_ops`);
//! * the **card bus** — [`MmcBus`], a first-class [`Bus`] whose devices are
//!   the cards a host driver discovered. The host driver probes on the
//!   platform bus, runs detection through this core, and adds the card to
//!   the domain; the block consumer (`blk`, the `mmc_blk` analogue) binds
//!   on this bus and publishes a `Gendisk` into the block class;
//! * the **SD protocol core** — [`sd`], the card-detection state machine
//!   (Linux `mmc_sd_init_card`): idle → voltage check → OCR negotiation →
//!   CID/CSD/SCR → bus width → high-speed switch.
//!
//! The block publication makes the full chain real:
//! platform enumeration → host driver → card bus → mmc_blk → block class
//! → devnode/uevent.
//!
//! # Execution contexts
//!
//! Detection runs in the host driver's probe (no sleeping guaranteed);
//! block I/O runs in block-layer task context and may park. [`XferSignal`]
//! carries both shapes: `wait_until` parks when a task is available and the
//! host falls back to a bounded poll otherwise (probe), `notify` is the
//! IRQ-side wake.

#![no_std]

extern crate alloc;

pub mod blk;
pub mod bus;
pub mod sd;
pub mod xfer;

pub use blk::MmcBlkDriver;
pub use bus::{CardKind, MmcBus, MmcCardData, MmcIdEntry};
use kerrno::KError;
pub use xfer::XferSignal;

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/lib.rs");

/// Detection inputs handed to [`sd::detect_card`] (Linux scans inside
/// `mmc_sd_init_card` with the host on hand).
pub struct DetectContext<'a> {
    /// The host slot being scanned, borrowed for the detection sequence.
    pub host: &'a dyn MmcHost,
    /// Host slot index (`mmc_host->index`): keys disk naming and the
    /// DEVPATH component of the resulting card device.
    pub slot: u8,
}

/// Expected response shape (Linux `MMC_RSP_*` flags, collapsed to the five
/// kinds the SD protocol actually uses).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RespKind {
    /// No response (broadcast commands like CMD0).
    None,
    /// 32-bit response, no busy (R1/R5/R6/R7).
    R1,
    /// 32-bit response followed by card busy on DAT0 (R1b).
    R1b,
    /// 128-bit response carried in RESP0..RESP3 (R2: CID/CSD).
    R2,
    /// OCR register response (R3/ACMD41), CRC bits undefined.
    R3,
    /// Short response whose CRC bits are undefined on this exchange —
    /// CMD8's R7 on several DW_mshc boards (the verified driver checks
    /// response presence only; enabling the CRC check there produces
    /// spurious Crc failures).
    ShortNoCrc,
}

/// One data direction (Linux `MMC_DATA_READ`/`MMC_DATA_WRITE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataDir {
    /// Controller reads from the card into memory.
    Read,
    /// Controller writes from memory to the card.
    Write,
}

/// One command (Linux `struct mmc_command`).
#[derive(Debug, Clone, Copy)]
pub struct MmcCommand {
    /// Command index (0..63).
    pub opcode: u8,
    /// Command argument.
    pub arg: u32,
    /// Expected response shape.
    pub resp: RespKind,
    /// Send the 80-clock initialization frame before the command
    /// (Linux sets `SDMMC_CMD_INIT` for `GO_IDLE_STATE`).
    pub init_clocks: bool,
    /// Append an automatic stop transmission after the data phase
    /// (`SDMMC_CMD_SEND_AUTO_STOP`; CMD18 multi-block reads).
    pub auto_stop: bool,
    /// Issue as STOP/ABORT (`SDMMC_CMD_STOP`, bit 14) — Linux marks
    /// GO_IDLE / GO_INACTIVE / SDIO-abort this way, and the verified
    /// driver sends CMD0 with it too.
    pub stop: bool,
}

impl MmcCommand {
    /// A plain no-response command.
    pub const fn none(opcode: u8) -> Self {
        Self {
            opcode,
            arg: 0,
            resp: RespKind::None,
            init_clocks: false,
            auto_stop: false,
            stop: false,
        }
    }

    /// A command with a response and the given argument.
    pub const fn with(opcode: u8, arg: u32, resp: RespKind) -> Self {
        Self {
            opcode,
            arg,
            resp,
            init_clocks: false,
            auto_stop: false,
            stop: false,
        }
    }
}

/// A decoded response (Linux `cmd->resp[0..4]`, unshifted into that order
/// by the host).
#[derive(Debug, Clone, Copy, Default)]
pub struct MmcResponse {
    /// Raw response words; for `R2` this is `{127:96} .. {31:0}`.
    pub raw: [u32; 4],
}

impl MmcResponse {
    /// The 32-bit response payload (R1/R1b/R3).
    pub fn word(&self) -> u32 {
        self.raw[0]
    }

    /// Read big-endian bit `msb..=lsb` across the four response words
    /// (Linux `unstuff_bits`).
    pub fn bits(&self, msb: u32, lsb: u32) -> u32 {
        let width = msb - lsb + 1;
        // Concatenate {127:0} as a logical 128-bit value read MSB-first.
        let mut out = 0u32;
        for i in 0..width {
            let bit = msb - i;
            let word = &self.raw[(3 - (bit / 32)) as usize];
            let mask_bit = (*word >> (bit % 32)) & 1;
            out = (out << 1) | mask_bit;
        }
        out
    }
}

/// Bus speed/width/power request (Linux `struct mmc_ios`), the `set_ios`
/// payload.
#[derive(Debug, Clone, Copy)]
pub struct MmcIos {
    /// Requested bus clock in Hz; 0 gates the clock.
    pub clock_hz: u32,
    /// Data-line count: 1 (init/default) or 4.
    pub bus_width: BusWidth,
    /// Whether the slot should be powered.
    pub power_on: bool,
}

/// Data bus width (Linux `MMC_BUS_WIDTH_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusWidth {
    /// One data line (initialization and fallback).
    Bit1,
    /// Four data lines (post-SCR negotiation).
    Bit4,
}

/// Host-side error vocabulary (Linux `mmc_err` errno mapping collapsed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MmcError {
    /// The controller or card did not answer in time.
    Timeout,
    /// CRC/checksum disagreement on a response or data phase.
    Crc,
    /// The card reported an error condition (R1 error bits).
    CardStatus,
    /// No card is present in the slot.
    NoCard,
    /// The card answered but lacks a capability this core drives.
    Unsupported,
    /// Controller-level fault outside the protocol (DMA, FIFO).
    Io,
}

impl MmcError {
    /// Map onto the kernel-wide [`KError`] vocabulary for block I/O.
    pub fn to_kerror(self) -> KError {
        match self {
            MmcError::Timeout => KError::WouldBlock,
            MmcError::Crc | MmcError::CardStatus | MmcError::Io => KError::Io,
            MmcError::NoCard | MmcError::Unsupported => KError::InvalidInput,
        }
    }
}

/// One host-controller slot, implemented by the concrete host driver
/// (Linux `struct mmc_host_ops`).
///
/// The protocol core in [`sd`] and the block consumer in [`blk`] drive a
/// card purely through this surface, which is what lets them run against a
/// mock host in unit tests.
pub trait MmcHost: Send + Sync {
    /// Issue a command without a data phase and wait for its completion.
    ///
    /// Probe-safe: implementations must not require a parkable task (they
    /// may poll). `R2` responses are unshifted into [`MmcResponse::raw`].
    fn send_cmd(&self, cmd: &MmcCommand) -> Result<MmcResponse, MmcError>;

    /// Issue `cmd` with a **read** data phase of `bytes` bytes into `buf`
    /// (Linux `mmc_send_cmd` with `MMC_DATA_READ`). `bytes` need not be a
    /// 512-multiple — the SCR read is 8 bytes, the switch-function reply
    /// is 64; block reads use multiples of 512 with `auto_stop` set.
    fn read_data_cmd(&self, cmd: &MmcCommand, bytes: usize, buf: &mut [u8])
    -> Result<(), MmcError>;

    /// Issue `cmd` with a **write** data phase of `bytes` bytes from
    /// `buf` (Linux `mmc_send_cmd` with `MMC_DATA_WRITE`). Returns once
    /// the data phase landed; programming-busy is checked by the caller
    /// via [`poll_card_ready`].
    fn write_data_cmd(&self, cmd: &MmcCommand, bytes: usize, buf: &[u8]) -> Result<(), MmcError>;

    /// Apply a bus speed/width/power change (Linux `set_ios`).
    fn set_ios(&self, ios: &MmcIos) -> Result<(), MmcError>;

    /// Whether a card is currently seated in the slot (Linux `get_cd`).
    fn card_present(&self) -> bool;
}

/// Read whole 512-byte blocks (Linux `mmc_read_blocks`): CMD17 for one
/// block, CMD18 + auto-stop for more. `addr` is in the card's addressing
/// unit (LBA for block-addressed cards, byte offset otherwise).
pub fn read_blocks(host: &dyn MmcHost, addr: u32, blocks: u16, buf: &mut [u8]) -> MmcResult<()> {
    let bytes = usize::from(blocks) * BLOCK_SIZE;
    if buf.len() < bytes {
        return Err(MmcError::Io);
    }
    let mut cmd = if blocks == 1 {
        MmcCommand::with(CMD_READ_SINGLE, addr, RespKind::R1)
    } else {
        let mut multi = MmcCommand::with(CMD_READ_MULTI, addr, RespKind::R1);
        multi.auto_stop = true;
        multi
    };
    cmd.auto_stop = blocks > 1;
    host.read_data_cmd(&cmd, bytes, buf)
}

/// Write whole 512-byte blocks (Linux `mmc_write_blocks`), one CMD24 per
/// block. Multi-block writes (CMD25 + pre-erase bookkeeping) are a
/// deliberate follow-up; per-block writes keep the first slice simple and
/// correct.
///
/// `block_addressed` selects the stride between consecutive device
/// addresses: 1 for LBA-addressed cards, `BLOCK_SIZE` for byte-addressed.
pub fn write_blocks(
    host: &dyn MmcHost,
    addr: u32,
    blocks: u16,
    buf: &[u8],
    rca: u16,
    block_addressed: bool,
) -> MmcResult<()> {
    let stride: u32 = if block_addressed {
        1
    } else {
        BLOCK_SIZE as u32
    };
    for i in 0..u32::from(blocks) {
        let off = i as usize * BLOCK_SIZE;
        let chunk = &buf[off..off + BLOCK_SIZE];
        let cmd = MmcCommand::with(CMD_WRITE_SINGLE, addr + i * stride, RespKind::R1);
        host.write_data_cmd(&cmd, BLOCK_SIZE, chunk)?;
        poll_card_ready(host, rca)?;
    }
    Ok(())
}

/// Polls the card status register until the card leaves its programming
/// busy state after a write (Linux `mmc_poll_for_busy`, collapsed to the
/// `READY_FOR_DATA` check).
pub fn poll_card_ready(host: &dyn MmcHost, rca: u16) -> Result<(), MmcError> {
    const READY_FOR_DATA: u32 = 1 << 8;
    const ATTEMPTS: usize = 250_000;
    for _ in 0..ATTEMPTS {
        let cmd = MmcCommand::with(13, u32::from(rca) << 16, RespKind::R1);
        let status = host.send_cmd(&cmd)?;
        if status.word() & READY_FOR_DATA != 0 {
            return Ok(());
        }
    }
    Err(MmcError::Timeout)
}

/// Standard 512-byte block size (Linux `MMC_SET_BLOCKLEN` argument).
pub const BLOCK_SIZE: usize = 512;

/// Command indexes shared by the protocol core and the block consumer.
pub(crate) const CMD_READ_SINGLE: u8 = 17;
pub(crate) const CMD_READ_MULTI: u8 = 18;
pub(crate) const CMD_WRITE_SINGLE: u8 = 24;

/// `Result` alias over the host-side [`MmcError`].
pub type MmcResult<T> = Result<T, MmcError>;
