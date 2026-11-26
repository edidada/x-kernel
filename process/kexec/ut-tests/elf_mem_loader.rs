// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::ToString, vec};

use kerrno::KError;
use memaddr::VirtAddr;
use memspace::MmSpace;
use unittest::def_test;

use super::{load_user_elf_image, parse_and_validate};

/// Returns a minimal AArch64 ET_DYN ELF with one R+X PT_LOAD,
/// file size 0x100, memory size 0x2000 (BSS tail).
fn sample_elf() -> alloc::vec::Vec<u8> {
    let mut image = vec![0; 0x100];
    image[..4].copy_from_slice(b"\x7fELF"); // magic
    image[4] = 2; // ELFCLASS64
    image[5] = 1; // ELFDATA2LSB
    image[6] = 1; // EV_CURRENT
    image[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
    image[18..20].copy_from_slice(&183u16.to_le_bytes()); // EM_AARCH64
    image[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
    image[24..32].copy_from_slice(&0x80u64.to_le_bytes()); // e_entry
    image[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
    image[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
    image[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    image[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
    // PT_LOAD: flags R|X (5), zero-based (offset=vaddr=0),
    // filesz 0x100, memsz 0x2000 (BSS tail)
    image[64..68].copy_from_slice(&1u32.to_le_bytes()); // p_type
    image[68..72].copy_from_slice(&5u32.to_le_bytes()); // p_flags
    // p_offset, p_vaddr, p_paddr stay 0 (default)
    image[96..104].copy_from_slice(&0x100u64.to_le_bytes()); // p_filesz
    image[104..112].copy_from_slice(&0x2000u64.to_le_bytes()); // p_memsz
    // Small instruction at e_entry.
    image[0x80..0x84].copy_from_slice(&[0x1f, 0x20, 0x03, 0xd5]);
    image
}

#[def_test]
fn maps_static_image_from_memory_and_zeros_bss() {
    let image = sample_elf();
    let mut uspace = MmSpace::new_user_empty().unwrap();
    let (entry, stack, _auxv) =
        load_user_elf_image(&mut uspace, &image, &["/tee/ta/test.ta".to_string()], &[])
            .unwrap();
    // ET_DYN → base = USER_SPACE_BASE (0x1000 in tests);
    // p_vaddr=0, e_entry=0x80 → entry = 0x1080
    assert_eq!(entry.as_usize() & 0xfff, 0x80);
    assert_ne!(stack.as_usize(), 0);

    let mut code = [0; 4];
    uspace.read(entry, &mut code).unwrap();
    assert_eq!(code, [0x1f, 0x20, 0x03, 0xd5]);

    // BSS: base + 0x100 (just past file data).  0x2000 is within
    // the [0x1100, 0x3000) BSS tail.
    let bss_addr = VirtAddr::from_usize(0x2000);
    let mut bss = [0xff; 16];
    uspace.read(bss_addr, &mut bss).unwrap();
    assert_eq!(bss, [0; 16]);
}

#[def_test]
fn rejects_truncated_and_interpreted_images() {
    // phoff beyond image
    let mut image = sample_elf();
    image[32..40].copy_from_slice(&0xffff_ffff_ffff_ffffu64.to_le_bytes());
    assert!(
        load_user_elf_image(
            &mut MmSpace::new_user_empty().unwrap(),
            &image,
            &["/tee/ta/test.ta".to_string()],
            &[],
        )
        .is_err()
    );

    // PT_INTERP
    let mut image = sample_elf();
    image[64..68].copy_from_slice(&3u32.to_le_bytes());
    assert_eq!(
        load_user_elf_image(
            &mut MmSpace::new_user_empty().unwrap(),
            &image,
            &["/tee/ta/test.ta".to_string()],
            &[],
        )
        .unwrap_err(),
        KError::Unsupported
    );
}

#[def_test]
fn rejects_dynamic_relocation_segment() {
    let mut image = sample_elf();
    image[64..68].copy_from_slice(&2u32.to_le_bytes()); // PT_DYNAMIC
    assert_eq!(
        load_user_elf_image(
            &mut MmSpace::new_user_empty().unwrap(),
            &image,
            &["/tee/ta/test.ta".to_string()],
            &[],
        )
        .unwrap_err(),
        KError::Unsupported
    );
}

/// Returns an ET_DYN ELF whose first program header is a `PT_NOTE` covering
/// the program-header table with a bogus virtual address, followed by the
/// real `PT_LOAD` that maps the table.
fn sample_elf_with_covering_note() -> alloc::vec::Vec<u8> {
    let mut image = vec![0; 0x100];
    image[..4].copy_from_slice(b"\x7fELF");
    image[4] = 2; // ELFCLASS64
    image[5] = 1; // ELFDATA2LSB
    image[6] = 1; // EV_CURRENT
    image[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
    image[18..20].copy_from_slice(&183u16.to_le_bytes()); // EM_AARCH64
    image[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
    image[24..32].copy_from_slice(&0x80u64.to_le_bytes()); // e_entry
    image[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
    image[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
    image[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    image[56..58].copy_from_slice(&2u16.to_le_bytes()); // e_phnum

    // phdr[0]: PT_NOTE at file offset 0 with a large bogus virtual address,
    // covering the whole program-header table.
    image[64..68].copy_from_slice(&4u32.to_le_bytes()); // p_type = PT_NOTE
    image[72..80].copy_from_slice(&0u64.to_le_bytes()); // p_offset
    image[80..88].copy_from_slice(&0x00de_ad00u64.to_le_bytes()); // p_vaddr
    image[96..104].copy_from_slice(&0x200u64.to_le_bytes()); // p_filesz

    // phdr[1]: PT_LOAD with R|X that really maps the table.
    image[120..124].copy_from_slice(&1u32.to_le_bytes()); // p_type = PT_LOAD
    image[124..128].copy_from_slice(&5u32.to_le_bytes()); // p_flags = R|X
    image[152..160].copy_from_slice(&0x100u64.to_le_bytes()); // p_filesz
    image[160..168].copy_from_slice(&0x2000u64.to_le_bytes()); // p_memsz
    image
}

#[def_test]
fn at_phdr_uses_mapped_load_segment_not_other_headers() {
    let image = sample_elf_with_covering_note();
    let parsed = parse_and_validate(&image).unwrap();
    assert_eq!(
        parsed.phdr,
        kaddr_layout::USER_SPACE_BASE + 64,
        "AT_PHDR must point into the mapped PT_LOAD, not the covering PT_NOTE"
    );
}
