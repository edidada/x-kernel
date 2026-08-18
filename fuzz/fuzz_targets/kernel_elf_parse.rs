// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(elf) = kernel_elf_loader::KernelElf::parse(data) else {
        return;
    };

    let _ = elf.image_size();
    let _ = elf.entry_point_vaddr();
    let (start, end) = elf.image_vaddr_range();
    let _ = elf.paddr_for_vaddr(0, start);
    let _ = elf.paddr_for_vaddr(0, end.saturating_sub(1));
    let _ = elf.paddr_for_vaddr(0x1000, start.wrapping_add(0x1000));

    for name in ["", "_start", "main", "init", "__start"] {
        let _ = elf.find_symbol_value(name);
    }
    if let Ok(name) = core::str::from_utf8(data.get(..32).unwrap_or(data)) {
        let _ = elf.find_symbol_value(name.trim_end_matches('\0'));
    }

    for seg in elf.load_segments() {
        let _ = (seg.vaddr, seg.memsz, seg.file_data.len());
        if !seg.file_data.is_empty() {
            let _ = seg.file_data[0];
            let _ = seg.file_data[seg.file_data.len() - 1];
        }
    }
});
