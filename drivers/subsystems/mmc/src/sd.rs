// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! SD memory-card protocol core — Linux `sd.c` + `sd_ops.c` collapsed to
//! the init/bring-up path this kernel drives (`mmc_sd_init_card`).
//!
//! Sequence (all through [`MmcHost`], no controller knowledge):
//!
//! 1. 400 kHz, 1-bit bus, 80-clock init frame (`GO_IDLE_STATE`)
//! 2. `SEND_IF_COND` (CMD8) — v2 voltage-info handshake
//! 3. `ACMD41` voltage negotiation with HCS (up to 1 s per spec)
//! 4. `ALL_SEND_CID` → `SET_RELATIVE_ADDR` → `SEND_CSD` → select
//! 5. `SEND_SCR` (ACMD51) — bus-width + spec version
//! 6. high-speed switch (CMD6) at the identification clock, then the default
//!    or high-speed clock, `SET_BLOCKLEN` for byte-addressed cards, and the
//!    4-bit bus (`ACMD6` + host width) last — Linux's ordering
//!
//! SDSC (byte addressing) and SDHC/SDXC (block addressing) both bind;
//! UHS-I (1.8 V signaling) is out of scope — default speed and HS only.

use alloc::sync::Arc;

use crate::{
    BLOCK_SIZE, BusWidth, DetectContext, MmcCommand, MmcError, MmcHost, MmcIos, MmcResponse,
    RespKind,
    bus::{CardKind, MmcCardData},
};

/// Coarse busy-wait for the card reset timing requirement (Linux
/// `mmc_delay(1)` after GO_IDLE). Boot-context friendly: no timer
/// dependency, iteration count rounded far above 1 ms on any aarch64
/// core this kernel targets. Init path only.
fn card_reset_delay() {
    for _ in 0..4_000_000 {
        core::hint::spin_loop();
    }
}

/// Log-and-return helper: card detection is a fixed script, so a failure
/// names the exact step (opcode + arg) — the difference between "card
/// absent", "signal integrity" and "protocol mismatch" at a glance.
fn step<T>(what: &str, opcode: u8, arg: u32, result: Result<T, MmcError>) -> Result<T, MmcError> {
    result.inspect_err(|err| {
        log::warn!("sd: {what} (cmd {opcode}, arg {arg:#x}) -> {err:?}");
    })
}

/// Command indexes used below (Linux `include/linux/mmc/sd.h` subset).
const CMD_GO_IDLE: u8 = 0;
const CMD_ALL_SEND_CID: u8 = 2;
const CMD_SET_RELATIVE_ADDR: u8 = 3;
const CMD_SELECT: u8 = 7;
const CMD_SEND_IF_COND: u8 = 8;
const CMD_SEND_CSD: u8 = 9;
const CMD_SWITCH_FUNC: u8 = 6;
const CMD_SET_BLOCKLEN: u8 = 16;
const CMD_APP: u8 = 55;
const ACMD_SET_BUS_WIDTH: u8 = 6;
const ACMD_SEND_OP_COND: u8 = 41;
const ACMD_SEND_SCR: u8 = 51;

