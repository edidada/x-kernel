// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::vec::Vec;

use unittest::{assert_eq, def_test};

use super::*;

fn encode(cells: &[u32]) -> Vec<u8> {
    cells.iter().flat_map(|cell| cell.to_be_bytes()).collect()
}

fn decode(bytes: &[u8], widths: [u32; 3]) -> Result<Vec<DmaRange>, DmaRangeError> {
    match parse_dma_ranges(Some(bytes), widths[0], widths[1], widths[2])? {
        DmaRanges::Windows(windows) => windows.collect(),
        _ => Ok(Vec::new()),
    }
}

#[def_test]
fn dma_missing_and_empty_are_distinct() {
    assert_eq!(
        matches!(parse_dma_ranges(None, 0, 0, 0), Ok(DmaRanges::Absent)),
        true
    );
    assert_eq!(
        matches!(
            parse_dma_ranges(Some(&[]), 0, 0, 0),
            Ok(DmaRanges::Identity)
        ),
        true
    );
}

#[def_test]
fn dma_preserves_multiple_windows_and_lower_bounds() {
    let bytes = encode(&[0x1000, 0x8000, 0x100, 0x3000, 0x9000, 0x200]);
    assert_eq!(
        decode(&bytes, [1, 1, 1]),
        Ok(alloc::vec![
            DmaRange {
                bus_addr: 0x1000,
                parent_addr: 0x8000,
                size: 0x100
            },
            DmaRange {
                bus_addr: 0x3000,
                parent_addr: 0x9000,
                size: 0x200
            },
        ])
    );
}

#[def_test]
fn dma_keeps_full_width_addresses_without_signed_offset_conversion() {
    let bytes = encode(&[u32::MAX, u32::MAX - 15, 0, 0, 0, 16]);
    assert_eq!(
        decode(&bytes, [2, 2, 2]),
        Ok(alloc::vec![DmaRange {
            bus_addr: u64::MAX - 15,
            parent_addr: 0,
            size: 16,
        }])
    );
    let bytes = encode(&[0x1000, 1, 0, 0, 0x100]);
    assert_eq!(
        decode(&bytes, [1, 2, 2]),
        Ok(alloc::vec![DmaRange {
            bus_addr: 0x1000,
            parent_addr: 1 << 32,
            size: 0x100,
        }])
    );
}

#[def_test]
fn dma_rejects_bad_layouts() {
    for widths in [[0, 1, 1], [1, 3, 1], [1, 1, u32::MAX]] {
        assert_eq!(
            decode(&[0; 12], widths),
            Err(DmaRangeError::UnsupportedCells)
        );
    }
    for length in [1, 4, 11, 13, 23] {
        assert_eq!(
            decode(&alloc::vec![0; length], [1, 1, 1]),
            Err(DmaRangeError::TruncatedTuple)
        );
    }
}

#[def_test]
fn dma_rejects_zero_size_and_both_address_overflows() {
    assert_eq!(
        decode(&encode(&[0, 0, 0]), [1, 1, 1]),
        Err(DmaRangeError::EmptyWindow)
    );
    for cells in [[u32::MAX, u32::MAX, 0, 0, 2], [0, 0, u32::MAX, u32::MAX, 2]] {
        assert_eq!(
            decode(&encode(&cells), [2, 2, 1]),
            Err(DmaRangeError::AddressOverflow)
        );
    }
    let bytes = encode(&[0x1000, 0x8000, 0x100, 0x3000, 0x9000, 0]);
    assert_eq!(decode(&bytes, [1, 1, 1]), Err(DmaRangeError::EmptyWindow));
}
