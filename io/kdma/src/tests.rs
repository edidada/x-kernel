// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device constraints, translated bounce alignment and ownership regression tests.

use alloc::vec;
use core::ptr::NonNull;

use unittest::{assert, assert_eq, def_test};

use crate::{DmaBusAddress, DmaConstraints, DmaInfo, DmaWindow, bounce_pool::BouncePool};

#[def_test]
fn dma_range_checks_complete_interval() {
    let limits = DmaConstraints::DMA32;
    assert!(limits.check_range(0xffff_fff0, 16, true).is_ok());
    assert!(limits.check_range(0xffff_fff0, 17, true).is_err());
    assert!(limits.check_range(0, 0, true).is_err());
    assert!(limits.check_range(u64::MAX, 2, true).is_err());
    let mut limits = limits;
    limits.streaming_mask = u64::MAX;
    assert!(limits.check_range(0x1_0000_0000, 16, true).is_ok());
    assert!(limits.check_range(0x1_0000_0000, 16, false).is_err());
}

#[def_test]
fn dma_translation_checks_window_and_offset() {
    let window = DmaWindow::new(0x1000, 0x8000, 0x1000).unwrap();
    assert_eq!(window.translate(0x1800, 0x800).unwrap().as_u64(), 0x8800);
    assert!(window.translate(0x1800, 0x801).is_err());
    assert!(window.translate(0xfff, 1).is_err());
    assert!(DmaWindow::new(0, u64::MAX, 2).is_err());
}

#[def_test]
fn bounce_alignment_uses_bus_address() {
    let mut backing = vec![0u8; 16384];
    let mut pool = BouncePool::new(
        DmaInfo {
            cpu_addr: NonNull::new(backing.as_mut_ptr()).unwrap(),
            bus_addr: DmaBusAddress::new(0x1003),
        },
        backing.len(),
    );
    let limits = DmaConstraints {
        min_addr_align_bytes: 64,
        segment_boundary_mask: 0xfff,
        max_segment_len_bytes: 2048,
        ..DmaConstraints::DMA32
    };
    for _ in 0..3 {
        let (dma, layout, allocation) = pool.allocate(2048, limits).unwrap();
        assert_eq!(dma.bus_addr.as_u64() % 64, 0);
        assert!(
            limits
                .check_range(dma.bus_addr.as_u64(), 2048, true)
                .is_ok()
        );
        pool.deallocate(allocation, layout);
    }
    assert!(pool.allocate(2049, limits).is_err());
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

#[def_test]
fn streaming_sync_retains_mapping_and_validates_original_arguments() {
    let context = crate::DmaDeviceContext::platform_default();
    let mut buffer = [3u8; 16];
    let pointer = NonNull::from(buffer.as_mut_slice());
    let direction = crate::DmaDirection::Bidirectional;
    // SAFETY: initialized buffer remains live and exclusive through matching unmap.
    let mapping = unsafe { crate::map_dma_buffer(&context, pointer, direction) }.unwrap();
    // SAFETY: simulated device owns the live bounce allocation and writes within its length.
    unsafe { mapping.cpu_addr.as_ptr().write(8) };
    // SAFETY: simulated device has stopped; the original mapping arguments hand ownership to CPU.
    unsafe {
        crate::sync_dma_buffer(
            &context,
            mapping.bus_addr,
            pointer,
            direction,
            crate::DmaSyncTarget::Cpu,
        )
    }
    .unwrap();
    assert_eq!(buffer[0], 8);
    buffer[1] = 9;
    assert_eq!(buffer[1], 9);
    // SAFETY: CPU accesses have stopped; the same active mapping is handed back to the device.
    unsafe {
        crate::sync_dma_buffer(
            &context,
            mapping.bus_addr,
            pointer,
            direction,
            crate::DmaSyncTarget::Device,
        )
    }
    .unwrap();
    // SAFETY: byte one is within the live 16-byte mapping.
    let second_byte = unsafe { mapping.cpu_addr.as_ptr().add(1) };
    // SAFETY: simulated device reads an initialized byte within the live mapping.
    let value = unsafe { second_byte.read() };
    assert_eq!(value, 9);
    let other_context = crate::DmaDeviceContext::platform_default();
    // SAFETY: buffer is live with both owners stopped; the other context rejects the foreign mapping before accessing bytes.
    let result = unsafe {
        crate::sync_dma_buffer(
            &other_context,
            mapping.bus_addr,
            pointer,
            direction,
            crate::DmaSyncTarget::Cpu,
        )
    };
    assert!(result.is_err());
    // SAFETY: both owners have stopped and this is the original live mapping, released exactly once.
    unsafe { crate::unmap_dma_buffer(&context, mapping.bus_addr, pointer, direction) };
}

#[def_test]
fn device_constraints_reject_invalid_masks_and_alignment() {
    let window = DmaWindow::new(0, 0, 0x1_0000_0000).unwrap();
    for limits in [
        DmaConstraints {
            streaming_mask: 0xff00,
            ..DmaConstraints::DMA32
        },
        DmaConstraints {
            coherent_mask: 0,
            ..DmaConstraints::DMA32
        },
        DmaConstraints {
            min_addr_align_bytes: 3,
            ..DmaConstraints::DMA32
        },
        DmaConstraints {
            segment_boundary_mask: 0xff00,
            ..DmaConstraints::DMA32
        },
        DmaConstraints {
            max_segment_len_bytes: 0,
            ..DmaConstraints::DMA32
        },
    ] {
        assert!(crate::DmaDeviceContext::new(limits, window).is_err());
    }
}
