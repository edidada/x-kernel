// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn failed_final_unmap_identity_cannot_be_reacquired_as_a_live_mapping() {
    let region = DeviceRegion {
        paddr: PhysAddr::from(0x1000),
        size: 4096,
        name: "retiring-mmio-test",
        vaddr: Some(VirtAddr::from(0x8000)),
    };
    let mut regions = [RegionEntry {
        region,
        refs: RegionRefs::Retiring,
    }];
    assert_eq!(
        acquire_mapping_locked(&mut regions, region.paddr, region.size),
        None
    );
    assert_eq!(regions[0].region, region);
    assert_eq!(regions[0].refs, RegionRefs::Retiring);
    regions[0].refs = RegionRefs::Counted(1);
    assert_eq!(
        acquire_mapping_locked(&mut regions, region.paddr, region.size),
        region.vaddr
    );
    assert_eq!(regions[0].refs, RegionRefs::Counted(2));
}
