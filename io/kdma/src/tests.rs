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
        let result =
            unsafe { allocator.map_with_policy(&limited, pa, direction, DmaMappingPolicy::Bounce) };
        assert!(result.is_err());
    }
    assert!(allocator.active_mappings.is_empty());
    // SAFETY: a remains live and exclusive until its matching unmap below.
    let (ma, _) =
        unsafe { allocator.map_with_policy(&first, pa, direction, DmaMappingPolicy::Bounce) }
            .unwrap();
    // SAFETY: b remains live and exclusive until its matching unmap below.
    let (mb, _) =
        unsafe { allocator.map_with_policy(&second, pb, direction, DmaMappingPolicy::Bounce) }
            .unwrap();
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

#[def_test]
fn direct_selection_requires_complete_reachable_cache_lines() {
    use crate::{DmaCacheOperation as Op, DmaCachePolicy as Policy};
    let device = DmaDeviceContext::platform_default();
    let flags = MappingFlags::READ | MappingFlags::WRITE;
    let policy = Policy::NonCoherent { alignment: 64 };
    assert!(
        direct_candidate(
            &device,
            0x1000,
            64,
            PhysAddr::from_usize(0x1000),
            flags,
            policy
        )
        .is_some()
    );
    assert!(
        direct_candidate(
            &device,
            0x1001,
            64,
            PhysAddr::from_usize(0x1001),
            flags,
            policy
        )
        .is_none()
    );
    assert!(
        direct_candidate(
            &device,
            0x1000,
            63,
            PhysAddr::from_usize(0x1000),
            flags,
            policy
        )
        .is_none()
    );
    assert!(
        direct_candidate(
            &device,
            0x1000,
            64,
            PhysAddr::from_usize(0x1000),
            flags | MappingFlags::DEVICE,
            policy
        )
        .is_none()
    );
    assert!(!crate::cache::can_direct(
        Policy::NonCoherent { alignment: 0 },
        0x1000,
        64
    ));
    assert!(!crate::cache::can_direct(
        Policy::NonCoherent { alignment: 63 },
        0x1000,
        64
    ));
    assert!(crate::cache::can_direct(Policy::Coherent, 0x1001, 63));
    assert!(!crate::cache::can_direct(Policy::Unsupported, 0x1000, 64));
    let narrow = DmaDeviceContext::new(0xffff, u64::MAX).unwrap();
    assert!(
        direct_candidate(
            &narrow,
            0x1000,
            64,
            PhysAddr::from_usize(0x1_0000),
            flags,
            policy
        )
        .is_none()
    );
    assert!(
        direct_candidate(
            &device,
            0x1000,
            64,
            PhysAddr::from_usize(0x1000),
            flags | MappingFlags::UNCACHED,
            Policy::Unsupported
        )
        .is_some()
    );
    for (direction, for_device, for_cpu) in [
        (DmaDirection::DriverToDevice, Op::Clean, None),
        (
            DmaDirection::DeviceToDriver,
            Op::CleanInvalidate,
            Some(Op::Invalidate),
        ),
        (
            DmaDirection::Bidirectional,
            Op::CleanInvalidate,
            Some(Op::Invalidate),
        ),
    ] {
        assert_eq!(
            crate::cache::operation(direction, DmaSyncTarget::Device),
            Some(for_device)
        );
        assert_eq!(
            crate::cache::operation(direction, DmaSyncTarget::Cpu),
            for_cpu
        );
    }
}

