// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::{discard_dma_buffer, map_dma_buffer, unmap_dma_buffer};

#[def_test(serial)]
fn discard_removes_active_mapping_and_restores_pool_capacity() {
    let mut original = [0xa5u8; 64];
    let buffer = NonNull::from(original.as_mut_slice());
    let mut allocator = ALLOCATOR.lock();
    let before_capacity = allocator.bounce_pool_mut().unwrap().available_bytes();
    let before_count = allocator.active_mappings.len();
    // SAFETY: the array is exclusively held and stays live until discard;
    // the allocator lock prevents unrelated mappings from changing the baseline.
    let mapped = unsafe { allocator.map_dma_buffer(buffer, DmaDirection::DeviceToDriver) }.unwrap();
    let was_registered = allocator
        .active_mappings
        .contains_key(&mapped.bus_addr.as_u64());
    let during_capacity = allocator.bounce_pool_mut().unwrap().available_bytes();
    // SAFETY: the matching mapping is still active, has never been published,
    // and is consumed exactly once while the original array remains live.
    unsafe { allocator.discard_dma_buffer(mapped.bus_addr, buffer) };
    let is_removed = !allocator
        .active_mappings
        .contains_key(&mapped.bus_addr.as_u64());
    let after_count = allocator.active_mappings.len();
    let after_capacity = allocator.bounce_pool_mut().unwrap().available_bytes();
    drop(allocator);
    assert!(was_registered && is_removed);
    assert!(during_capacity < before_capacity);
    assert_eq!(after_count, before_count);
    assert_eq!(after_capacity, before_capacity);
    assert_eq!(original, [0xa5; 64]);
}

#[def_test(serial)]
fn unpublished_mapping_discard_preserves_original_bytes_in_every_direction() {
    for direction in [
        DmaDirection::DriverToDevice,
        DmaDirection::DeviceToDriver,
        DmaDirection::Bidirectional,
    ] {
        for _ in 0..8 {
            let mut original = [0xa5u8; 64];
            let buffer = NonNull::from(original.as_mut_slice());
            // SAFETY: the local array stays exclusively owned until discard;
            // the returned address is never submitted to hardware.
            let mapped = unsafe { map_dma_buffer(buffer, direction) }.unwrap();
            // SAFETY: mapped.cpu_addr is a distinct live bounce allocation of
            // at least 64 bytes, privately held by this unpublished mapping.
            unsafe { mapped.cpu_addr.as_ptr().write_bytes(0x37, original.len()) };
            // SAFETY: this is the original buffer and still-active unpublished
            // mapping, consumed once, before the local array is accessed again.
            unsafe { discard_dma_buffer(mapped.bus_addr, buffer) };
            assert_eq!(original, [0xa5; 64]);
        }
    }
}

#[def_test(serial)]
fn normal_streaming_completion_keeps_directional_copy_back() {
    for direction in [
        DmaDirection::DriverToDevice,
        DmaDirection::DeviceToDriver,
        DmaDirection::Bidirectional,
    ] {
        let mut original = [0xa5u8; 64];
        let buffer = NonNull::from(original.as_mut_slice());
        // SAFETY: the original buffer remains exclusively held until unmap;
        // this test simulates device writes without actual hardware access.
        let mapped = unsafe { map_dma_buffer(buffer, direction) }.unwrap();
        // SAFETY: the mapping owns a live 64-byte bounce region, disjoint from
        // the original array; no device or concurrent CPU accesses it.
        unsafe { mapped.cpu_addr.as_ptr().write_bytes(0x37, original.len()) };
        // SAFETY: the same active mapping, buffer and direction are consumed
        // once, after simulated device writes have stopped.
        unsafe { unmap_dma_buffer(mapped.bus_addr, buffer, direction) };
        let expected = if direction == DmaDirection::DriverToDevice {
            0xa5
        } else {
            0x37
        };
        assert!(original.iter().all(|byte| *byte == expected));
    }
}
