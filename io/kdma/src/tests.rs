// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device limits and shared bounce-backend regressions.

use alloc::vec;
use core::ptr::NonNull;

use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn device_masks_check_complete_ranges() {
    let device = DmaDeviceContext::new(u64::MAX, u32::MAX as u64).unwrap();
    assert!(device.check_range(0xffff_fff0, 16, false).is_ok());
    assert!(device.check_range(0xffff_fff0, 17, false).is_err());
    assert!(device.check_range(0x1_0000_0000, 16, true).is_ok());
    assert!(device.check_range(0, 0, true).is_err());
    assert!(device.check_range(u64::MAX, 2, true).is_err());
    assert!(DmaDeviceContext::new(0, u64::MAX).is_err());
    assert!(DmaDeviceContext::new(u64::MAX, 0xff00).is_err());
}

#[def_test]
fn shared_pool_rejects_unreachable_mapping_without_consuming_storage() {
    // A test-owned pool above 4 GiB makes mask rejection deterministic on every platform.
    let mut backing = vec![0u8; 16384];
    let mut allocator = DmaAllocator::new();
    allocator.bounce_pool = Some(BouncePool::new(
        DmaInfo {
            cpu_addr: NonNull::new(backing.as_mut_ptr()).unwrap(),
            bus_addr: DmaBusAddress::new(0x1_0000_0000),
        },
        backing.len(),
    ));
    let limited = DmaDeviceContext::new(u32::MAX as u64, u32::MAX as u64).unwrap();
    let first = DmaDeviceContext::platform_default();
    let second = DmaDeviceContext::platform_default();
    let mut a = [7u8; 16];
    let mut b = [3u8; 16];
    let pa = NonNull::from(a.as_mut_slice());
    let pb = NonNull::from(b.as_mut_slice());
    let direction = DmaDirection::Bidirectional;
    for _ in 0..32 {
        // SAFETY: a remains initialized and exclusive; failed preparation publishes no mapping.
        assert!(unsafe { allocator.map_dma_buffer(&limited, pa, direction) }.is_err());
    }
    assert!(allocator.active_mappings.is_empty());
    // SAFETY: a remains live and exclusive until its matching unmap below.
    let ma = unsafe { allocator.map_dma_buffer(&first, pa, direction) }.unwrap();
    // SAFETY: b remains live and exclusive until its matching unmap below.
    let mb = unsafe { allocator.map_dma_buffer(&second, pb, direction) }.unwrap();
    assert!(ma.bus_addr != mb.bus_addr);
    assert_eq!(allocator.active_mappings.len(), 2);
    // SAFETY: the simulated device writes inside a's live bounce allocation.
    unsafe { ma.cpu_addr.as_ptr().write(9) };
    // SAFETY: device access has stopped and the original mapping hands ownership back to the CPU.
    unsafe { allocator.sync_dma_buffer(ma.bus_addr, pa, direction, crate::DmaSyncTarget::Cpu) }
        .unwrap();
    assert_eq!(a[0], 9);
    a[1] = 8;
    // SAFETY: CPU writes have stopped; the original active mapping is handed back to the device.
    unsafe { allocator.sync_dma_buffer(ma.bus_addr, pa, direction, crate::DmaSyncTarget::Device) }
        .unwrap();
    // SAFETY: byte one is within the live bounce allocation.
    let second_byte = unsafe { ma.cpu_addr.as_ptr().add(1) };
    // SAFETY: the ownership handoff initialized this byte before simulated device access.
    let value = unsafe { second_byte.read() };
    assert_eq!(value, 8);
    // SAFETY: b's simulated DMA has stopped; these are the original arguments.
    unsafe { allocator.unmap_dma_buffer(mb.bus_addr, pb, direction) };
    assert_eq!(allocator.active_mappings.len(), 1);
    // SAFETY: a's simulated DMA has stopped; these are the original arguments.
    unsafe { allocator.unmap_dma_buffer(ma.bus_addr, pa, direction) };
    assert!(allocator.active_mappings.is_empty());
    assert_eq!(a, [9, 8, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
    assert_eq!(b, [3; 16]);
}

#[def_test]
fn bounced_receive_preserves_data_on_cancel_and_copies_on_completion() {
    let context = crate::DmaDeviceContext::platform_default();
    let mut buffer = [7u8; 16];
    let pointer = NonNull::from(buffer.as_mut_slice());
    // SAFETY: buffer is initialized and retained exclusively until the following unmap.
    let mapping =
        unsafe { crate::map_dma_buffer(&context, pointer, crate::DmaDirection::DeviceToDriver) }
            .unwrap();
    // SAFETY: hardware never received this mapping; original arguments cancel it exactly once.
    unsafe {
        crate::unmap_dma_buffer(
            &context,
            mapping.bus_addr,
            pointer,
            crate::DmaDirection::DeviceToDriver,
        )
    };
    assert_eq!(buffer, [7; 16]);
    // SAFETY: buffer stays initialized, live and exclusive until the matching unmap.
    let mapping =
        unsafe { crate::map_dma_buffer(&context, pointer, crate::DmaDirection::DeviceToDriver) }
            .unwrap();
    // SAFETY: the returned bounce allocation covers 16 bytes; simulated DMA writes the first byte.
    unsafe { mapping.cpu_addr.as_ptr().write(9) };
    // SAFETY: simulated DMA has completed and these are the original mapping arguments.
    unsafe {
        crate::unmap_dma_buffer(
            &context,
            mapping.bus_addr,
            pointer,
            crate::DmaDirection::DeviceToDriver,
        )
    };
    assert_eq!(buffer[0], 9);
    assert_eq!(buffer[1], 7);
}