#[def_test]
fn failed_cache_handoff_retains_owner_and_repeated_sync_is_idempotent() {
    #[repr(align(64))]
    struct Buffer([u8; 64]);
    let mut buffer = Buffer([7; 64]);
    let mut mapping = ActiveMapping {
        original_cpu_addr: buffer.0.as_mut_ptr() as usize,
        len: buffer.0.len(),
        direction: DmaDirection::Bidirectional,
        owner: DmaSyncTarget::Cpu,
        backing: MappingBacking::Direct {
            cache_policy: DmaCachePolicy::NonCoherent { alignment: 64 },
        },
    };
    // SAFETY: the local buffer is initialized exclusive RAM; this injected hook
    // reports failure without retaining addresses or accessing memory.
    let failed =
        unsafe { mapping.sync_with(DmaSyncTarget::Device, |_, _, _| Err(AllocError::NoMemory)) };
    assert_eq!(failed, Err(AllocError::NoMemory));
    assert_eq!(mapping.owner, DmaSyncTarget::Cpu);
    let mut operations = alloc::vec::Vec::new();
    for target in [
        DmaSyncTarget::Device,
        DmaSyncTarget::Device,
        DmaSyncTarget::Cpu,
        DmaSyncTarget::Cpu,
    ] {
        // SAFETY: both simulated owners are stopped; the synchronous test hook
        // records maintenance for this exclusive live buffer and retains no pointer.
        unsafe {
            mapping.sync_with(target, |_, _, operation| {
                operations.push(operation);
                Ok(())
            })
        }
        .unwrap();
    }
    assert_eq!(
        operations,
        [
            crate::DmaCacheOperation::CleanInvalidate,
            crate::DmaCacheOperation::Invalidate
        ]
    );
    assert_eq!(mapping.owner, DmaSyncTarget::Cpu);
}

#[def_test]
fn coherent_allocation_and_forced_direct_mapping_keep_distinct_lifetimes() {
    let allocation =
        crate::CoherentAllocation::allocate(Layout::from_size_align(37, PAGE_SIZE_4K).unwrap())
            .unwrap();
    // SAFETY: allocation owns an initialized full rounded page; hardware has not seen it.
    let page = unsafe { core::slice::from_raw_parts(allocation.cpu_addr().as_ptr(), PAGE_SIZE_4K) };
    assert!(page.iter().all(|byte| *byte == 0));
    #[cfg(target_arch = "aarch64")]
    if kbuild_config::KFEAT_KVM_GUEST_MEM_SHARE {
        // SAFETY: this allocation is unpublished and has no active mapping or CPU alias.
        unsafe { allocation.free() };
        return unittest::TestResult::Ignored;
    }
    let pointer = NonNull::slice_from_raw_parts(allocation.cpu_addr(), 37);
    // SAFETY: allocation retains exclusive initialized uncached RAM and stable
    // attributes; it outlives the mapping and simulated hardware.
    let mapping = unsafe {
        crate::StreamingMapping::map(
            pointer,
            DmaDirection::DeviceToDriver,
            DmaMappingPolicy::Direct,
        )
    }
    .unwrap();
    assert_eq!(mapping.kind(), DmaMappingKind::Direct);
    assert_eq!(mapping.bus_addr(), allocation.bus_addr());
    // SAFETY: simulated device writes inside its direct DMA grant, with CPU accesses stopped.
    unsafe { mapping.cpu_addr().as_ptr().write(9) };
    // SAFETY: simulated hardware has stopped; original storage remains exclusively owned.
    unsafe { mapping.unmap() }.unwrap();
    // SAFETY: unmap returned ownership; the original allocation remains live.
    assert_eq!(unsafe { allocation.cpu_addr().as_ptr().read() }, 9);
    // SAFETY: mapping and CPU aliases have retired, and simulated hardware is stopped.
    unsafe { allocation.free() };
}

