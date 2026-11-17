// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.
use super::*;
use unittest::{assert, assert_eq, def_test};

#[def_test]
fn unsupported_fixed_counter_capabilities_are_rejected() {
    assert!(Counter::intel(1, 3 | (48 << 5)).is_none());
    assert!(Counter::intel(2, 2 | (48 << 5)).is_none());
    assert!(Counter::intel(2, 3 | (65 << 5)).is_none());
}
#[def_test]
fn counter_preloads_saturate_instead_of_wrapping() {
    let intel = Counter::intel(2, 3 | (48 << 5)).unwrap();
    assert_eq!(intel.preload(0, 1), None);
    assert_eq!(intel.preload(1, 0), None);
    assert_eq!(intel.preload(1, 1), Some(intel.mask()));
    for width in [32, 48, 64] {
        let counter = Counter { width, ..intel };
        let preload = counter.preload(u64::MAX, u64::MAX).unwrap();
        assert_ne!(preload & (1 << (width - 1)), 0);
        assert_eq!(preload & !counter.mask(), 0);
    }
}
#[def_test]
fn amd_banks_use_their_documented_counter_stride() {
    let base = Counter { kind: Kind::Amd { extended: false, global: false }, width: 48, counters: 4 };
    assert_eq!(base.amd_control(), 0xc0010000);
    assert_eq!(base.amd_counter(), 0xc0010004);
    let extended = Counter { kind: Kind::Amd { extended: true, global: true }, counters: 6, ..base };
    assert_eq!(extended.amd_control(), 0xc0010200);
    assert_eq!(extended.amd_counter(), 0xc0010201);
}
