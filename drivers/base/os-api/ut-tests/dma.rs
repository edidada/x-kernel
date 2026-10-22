// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

#[def_test]
fn dma_view_translate_applies_offset_and_enforces_mask() {
    // Identity view: pass-through.
    let identity = crate::DmaInfo::DEFAULT;
    assert_eq!(identity.translate(0x8000_1234), Some(0x8000_1234));

    // dma-ranges offset: CPU 0x8000_0000 window appears at device 0x4000_0000.
    let offset_view = crate::DmaInfo {
        mask: u64::MAX,
        offset: 0x4000_0000,
        coherent: true,
    };
    assert_eq!(offset_view.translate(0x8000_1234), Some(0x4000_1234));

    // A 32-bit device: anything above the mask is REJECTED, not truncated —
    // the failure a real host converts into a failed allocation.
    let narrow = crate::DmaInfo {
        mask: 0xFFFF_FFFF,
        offset: 0,
        coherent: true,
    };
    assert_eq!(narrow.translate(0x1_0000_0000), None);
    assert_eq!(narrow.translate(0xFFFF_FFFF), Some(0xFFFF_FFFF));

    // The translated address (not the physical one) is what the mask gates.
    let narrow_offset = crate::DmaInfo {
        mask: 0xFFFF_FFFF,
        offset: -0x4000_0000, // device ids sit ABOVE the CPU window
        coherent: true,
    };
    assert_eq!(narrow_offset.translate(0x4000_0000), Some(0x8000_0000));
    assert_eq!(narrow_offset.translate(0x1_4000_0000), None);
}