/// Check pattern + 2.7–3.6 V support for CMD8 (Linux `0x1AA`).
const IF_COND_ARG: u32 = 0x1AA;
/// HCS bit (host supports SDHC) + the 3.2–3.4 V window (bits 20/21) —
/// the verified driver's exact argument (`MMC_VDD_32_33 | MMC_VDD_33_34 |
/// MMC_SDHC_CAPACITY_HCS`).
const OP_COND_ARG: u32 = 0x4000_0000 | 0x0030_0000;
/// OCR busy bit (card powered up, Linux `MMC_OCR_BUSY`).
const OCR_BUSY: u32 = 1 << 31;
/// OCR high-capacity bit (Linux `MMC_OCR_CCS`).
const OCR_CCS: u32 = 1 << 30;
/// Card-relative-address shift in command/status arguments.
const RCA_SHIFT: u32 = 16;
/// CSD v1 fields (Linux `mmc_decode_csd` case 0).
const CSD1_C_SIZE_MSB: u32 = 73;
const CSD1_C_SIZE_LSB: u32 = 62;
const CSD1_C_SIZE_MULT_MSB: u32 = 49;
const CSD1_C_SIZE_MULT_LSB: u32 = 47;
const CSD1_READ_BL_MSB: u32 = 83;
const CSD1_READ_BL_LSB: u32 = 80;
/// CSD v2 capacity field (Linux `unstuff_bits(resp, 48, 22)`).
const CSD2_SIZE_MSB: u32 = 69;
const CSD2_SIZE_LSB: u32 = 48;
/// CSD command-class field (bit 10 = CCC_SWITCH).
const CSD_CMDCLASS_MSB: u32 = 95;
const CSD_CMDCLASS_LSB: u32 = 84;
/// CSD structure version (`CSD_STRUCTURE`, bits [127:126]).
const CSD_STRUCTURE_MSB: u32 = 127;
const CSD_STRUCTURE_LSB: u32 = 126;
/// ACMD41 attempt budget: the spec allows 1 s of busy polling.
const OP_COND_ATTEMPTS: usize = 512;
/// Switch-function status is 512 bits (Linux reads 64 bytes).
const SWITCH_STATUS_BYTES: usize = 64;
/// SCR register is 64 bits (Linux reads 8 bytes).
const SCR_BYTES: usize = 8;
/// `HIGH_SPEED_BUS_SPEED` function-selection argument (Linux
/// `sd_switch(mode, 0, HIGH_SPEED_BUS_SPEED)` = 0x80FFFFF1).
const HS_FUN_SEL_ARG: u32 = 0x80FF_FFF1;
/// Default-speed card clock (SD spec `max_dtr` for the default speed mode).
const DEFAULT_SPEED_HZ: u32 = 25_000_000;
/// High-speed card clock (the HS mode's `max_dtr`).
const HIGH_SPEED_HZ: u32 = 50_000_000;

/// Scan a slot for an SD memory card and bring it to transfer-ready state.
///
/// Returns the card identity on success, [`MmcError::NoCard`] when the
/// slot is empty (the caller decides whether that is fatal), and protocol
/// errors otherwise. Runs in the host driver's probe context: no task
/// parking is required — the host polls its own completion status there.
pub fn detect_card(
    ctx: &DetectContext<'_>,
    host: Arc<dyn MmcHost>,
) -> Result<MmcCardData, MmcError> {
    let host_ref = host.as_ref();
    if !host_ref.card_present() {
        log::warn!("sd: CDETECT says no card");
        return Err(MmcError::NoCard);
    }

    // 1-3. Init frame, voltage handshake, power-up negotiation.
    let ocr = init_and_power_up(host_ref)?;
    let block_addressed = ocr & OCR_CCS != 0;

    // 4. Identification: CID → RCA → CSD → select.
    let (cid, rca, csd, sectors) = identify_card(host_ref)?;

    // 5-6. SCR, bus width, block length, speed.
    let (bus_width_4bit, high_speed) = setup_bus(host_ref, rca, &csd, block_addressed)?;

    // Phase-boundary hygiene: the switch/SCR data phases leave the
    // controller's FIFO and the card's data state machine with residue;
    // a force-stop + FIFO reset here gives the next transfer a clean
    // start (Linux mmc_set_timing boundary housekeeping).
    host_ref.recover();

    Ok(MmcCardData {
        kind: CardKind::Sd,
        slot: ctx.slot,
        rca,
        cid: cid.raw,
        sectors,
        block_addressed,
        high_speed,
        bus_width_4bit,
        host,
    })
}