#[def_test]
fn cacheable_contiguous_pages_use_direct_without_changing_attributes() {
    if DmaCacheIf::policy() == DmaCachePolicy::Unsupported {
        return unittest::TestResult::Ignored;
    }
    #[cfg(target_arch = "aarch64")]
    if kbuild_config::KFEAT_KVM_GUEST_MEM_SHARE {
        return unittest::TestResult::Ignored;
    }
    let address = global_allocator()
        .alloc_pages(2, PAGE_SIZE_4K, UsageKind::Dma)
        .unwrap();
    let pointer = NonNull::new(address as *mut u8).unwrap();
    let len = 2 * PAGE_SIZE_4K;
    // SAFETY: fresh contiguous pages cover len writable bytes and have no other owner.
    unsafe { pointer.as_ptr().write_bytes(7, len) };
    let before = DmaPageTableIf::query_contiguous(va!(address), len).unwrap();
    assert!(!before.1.contains(MappingFlags::UNCACHED));
    // SAFETY: allocated full pages are initialized, pinned and exclusive with
    // stable attributes until the matching unmap and page release below.
    let mut mapping = unsafe {
        crate::StreamingMapping::map(
            NonNull::slice_from_raw_parts(pointer, len),
            DmaDirection::Bidirectional,
            DmaMappingPolicy::Auto,
        )
    }
    .unwrap();
    assert_eq!(mapping.kind(), DmaMappingKind::Direct);
    assert_eq!(mapping.bus_addr(), crate::p2b(before.0));
    // SAFETY: test simulates a device write inside the exclusive direct grant.
    unsafe { pointer.as_ptr().write(9) };
    if matches!(DmaCacheIf::policy(), DmaCachePolicy::NonCoherent { .. }) {
        // SAFETY: simulated device wrote through the CPU mapping, so write that
        // test data to PoC before exercising CPU-side invalidation; both owners are stopped.
        unsafe { crate::cache::maintain(va!(address), len, crate::DmaCacheOperation::Clean) }
            .unwrap();
    }
    // SAFETY: simulated hardware is stopped, and original pages remain exclusive and live.
    unsafe { mapping.sync_for_cpu() }.unwrap();
    // SAFETY: CPU ownership was restored for these initialized live pages.
    assert_eq!(unsafe { pointer.as_ptr().read() }, 9);
    // SAFETY: CPU owns the buffer, hardware remains stopped, and these are live writable pages.
    unsafe { pointer.as_ptr().write(8) };
    // SAFETY: CPU writes stopped before handing the original mapping to simulated hardware.
    unsafe { mapping.sync_for_device() }.unwrap();
    // SAFETY: both owners stopped; unmap returns final CPU ownership and retires the record.
    unsafe { mapping.unmap() }.unwrap();
    assert_eq!(
        DmaPageTableIf::query_contiguous(va!(address), len).unwrap(),
        before
    );
    // SAFETY: the pages are still live and exclusively CPU-owned after unmap.
    assert_eq!(unsafe { pointer.as_ptr().read() }, 8);
    global_allocator().dealloc_pages(address, 2, UsageKind::Dma);
}

#[def_test]
fn forced_bounce_sync_and_unmap_preserve_cpu_edits_after_handoff() {
    let mut original = [7u8; 16];
    let pointer = NonNull::from(original.as_mut_slice());
    // SAFETY: original is initialized and exclusive through matching unmap;
    // simulated device and CPU accesses follow explicit ownership transitions.
    let mut mapping = unsafe {
        crate::StreamingMapping::map(
            pointer,
            DmaDirection::Bidirectional,
            DmaMappingPolicy::Bounce,
        )
    }
    .unwrap();
    assert_eq!(mapping.kind(), DmaMappingKind::Bounce);
    // SAFETY: simulated device has stopped; mapping retains original initialized storage.
    unsafe { mapping.sync_for_cpu() }.unwrap();
    original[0] = 9;
    // SAFETY: repeated CPU sync retains the same stopped hardware and live storage.
    unsafe { mapping.sync_for_cpu() }.unwrap();
    assert_eq!(original[0], 9);
    // SAFETY: CPU edits are complete; simulated hardware remains stopped until handoff returns.
    unsafe { mapping.sync_for_device() }.unwrap();
    // SAFETY: simulated hardware is stopped and storage remains initialized and exclusive.
    unsafe { mapping.sync_for_cpu() }.unwrap();
    assert_eq!(original[0], 9);
    original[1] = 8;
    // SAFETY: both owners stopped; CPU owns the latest bytes, which unmap must retain.
    unsafe { mapping.unmap() }.unwrap();
    assert_eq!(original, [9, 8, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
}
