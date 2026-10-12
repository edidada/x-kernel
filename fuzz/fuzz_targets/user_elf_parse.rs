// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(builder) = kernel_elf_parser::ELFHeadersBuilder::new(data) else {
        return;
    };

    let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let range = builder.ph_range();
        let start = usize::try_from(range.start).ok()?;
        let end = usize::try_from(range.end).ok()?;
        if start > end || end > data.len() {
            return None;
        }
        let ph_slice = data.get(start..end)?;
        if ph_slice.is_empty() {
            return None;
        }

        let headers = builder.build(ph_slice).ok()?;
        let interp_base = if data.first().copied().unwrap_or(0) & 1 == 0 {
            0
        } else {
            0x1000
        };
        let parser = kernel_elf_parser::ELFParser::new(&headers, interp_base).ok()?;

        let _ = parser.entry();
        let _ = parser.phnum();
        let _ = parser.phent();
        let _ = parser.phdr();
        let _ = parser.base();
        for ph in parser.headers().ph.iter() {
            let _ = (
                ph.offset,
                ph.virtual_addr,
                ph.physical_addr,
                ph.file_size,
                ph.mem_size,
                ph.align,
                ph.flags,
                ph.get_type(),
            );
        }

        let stack_end = 0x4000_0000usize;
        let ldso_base = if data.get(1).copied().unwrap_or(0) & 2 != 0 {
            Some(0x1000)
        } else {
            None
        };
        let auxv: std::vec::Vec<_> = parser.aux_vector(stack_end, ldso_base).collect();
        for entry in &auxv {
            let _ = (entry.get_type(), entry.value());
        }

        if !auxv.is_empty() {
            let args = [std::string::String::from("fuzz")];
            let envs = [std::string::String::from("FUZZ=1")];
            let at_random = [0u8; 16];
            let _ = kernel_elf_parser::app_stack_region(&args, &envs, &auxv, &at_random, stack_end);
        }
        Some(())
    }));

    let _ = parsed;
});
