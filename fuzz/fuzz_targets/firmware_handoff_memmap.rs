// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(memmap) = firmware_handoff::efi::BootMemmapRef::from_bytes(data) else {
        return;
    };
    let header = memmap.header();
    let _ = (
        header.map_size,
        header.desc_size,
        header.desc_ver,
        header.map_key,
        header.buff_size,
    );
    for entry in memmap.entries() {
        let kind = firmware_handoff::efi::classify_memory_type(entry.memory_type());
        let _ = (
            entry.phys_start(),
            entry.phys_end(),
            entry.size(),
            entry.kind(),
            kind,
            entry.type_name(),
            entry.is_usable_ram(),
            entry.is_linear_mapping_candidate(),
            kind.is_linear_mapping_candidate(),
        );
    }
});
