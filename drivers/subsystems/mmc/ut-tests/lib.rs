// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mock-host regressions for the SD protocol core and the block consumer
//! — the L0-b payoff: everything below runs without a controller.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use unittest::{assert_eq, def_test};

use super::bus::CardKind;
use super::sd;
use super::xfer::{status_is_ok, XFER_DONE};
use super::{DetectContext, MmcCommand, MmcError, MmcHost, MmcIos, MmcResponse, RespKind};
use super::{read_blocks, write_blocks, BusWidth, DataDir};

// ---------------------------------------------------------------------------
// Mock host: a scripted card on the other end of the [`MmcHost`] trait.
// ---------------------------------------------------------------------------

/// Set `value` into `raw` at big-endian bit range `msb..=lsb` (the inverse
/// of [`MmcResponse::bits`]).
fn set_bits(raw: &mut [u32; 4], msb: u32, lsb: u32, value: u32) {
    let width = msb - lsb + 1;
    for i in 0..width {
        let bit = msb - i;
        let word = &mut raw[(3 - (bit / 32)) as usize];
        let out_bit = width - 1 - i;
        let mask = 1 << (bit % 32);
        *word = (*word & !mask) | (((value >> out_bit) & 1) << (bit % 32));
    }
}

/// A CSD v2 register advertising `c_size` (block-addressed card) with the
/// switch command class set.
fn csd_v2(c_size: u32) -> [u32; 4] {
    let mut raw = [0u32; 4];
    set_bits(&mut raw, 127, 126, 1);
    set_bits(&mut raw, 95, 84, 1 << 10); // CCC_SWITCH
    set_bits(&mut raw, 69, 48, c_size);
    raw
}