/// Reset frame plus voltage negotiation (Linux `mmc_sd_init_card` prologue):
/// GO_IDLE at 400 kHz on one data line, the CMD8 echo check, then the
/// ACMD41 power-up poll. Returns the card's OCR.
fn init_and_power_up(host: &dyn MmcHost) -> Result<u32, MmcError> {
    // 1. Init frame: 400 kHz, 1-bit, power on. GO_IDLE carries the
    //    80-clock initialization pattern.
    step(
        "initial ios (400 kHz / 1-bit / power)",
        0,
        0,
        host.set_ios(&MmcIos {
            clock_hz: 400_000,
            bus_width: BusWidth::Bit1,
            power_on: true,
        }),
    )?;
    let mut idle = MmcCommand::none(CMD_GO_IDLE);
    idle.init_clocks = true;
    idle.stop = true;
    step("go idle", CMD_GO_IDLE, 0, host.send_cmd(&idle))?;
    // The card is busy resetting after GO_IDLE; hammering CMD8 at it
    // inside that window returns corrupted responses (Crc).
    card_reset_delay();

    // 2. v2 voltage handshake: a card that does not echo 0x1AA in the
    //    low bits is a v1-era card this core does not drive.
    // R7's CRC bits are undefined on several DW_mshc boards — the
    // verified driver checks response presence only (ShortNoCrc).
    let if_cond = match host.send_cmd(&MmcCommand::with(
        CMD_SEND_IF_COND,
        IF_COND_ARG,
        RespKind::ShortNoCrc,
    )) {
        Ok(resp) => resp,
        // An empty slot leaves the CMD line floating, so the controller
        // reports a response timeout or a CRC/start-bit mismatch on CMD8.
        // That is "no card", not a signal-integrity fault — and CDETECT
        // cannot settle it: on RK3588 both the sdmmc and the sdio node read
        // "present" even when only one has a card. Linux treats a CMD8
        // timeout the same way.
        Err(err @ (MmcError::Crc | MmcError::Timeout)) => {
            log::info!("sd: no card answered the voltage handshake ({err:?})");
            return Err(MmcError::NoCard);
        }
        Err(err) => {
            step::<()>("voltage handshake", CMD_SEND_IF_COND, IF_COND_ARG, Err(err))?;
            return Err(err);
        }
    };
    if if_cond.word() & 0x0FFF != IF_COND_ARG & 0x0FFF {
        return Err(MmcError::Unsupported);
    }

    // 3. Voltage negotiation.
    let mut ocr = 0;
    let mut powered = false;
    // ACMD41 is a broadcast-style exchange: while the card is busy with
    // its internal power-up it MAY return corrupted or missing responses
    // (Linux's __mmc_send_op_cond retries through exactly this). Per-
    // iteration errors are tolerated; only exhausting the budget (the
    // spec's 1 s) fails the card.
    for attempt in 0..OP_COND_ATTEMPTS {
        let resp = match app_cmd(host, 0, ACMD_SEND_OP_COND, OP_COND_ARG, RespKind::R3) {
            Ok(resp) => resp,
            // Cards mid-initialization commonly show Timeout (no
            // response yet) or Crc (partial response); keep polling.
            Err(MmcError::Timeout | MmcError::Crc) => {
                if attempt % 64 == 63 {
                    log::warn!("sd: ACMD41 attempt {attempt}: still no usable response");
                }
                continue;
            }
            Err(err) => {
                log::warn!("sd: ACMD41 attempt {attempt} -> {err:?}");
                return Err(err);
            }
        };
        ocr = resp.word();
        if ocr & OCR_BUSY != 0 {
            powered = true;
            break;
        }
        if attempt % 64 == 63 {
            log::warn!("sd: ACMD41 attempt {attempt}: ocr={ocr:#010x} (not busy yet)");
        }
    }
    if !powered {
        log::warn!(
            "sd: ACMD41 never completed power-up ({} polls)",
            OP_COND_ATTEMPTS
        );
        return Err(MmcError::Timeout);
    }
    Ok(ocr)
}

