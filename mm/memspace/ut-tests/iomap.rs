// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn iomap_range_preserves_partial_page_offsets() {
    for (base, size, expected_base, expected_span) in [
        (0x1000, 4096, 0x1000, 4096),
        (0x1001, 1, 0x1000, 4096),
        (0x1fff, 2, 0x1000, 8192),
        (usize::MAX - 8191, 4096, usize::MAX - 8191, 4096),
    ] {
        assert_eq!(
            mapping_range(base.into(), size),
            Ok((expected_base.into(), expected_span))
        );
    }
}

#[def_test]
fn iomap_range_rejects_raw_and_page_rounding_overflow() {
    for (base, size) in [
        (0, 0),
        (usize::MAX, 1),
        (usize::MAX - 1, 1),
        (usize::MAX - 4095, 1),
    ] {
        assert_eq!(
            mapping_range(base.into(), size),
            Err(IoMapError::InvalidRange)
        );
        assert_eq!(
            iomap_device(base.into(), size, "invalid-iomap-ut"),
            Err(IoMapError::InvalidRange)
        );
    }
}

#[def_test]
fn iomap_reuse_counts_only_runtime_references() {
    for refs in [RegionRefs::Pinned, RegionRefs::Counted(1)] {
        let mut entries = [RegionEntry {
            region: DeviceRegion {
                paddr: 0x1000.into(),
                size: 8192,
                name: "local-iomap-ut",
                vaddr: Some(va!(0x10000)),
            },
            refs,
        }];
        assert_eq!(
            acquire_mapping_locked(&mut entries, 0x2000.into(), 4096),
            Some(va!(0x11000))
        );
        assert_eq!(
            acquire_mapping_locked(&mut entries, 0x2000.into(), 4097),
            None
        );
        assert!(matches!(
            (refs, entries[0].refs),
            (RegionRefs::Pinned, RegionRefs::Pinned)
                | (RegionRefs::Counted(1), RegionRefs::Counted(2))
        ));
    }
}
