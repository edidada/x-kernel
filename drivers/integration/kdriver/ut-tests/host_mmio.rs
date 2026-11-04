// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn model_mmio_rejects_invalid_extents_and_preserves_error_kinds() {
    for region in [
        MmioRegion::new(0, 0),
        MmioRegion::new(usize::MAX, 1),
        MmioRegion::new(usize::MAX - 1, 1),
        MmioRegion::new(0, usize::MAX),
    ] {
        assert_eq!(
            map_region(region, "invalid-model-mmio-ut").unwrap_err(),
            RuntimeError::Invalid
        );
    }
    assert_eq!(
        RuntimeError::from(driver_os_api::ResError::InvalidResource),
        RuntimeError::Invalid
    );
    assert_eq!(
        RuntimeError::from(driver_os_api::ResError::NoMemory),
        RuntimeError::NoMemory
    );
    assert_eq!(
        RuntimeError::from(driver_os_api::ResError::MappingFailed),
        RuntimeError::Io
    );
}

#[def_test(serial)]
fn model_mmio_guards_release_one_native_reference_each() {
    let physical_page = 0x0b00_0000;
    let region = MmioRegion::new(physical_page + 0x123, 0x80);
    let contains_page = || {
        memspace::device_regions().any(|entry| {
            entry.paddr.as_usize() <= physical_page
                && entry
                    .paddr
                    .as_usize()
                    .checked_add(entry.size)
                    .is_some_and(|end| end > physical_page)
        })
    };
    assert!(!contains_page());
    let first = map_region(region, "model-mmio-reference-ut").unwrap();
    let second = map_region(region, "model-mmio-reference-ut").unwrap();
    let same_pointer = first.as_ptr() == second.as_ptr();
    let preserves_offset = first.as_ptr() as usize & 0xfff == 0x123;
    let virtual_address = memaddr::VirtAddr::from_usize(first.as_ptr() as usize);
    let original_region = first.region();
    drop(first);
    let is_still_mapped = contains_page();
    drop(second);
    let is_released = !contains_page();
    let vma_is_released = memspace::kernel_layout()
        .lock()
        .find_vma(virtual_address)
        .is_none();
    assert!(same_pointer && preserves_offset && is_still_mapped && is_released && vma_is_released);
    assert_eq!(original_region, region);
}