/// Identification and select (Linux `mmc_sd_init_card` step 4): CID, RCA,
/// CSD, capacity, then CMD7 so the card answers addressed commands.
fn identify_card(host: &dyn MmcHost) -> Result<(MmcResponse, u16, MmcResponse, u64), MmcError> {
    let cid = step(
        "read CID",
        CMD_ALL_SEND_CID,
        0,
        host.send_cmd(&MmcCommand::with(CMD_ALL_SEND_CID, 0, RespKind::R2)),
    )?;
    let rca = {
        let resp = step(
            "assign RCA",
            CMD_SET_RELATIVE_ADDR,
            0,
            host.send_cmd(&MmcCommand::with(CMD_SET_RELATIVE_ADDR, 0, RespKind::R1)),
        )?;
        (resp.word() >> RCA_SHIFT) as u16
    };
    let csd = step(
        "read CSD",
        CMD_SEND_CSD,
        u32::from(rca) << RCA_SHIFT,
        host.send_cmd(&MmcCommand::with(
            CMD_SEND_CSD,
            u32::from(rca) << RCA_SHIFT,
            RespKind::R2,
        )),
    )?;
    let sectors = csd_capacity(&csd)?;
    // The CSD decode is the one place a word-order mistake is silent: it
    // only shows up as a wrong capacity and a wrong CCC (which disables the
    // speed switch), so the fields go on the record.
    log::info!(
        "sd: CSD structure={} capacity {sectors} sectors, ccc={:#05x}",
        csd.bits(CSD_STRUCTURE_MSB, CSD_STRUCTURE_LSB),
        csd.bits(CSD_CMDCLASS_MSB, CSD_CMDCLASS_LSB),
    );
    step(
        "select card",
        CMD_SELECT,
        u32::from(rca) << RCA_SHIFT,
        host.send_cmd(&MmcCommand::with(
            CMD_SELECT,
            u32::from(rca) << RCA_SHIFT,
            RespKind::R1b,
        )),
    )?;
    Ok((cid, rca, csd, sectors))
}

