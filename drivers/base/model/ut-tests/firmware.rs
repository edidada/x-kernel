// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, vec};
use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn cell_payload_ending_in_zero_stays_reachable_as_cells() {
    // DT cells <0x1000 0x1000>: the last cell's low byte is 0x00, so the
    // payload ends in NUL. It must stay cells (Bytes) — NOT be swallowed
    // as a string table — or `u32`/`u32_list` become unreachable for the
    // address/length properties drivers actually read (`reg`, ranges).
    let payload: &'static [u8] = Box::leak(
        vec![0x00u8, 0x00, 0x10, 0x00, 0x00, 0x00, 0x10, 0x00].into_boxed_slice(),
    );
    let prop = decode_prop(payload);
    // Two u32 cells read through  — the payload must NOT be
    // swallowed as a string table nor typed into an inaccessible variant.
    let cells = prop
        .u32_list()
        .expect("cell payload ending in 0x00 must stay readable as u32_list");
    assert_eq!(cells, vec![0x00001000, 0x00001000]);
}

#[def_test]
fn single_cell_with_zero_low_byte_reads_as_u32() {
    // The <0 0x40000000> shape collapsed to a single zero cell: all four
    // bytes are 0x00, the classic all-zero reg entry. First byte NUL means
    // the string-table attempt rejects it and it stays a U32 cell.
    let payload: &'static [u8] = Box::leak(vec![0x00u8, 0x00, 0x00, 0x00].into_boxed_slice());
    let prop = decode_prop(payload);
    assert_eq!(prop.as_u32(), Some(0));
}

#[def_test]
fn string_tables_with_non_empty_components_still_decode() {
    let payload: &'static [u8] = Box::leak(b"console=tty0\0foo\0".to_vec().into_boxed_slice());
    let prop = decode_prop(payload);
    assert!(
        matches!(prop, FirmwareProp::StrList(_)),
        "a genuine string table must still decode as StrList"
    );
    let single: &'static [u8] = Box::leak(b"okay\0".to_vec().into_boxed_slice());
    assert!(matches!(decode_prop(single), FirmwareProp::Str("okay")));
}
