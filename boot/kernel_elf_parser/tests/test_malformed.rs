// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kernel_elf_parser::{ELFHeadersBuilder, ELFParser};

fn minimal_elf64(phoff: u64, phentsize: u16, phnum: u16) -> Vec<u8> {
    minimal_elf64_with_type_entry(phoff, phentsize, phnum, 2, 0)
}

fn minimal_elf64_with_type_entry(
    phoff: u64,
    phentsize: u16,
    phnum: u16,
    e_type: u16,
    e_entry: u64,
) -> Vec<u8> {
    let mut hdr = vec![0u8; 64];
    hdr[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    hdr[4] = 2;
    hdr[5] = 1;
    hdr[6] = 1;
    hdr[16..18].copy_from_slice(&e_type.to_le_bytes());
    hdr[18..20].copy_from_slice(&0x3eu16.to_le_bytes());
    hdr[20..24].copy_from_slice(&1u32.to_le_bytes());
    hdr[24..32].copy_from_slice(&e_entry.to_le_bytes());
    hdr[32..40].copy_from_slice(&phoff.to_le_bytes());
    hdr[52..54].copy_from_slice(&64u16.to_le_bytes());
    hdr[54..56].copy_from_slice(&phentsize.to_le_bytes());
    hdr[56..58].copy_from_slice(&phnum.to_le_bytes());
    hdr
}

#[test]
fn ph_range_rejects_overflow() {
    let elf = minimal_elf64(u64::MAX - 8, 56, 2);
    let builder = ELFHeadersBuilder::new(&elf).expect("header should parse");
    assert!(builder.ph_range().is_err());
}

#[test]
fn build_rejects_zero_phentsize() {
    let elf = minimal_elf64(64, 0, 1);
    let builder = ELFHeadersBuilder::new(&elf).expect("header should parse");
    assert!(builder.build(&[]).is_err());
}

#[test]
fn entry_rejects_dyn_base_overflow() {
    // ET_DYN so ELFParser applies the load bias; e_entry + bias must not panic.
    let elf = minimal_elf64_with_type_entry(64, 56, 0, 3, u64::MAX);
    let builder = ELFHeadersBuilder::new(&elf).expect("header should parse");
    let headers = builder.build(&[]).expect("empty PH table");
    let parser = ELFParser::new(&headers, 0x1000).expect("parser");
    assert!(parser.entry().is_err());
    assert!(parser.aux_vector(0x1000, None).is_err());
}