/// Speed, block length and bus width (Linux `mmc_sd_init_card` tail).
///
/// Order matters and follows Linux: SCR → **CMD6 speed switch (still at the
/// identification clock, one data line)** → raise the clock → CMD16 for
/// byte-addressed cards → ACMD6 and the 4-bit host width **last**.
/// Returns `(bus_width_4bit, high_speed)`.
fn setup_bus(
    host: &dyn MmcHost,
    rca: u16,
    csd: &MmcResponse,
    block_addressed: bool,
) -> Result<(bool, bool), MmcError> {
    // 5. SCR: spec version + advertised bus widths.
    let scr = read_scr(host, rca)?;
    log::info!(
        "sd: SCR structure={} spec={} bus-widths={:#05x}",
        scr.structure,
        scr.sda_vsn,
        scr.bus_widths
    );
    // Never ask for a bus the slot cannot drive (Linux
    // `MMC_CAP_4_BIT_DATA`): a slot without usable DAT1..DAT3 would
    // otherwise run every data phase into start-bit errors.
    let bus_width_4bit = scr.bus_width_4bit && host.max_bus_width() == BusWidth::Bit4;

    // 6. Speed selection, still at the identification clock on one data line.
    //
    // This order is Linux `mmc_sd_init_card`: `mmc_sd_switch_hs` runs there
    // before `mmc_set_clock` and before `mmc_app_set_bus_width`, so the
    // switch-function read is never the first transfer after two changes at
    // once. Doing width and clock first (as this driver did) makes the
    // 64-byte switch read the *first* 4-bit/25 MHz data phase — the exact
    // corner this board fails with `SBE`/`TCBCNT=0`.
    let high_speed = match switch_high_speed(host, csd, &scr) {
        Ok(high_speed) => high_speed,
        Err(err) => {
            // Best-effort: a card or slot that cannot serve the switch read
            // keeps the default speed instead of failing identification.
            log::warn!("sd: HS switch failed ({err:?}); staying at default speed");
            host.recover();
            false
        }
    };
    let data_hz = if high_speed {
        HIGH_SPEED_HZ
    } else {
        DEFAULT_SPEED_HZ
    };

    // 7. Raise the clock, still on one data line (Linux `mmc_set_clock` sits
    //    between the switch and the width change).
    host.set_ios(&MmcIos {
        clock_hz: data_hz,
        bus_width: BusWidth::Bit1,
        power_on: true,
    })?;

    // CMD16 (SET_BLOCKLEN) only matters for byte-addressed cards: SDHC/
    // SDXC fix the block length at 512 by definition (Linux skips it for
    // block-addressed cards as well).
    if !block_addressed {
        step(
            "set block length",
            CMD_SET_BLOCKLEN,
            BLOCK_SIZE as u32,
            host.send_cmd(&MmcCommand::with(
                CMD_SET_BLOCKLEN,
                BLOCK_SIZE as u32,
                RespKind::R1,
            )),
        )?;
    }

    // 8. Width last (Linux `mmc_app_set_bus_width` + `mmc_set_bus_width`):
    //    the card switches first, then the host.
    if bus_width_4bit {
        step(
            "set 4-bit bus",
            ACMD_SET_BUS_WIDTH,
            (u32::from(rca) << RCA_SHIFT) | 0x2,
            app_cmd(
                host,
                rca,
                ACMD_SET_BUS_WIDTH,
                (u32::from(rca) << RCA_SHIFT) | 0x2,
                RespKind::R1,
            ),
        )?;
        host.set_ios(&MmcIos {
            clock_hz: data_hz,
            bus_width: BusWidth::Bit4,
            power_on: true,
        })?;
    }
    // Report the *negotiated* mode, not the ceiling the host was configured
    // with: a host ceiling of 50 MHz means nothing if the card stayed at the
    // default speed.
    log::info!(
        "sd: card bus {} ({} Hz, {}-bit){}",
        if high_speed {
            "high speed"
        } else {
            "default speed"
        },
        data_hz,
        if bus_width_4bit { 4 } else { 1 },
        if high_speed {
            ""
        } else {
            " [HS switch not applied]"
        },
    );
    Ok((bus_width_4bit, high_speed))
}

/// APP-command wrapper: CMD55 (with the card RCA) followed by the
/// application command (Linux `mmc_send_app_cmd`).
fn app_cmd(
    host: &dyn MmcHost,
    rca: u16,
    opcode: u8,
    arg: u32,
    resp: RespKind,
) -> Result<MmcResponse, MmcError> {
    host.send_cmd(&MmcCommand::with(
        CMD_APP,
        u32::from(rca) << RCA_SHIFT,
        RespKind::R1,
    ))?;
    host.send_cmd(&MmcCommand::with(opcode, arg, resp))
}

/// Capacity in 512-byte sectors from CSD v1 (byte-addressed) or v2
/// (block-addressed) — Linux `mmc_decode_csd` cases 0/1.
fn csd_capacity(csd: &MmcResponse) -> Result<u64, MmcError> {
    match csd.bits(127, 126) {
        0 => {
            let read_bl_len = csd.bits(CSD1_READ_BL_MSB, CSD1_READ_BL_LSB);
            let c_size = csd.bits(CSD1_C_SIZE_MSB, CSD1_C_SIZE_LSB);
            let mult = csd.bits(CSD1_C_SIZE_MULT_MSB, CSD1_C_SIZE_MULT_LSB);
            let capacity = (u64::from(c_size) + 1) << (u64::from(mult) + 2);
            let sectors = capacity << (u64::from(read_bl_len.saturating_sub(9)));
            Ok(sectors)
        }
        1 => {
            let c_size = csd.bits(CSD2_SIZE_MSB, CSD2_SIZE_LSB);
            Ok((u64::from(c_size) + 1) << 10)
        }
        _ => Err(MmcError::Unsupported),
    }
}