/// An SCR with spec version 2 and both 1-bit/4-bit bus widths advertised.
fn scr_spec2_width4() -> [u8; 8] {
    // {63:60}=0 structure, {59:56}=2 spec, {51:48}=0x5 widths.
    [0x02, 0x50, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
}

#[derive(Default)]
struct MockState {
    acmd41_polls: u32,
    commands: Vec<(u8, u32, bool)>,
    data_reads: Vec<(u8, usize)>,
    ios: Vec<(u32, u8)>,
    writes: Vec<(u32, usize)>,
}

/// The scripted card.
struct MockHost {
    state: RefCell<MockState>,
}

// SAFETY: the mock is exercised single-threaded by the unittest runner;
// `RefCell` borrows never cross threads.
unsafe impl Sync for MockHost {}
// SAFETY: no thread-local state; see the `Sync` note.
unsafe impl Send for MockHost {}

impl MockHost {
    fn new() -> Arc<Self> {
        Arc::new(Self { state: RefCell::new(MockState::default()) })
    }
}

impl MmcHost for MockHost {
    fn send_cmd(&self, cmd: &MmcCommand) -> Result<MmcResponse, MmcError> {
        self.state.borrow_mut().commands.push((cmd.opcode, cmd.arg, cmd.auto_stop));
        let mut resp = MmcResponse { raw: [0; 4] };
        match cmd.opcode {
            0 => {}
            8 => resp.raw[0] = cmd.arg, // echo: v2 voltage handshake
            55 => {}
            41 => {
                let mut state = self.state.borrow_mut();
                state.acmd41_polls += 1;
                // Busy for two polls, then powered up as SDHC.
                resp.raw[0] = if state.acmd41_polls < 3 {
                    0x00FF_8000
                } else {
                    0xC0FF_8000
                };
            }
            2 => resp.raw = [0xDEAD_BEEF, 0x1234_5678, 0x9ABC_DEF0, 0x0F1E_2D3C],
            3 => resp.raw[0] = 0x1234 << 16, // assigned RCA 0x1234
            9 => resp.raw = csd_v2(1023),
            7 | 16 => {}
            _ => {}
        }
        Ok(resp)
    }

    fn read_data_cmd(
        &self,
        cmd: &MmcCommand,
        bytes: usize,
        buf: &mut [u8],
    ) -> Result<(), MmcError> {
        self.state.borrow_mut().data_reads.push((cmd.opcode, bytes));
        match cmd.opcode {
            51 => buf[..bytes].copy_from_slice(&scr_spec2_width4()[..bytes]),
            6 => {
                // Switch-function status: group 1 selects HS.
                for slot in buf.iter_mut() {
                    *slot = 0;
                }
                buf[16] = 0x01;
            }
            17 | 18 => {
                // Data reads carry the block address as a marker.
                let marker = cmd.arg.to_be_bytes();
                for (i, slot) in buf[..bytes].iter_mut().enumerate() {
                    *slot = marker[i % 4];
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn write_data_cmd(&self, cmd: &MmcCommand, bytes: usize, buf: &[u8]) -> Result<(), MmcError> {
        self.state
            .borrow_mut()
            .writes
            .push((cmd.arg, buf.len().min(bytes)));
        Ok(())
    }

    fn set_ios(&self, ios: &MmcIos) -> Result<(), MmcError> {
        let width = match ios.bus_width {
            BusWidth::Bit1 => 1,
            BusWidth::Bit4 => 4,
        };
        self.state.borrow_mut().ios.push((ios.clock_hz, width));
        Ok(())
    }

    fn card_present(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Detection.
// ---------------------------------------------------------------------------

#[def_test]
fn detect_negotiates_identifies_and_switches_speed() {
    let host = MockHost::new();
    let ctx = DetectContext { host: host.as_ref(), slot: 2 };
    let card = sd::detect_card(&ctx, host.clone()).expect("detection succeeds");

    assert_eq!(card.kind, CardKind::Sd);
    assert_eq!(card.slot, 2);
    assert_eq!(card.rca, 0x1234);
    // c_size 1023 → (1023 + 1) << 10 sectors.
    assert_eq!(card.sectors, 1024 * 1024);
    assert_eq!(card.block_addressed, true);
    assert_eq!(card.high_speed, true);
    assert_eq!(card.bus_width_4bit, true);
    assert_eq!(card.host.as_ref() as *const dyn MmcHost, host.as_ref() as *const dyn MmcHost);

    let state = host.state.borrow();
    // Init at 400 kHz/1-bit; width change at 400 kHz/4-bit; HS at 50 MHz.
    assert_eq!(state.ios.first().copied(), Some((400_000, 1)));
    assert_eq!(state.ios.last().copied(), Some((50_000_000, 4)));
    // ACMD41 polled until busy cleared.
    assert!(state.acmd41_polls >= 3);
}

#[def_test]
fn empty_slot_reports_no_card() {
    struct Empty;
    impl MmcHost for Empty {
        fn send_cmd(&self, _cmd: &MmcCommand) -> Result<MmcResponse, MmcError> {
            Err(MmcError::NoCard)
        }
        fn read_data_cmd(&self, _: &MmcCommand, _: usize, _: &mut [u8]) -> Result<(), MmcError> {
            Err(MmcError::NoCard)
        }
        fn write_data_cmd(&self, _: &MmcCommand, _: usize, _: &[u8]) -> Result<(), MmcError> {
            Err(MmcError::NoCard)
        }
        fn set_ios(&self, _: &MmcIos) -> Result<(), MmcError> {
            Ok(())
        }
        fn card_present(&self) -> bool {
            false
        }
    }
    let ctx = DetectContext { host: &Empty, slot: 0 };
    assert_eq!(sd::detect_card(&ctx, Arc::new(Empty)).map(|_| ()), Err(MmcError::NoCard));
}

#[def_test]
fn v1_card_is_reported_unsupported() {
    struct V1Card;
    impl MmcHost for V1Card {
        fn send_cmd(&self, cmd: &MmcCommand) -> Result<MmcResponse, MmcError> {
            // CMD8 echoes garbage: a pre-v2 card.
            let mut resp = MmcResponse { raw: [0; 4] };
            if cmd.opcode == 8 {
                resp.raw[0] = 0xBEEF;
            }
            Ok(resp)
        }
        fn read_data_cmd(&self, _: &MmcCommand, _: usize, _: &mut [u8]) -> Result<(), MmcError> {
            Ok(())
        }
        fn write_data_cmd(&self, _: &MmcCommand, _: usize, _: &[u8]) -> Result<(), MmcError> {
            Ok(())
        }
        fn set_ios(&self, _: &MmcIos) -> Result<(), MmcError> {
            Ok(())
        }
        fn card_present(&self) -> bool {
            true
        }
    }
    let ctx = DetectContext { host: &V1Card, slot: 0 };
    assert_eq!(sd::detect_card(&ctx, Arc::new(V1Card)).map(|_| ()), Err(MmcError::Unsupported));
}

// ---------------------------------------------------------------------------
// Block-level helpers over the host.
// ---------------------------------------------------------------------------

#[def_test]
fn read_blocks_picks_single_vs_multi_with_auto_stop() {
    let host = MockHost::new();
    let mut one = [0u8; 512];
    read_blocks(host.as_ref(), 100, 1, &mut one).expect("single read");
    let mut four = [0u8; 4 * 512];
    read_blocks(host.as_ref(), 200, 4, &mut four).expect("multi read");

    let state = host.state.borrow();
    assert_eq!(state.data_reads.len(), 2);
    assert_eq!(state.data_reads[0], (17, 512)); // CMD17
    assert_eq!(state.data_reads[1], (18, 4 * 512)); // CMD18
    // The CMD18 row carries auto_stop (the third tuple field records it
    // via `commands` — CMD18's issue is recorded with the flag set).
    let multi = state.commands.iter().find(|(op, _, _)| *op == 18).copied();
    assert_eq!(multi, Some((18, 200, true)));
}

#[def_test]
fn write_blocks_stride_follows_addressing_mode() {
    let host = MockHost::new();
    let payload = [0xA5u8; 512];
    // Byte-addressed card: two blocks from byte offset 512 → 512, 1024.
    write_blocks(host.as_ref(), 512, 2, &payload, 0x1234, false).expect("byte-addressed writes");
    // Block-addressed card: two blocks from LBA 9 → 9, 10.
    write_blocks(host.as_ref(), 9, 2, &payload, 0x1234, true).expect("block-addressed writes");

    let state = host.state.borrow();
    let addrs: Vec<u32> = state.writes.iter().map(|(a, _)| *a).collect();
    assert_eq!(addrs, alloc::vec![512, 1024, 9, 10]);
}

// ---------------------------------------------------------------------------
// Response bit extraction and transfer status vocabulary.
// ---------------------------------------------------------------------------

#[def_test]
fn response_bits_span_word_boundaries() {
    let mut raw = [0u32; 4];
    set_bits(&mut raw, 127, 126, 0b10);
    set_bits(&mut raw, 69, 48, 0b11_1111_1111_1111_1111_1111);
    let resp = MmcResponse { raw };
    assert_eq!(resp.bits(127, 126), 0b10);
    assert_eq!(resp.bits(69, 48), 0x3F_FFFF);
    assert_eq!(resp.bits(31, 0), 0);
}

#[def_test]
fn transfer_status_vocabulary() {
    assert_eq!(XFER_DONE, 1);
    assert_eq!(status_is_ok(XFER_DONE), true);
    assert_eq!(status_is_ok(0), false);
    assert_eq!(status_is_ok(1 << 9), false);
    let _ = DataDir::Read;
    let _ = RespKind::R3;
}