/// Whether the CSD advertises the switch command class (bit 10).
fn csd_has_switch(csd: &MmcResponse) -> bool {
    csd.bits(CSD_CMDCLASS_MSB, CSD_CMDCLASS_LSB) & (1 << 10) != 0
}

/// Read and decode the 64-bit SCR register (ACMD51: CMD55 then CMD51 with
/// an 8-byte bus read — Linux `mmc_app_send_scr` + `mmc_decode_scr`).
fn read_scr(host: &dyn MmcHost, rca: u16) -> Result<Scr, MmcError> {
    // CMD55 first, then CMD51 *with* the 8-byte data phase — issuing the
    // data-less CMD51 twice would stall the DAT lines.
    host.send_cmd(&MmcCommand::with(
        CMD_APP,
        u32::from(rca) << RCA_SHIFT,
        RespKind::R1,
    ))?;
    let mut buf = [0u8; SCR_BYTES];
    let scr_cmd = MmcCommand::with(ACMD_SEND_SCR, u32::from(rca) << RCA_SHIFT, RespKind::R1);
    step(
        "read SCR (first data phase)",
        ACMD_SEND_SCR,
        u32::from(rca) << RCA_SHIFT,
        host.read_data_cmd(&scr_cmd, SCR_BYTES, &mut buf),
    )?;

    // SCR arrives MSB-first; fold into the {63:32}/{31:0} words that
    // [`MmcResponse::bits`] reads as raw[2]/raw[3].
    let hi = u32::from_be_bytes(buf[0..4].try_into().map_err(|_| MmcError::Io)?);
    let lo = u32::from_be_bytes(buf[4..8].try_into().map_err(|_| MmcError::Io)?);
    let scr = MmcResponse {
        raw: [0, 0, hi, lo],
    };

    let structure = scr.bits(63, 60);
    if structure != 0 {
        return Err(MmcError::Unsupported);
    }
    Ok(Scr {
        structure,
        sda_vsn: scr.bits(59, 56),
        bus_widths: scr.bits(51, 48),
        bus_width_4bit: scr.bits(51, 48) & 0x4 != 0,
    })
}

/// Decoded SCR fields (Linux `struct sd_scr`, the parts this core uses).
struct Scr {
    /// SCR structure version (bits [63:60]; only 0 is defined).
    structure: u32,
    /// Spec version: 0 = 1.0/1.01, 1 = 1.10, 2 = 2.0+.
    sda_vsn: u32,
    /// Raw `SD_BUS_WIDTHS` field (bits [51:48]).
    bus_widths: u32,
    /// 4-bit bus advertised (Linux `SD_BUS_WIDTH_4`).
    bus_width_4bit: bool,
}

/// Attempt the CMD6 high-speed switch (Linux `mmc_sd_switch_hs`).
///
/// Best-effort: every "can't switch" outcome keeps the default speed and
/// reports `false` rather than failing the card. The reply is a 64-byte bus
/// read carried on the switch command itself, and it runs at the caller's
/// current clock/width — Linux raises the clock only after this returns.
fn switch_high_speed(host: &dyn MmcHost, csd: &MmcResponse, scr: &Scr) -> Result<bool, MmcError> {
    // Cards older than spec 1.10 never implement the switch function.
    if scr.sda_vsn < 1 || !csd_has_switch(csd) {
        return Ok(false);
    }

    let mut status = [0u8; SWITCH_STATUS_BYTES];
    let switch_cmd = MmcCommand::with(CMD_SWITCH_FUNC, HS_FUN_SEL_ARG, RespKind::R1);
    step(
        "HS switch (CMD6 data phase)",
        CMD_SWITCH_FUNC,
        HS_FUN_SEL_ARG,
        host.read_data_cmd(&switch_cmd, SWITCH_STATUS_BYTES, &mut status),
    )?;

    // The caller raises the clock (Linux `mmc_sd_get_max_clock` +
    // `mmc_set_clock`), so this only reports the card's answer.
    Ok(status[16] & 0xF == 0x1)
}
