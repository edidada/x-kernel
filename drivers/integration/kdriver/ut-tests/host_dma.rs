// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use driver_model::DmaWindow;
use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn host_dma_publication_releases_original_token_exactly_once() {
    for should_publish in [false, true] {
        let mut storage = alloc::vec![0xa5u8; 8192];
        let cpu = NonNull::from(&mut storage[0]);
        let physical = v2p(VirtAddr::from_usize(cpu.as_ptr() as usize)).as_usize() as u64;
        let records = Arc::new(kspin::SpinNoIrq::new(alloc::vec::Vec::with_capacity(1)));
        let observed = records.clone();
        let native_bus = 0xdead_beef;
        let allocation = NativeAllocation {
            native: kdma::DmaInfo {
                cpu_addr: cpu,
                bus_addr: kdma::DmaBusAddress::new(native_bus),
            },
            layout: Layout::from_size_align(4097, 1).unwrap(),
            release: Some(move |native: kdma::DmaInfo, layout: Layout| {
                observed.lock().push((
                    native.cpu_addr.as_ptr() as usize,
                    native.bus_addr.as_u64(),
                    layout.size(),
                    layout.align(),
                ));
            }),
        };
        let view = DmaInfo {
            window: DmaWindow::new(physical, 0x4000, if should_publish { 8192 } else { 4097 })
                .unwrap(),
            ..DmaInfo::DEFAULT
        };
        let result = publish(&view, allocation);
        if should_publish {
            let memory = result.unwrap();
            assert_eq!(memory.bus_addr(), 0x4000);
            assert_eq!(records.lock().len(), 0);
            assert!(storage.iter().all(|byte| *byte == 0));
            drop(memory);
        } else {
            assert_eq!(result.unwrap_err(), RuntimeError::NoMemory);
            assert!(storage.iter().all(|byte| *byte == 0xa5));
        }
        let recorded = records.lock();
        assert_eq!(
            recorded.as_slice(),
            &[(cpu.as_ptr() as usize, native_bus, 4097, 1)]
        );
    }
}

#[def_test]
fn host_dma_rejects_missing_unsupported_and_invalid_configuration() {
    assert_eq!(
        allocate_for_view(None, 4096, 4096).unwrap_err(),
        RuntimeError::Unsupported
    );
    let noncoherent = DmaInfo {
        coherent: false,
        ..DmaInfo::DEFAULT
    };
    assert_eq!(
        allocate_for_view(Some(&noncoherent), 4096, 4096).unwrap_err(),
        RuntimeError::Unsupported
    );
    for (len, align) in [(0, 4096), (4096, 0), (4096, 3), (usize::MAX, 4096)] {
        assert_eq!(
            allocate_for_view(Some(&DmaInfo::DEFAULT), len, align).unwrap_err(),
            RuntimeError::Invalid
        );
    }
    let invalid = DmaInfo {
        mask: 0x17ff,
        ..DmaInfo::DEFAULT
    };
    assert_eq!(
        allocate_for_view(Some(&invalid), 4096, 4096).unwrap_err(),
        RuntimeError::Invalid
    );
}

#[def_test]
fn host_dma_translation_checks_full_span_mask_and_bus_alignment() {
    let view = DmaInfo {
        mask: 0x7fff,
        window: DmaWindow::new(0x10000, 0x4000, 0x8000).unwrap(),
        coherent: true,
    };
    assert_eq!(translate(&view, 0x10000, 8192, 4096), Ok(0x4000));
    assert_eq!(translate(&view, 0x13000, 4096, 4096), Ok(0x7000));
    assert_eq!(
        translate(&view, 0x13000, 8192, 4096),
        Err(RuntimeError::NoMemory)
    );
    assert_eq!(
        translate(&view, 0xf000, 4096, 4096),
        Err(RuntimeError::NoMemory)
    );
    let short = DmaInfo {
        window: DmaWindow::new(0x10000, 0x4000, 4097).unwrap(),
        ..view
    };
    assert_eq!(
        translate(&short, 0x10000, 8192, 4096),
        Err(RuntimeError::NoMemory)
    );
    let unaligned = DmaInfo {
        window: DmaWindow::new(0x10000, 0x4001, 8192).unwrap(),
        ..view
    };
    assert_eq!(
        translate(&unaligned, 0x10000, 4096, 4096),
        Err(RuntimeError::NoMemory)
    );
}

#[def_test]
fn host_dma_real_allocations_keep_independent_views_and_zero_padding() {
    let first_view = DmaInfo {
        window: DmaWindow::new(0, 0x1_0000_0000, 0x1_0000_0000).unwrap(),
        ..DmaInfo::DEFAULT
    };
    let second_view = DmaInfo {
        window: DmaWindow::new(0, 0x2_0000_0000, 0x1_0000_0000).unwrap(),
        ..DmaInfo::DEFAULT
    };
    let first = allocate_for_view(Some(&first_view), 4097, 8192).unwrap();
    let second = allocate_for_view(Some(&second_view), 4097, 8192).unwrap();
    assert!(first.as_ptr() != second.as_ptr());
    for (memory, view) in [(&first, &first_view), (&second, &second_view)] {
        let physical = v2p(VirtAddr::from_usize(memory.as_ptr() as usize)).as_usize() as u64;
        assert_eq!(
            memory.bus_addr(),
            view.translate_range(physical, 8192).unwrap()
        );
        assert_eq!(memory.bus_addr() & 8191, 0);
        assert_eq!(memory.len(), 4097);
        // SAFETY: this test owns the live allocation, which kdma rounds to
        // two pages; neither pointer nor bus address is given to hardware.
        let bytes = unsafe { core::slice::from_raw_parts(memory.as_ptr(), 8192) };
        assert!(bytes.iter().all(|byte| *byte == 0));
    }
    drop(first);
    drop(second);
}

#[def_test]
fn host_dma_rejects_native_allocation_outside_device_window() {
    let view = DmaInfo {
        window: DmaWindow::new(u64::MAX - 4095, 0, 4096).unwrap(),
        ..DmaInfo::DEFAULT
    };
    for _ in 0..4 {
        assert_eq!(
            allocate_for_view(Some(&view), 4096, 4096).unwrap_err(),
            RuntimeError::NoMemory
        );
    }
    let memory = allocate_for_view(Some(&DmaInfo::DEFAULT), 4096, 4096).unwrap();
    assert_eq!(memory.len(), 4096);
}

#[def_test(serial)]
fn streaming_guard_translates_bounce_and_releases_original_native_mapping() {
    for direction in [
        kdma::DmaDirection::DriverToDevice,
        kdma::DmaDirection::DeviceToDriver,
        kdma::DmaDirection::Bidirectional,
    ] {
        let mut original = [0xa5u8; 64];
        let buffer = NonNull::from(original.as_mut_slice());
        // SAFETY: the original array is exclusively held until the returned
        // guard is dropped; this test never submits the address to hardware.
        let native = unsafe { kdma::map_dma_buffer(buffer, direction) }.unwrap();
        let physical =
            v2p(VirtAddr::from_usize(native.cpu_addr.as_ptr() as usize)).as_usize() as u64;
        let view = DmaInfo {
            window: DmaWindow::new(physical, 0x2000, 64).unwrap(),
            ..DmaInfo::DEFAULT
        };
        // SAFETY: the live native bounce allocation has 64 exclusive bytes;
        // simulated device writes do not touch the original caller buffer.
        unsafe { native.cpu_addr.as_ptr().write_bytes(0x37, 64) };
        // SAFETY: ownership of this active unpublished native mapping is
        // transferred exactly once; the original buffer remains live.
        let mapping = unsafe { publish_streaming(&view, native, buffer, direction) }.unwrap();
        let observed = (mapping.bus_addr(), mapping.len());
        drop(mapping);
        let expected = if direction == kdma::DmaDirection::DriverToDevice {
            0xa5
        } else {
            0x37
        };
        assert_eq!(observed, (0x2000, 64));
        assert_eq!(original, [expected; 64]);
    }
}

#[def_test]
fn streaming_acquisition_rejects_unsupported_views_and_empty_buffers() {
    let noncoherent = DmaInfo {
        coherent: false,
        ..DmaInfo::DEFAULT
    };
    let invalid_mask = DmaInfo {
        mask: 0x17ff,
        ..DmaInfo::DEFAULT
    };
    for (view, len, expected) in [
        (None, 64, driver_os_api::ResError::Unsupported),
        (Some(&noncoherent), 64, driver_os_api::ResError::Unsupported),
        (
            Some(&invalid_mask),
            64,
            driver_os_api::ResError::InvalidResource,
        ),
        (
            Some(&DmaInfo::DEFAULT),
            0,
            driver_os_api::ResError::InvalidResource,
        ),
    ] {
        let mut original = [0xa5u8; 64];
        let buffer = NonNull::from(&mut original[..len]);
        // SAFETY: the local buffer stays exclusive and live through rejection
        // or immediate guard cleanup; no device receives the mapping.
        let result = unsafe {
            map_streaming_for_view(view, buffer, driver_os_api::DmaDirection::DriverToDevice)
        };
        let error = result.err();
        assert_eq!(error, Some(expected));
        assert_eq!(original, [0xa5; 64]);
    }
}

#[def_test(serial)]
fn rejected_streaming_publication_preserves_bytes_and_recycles_bounce_capacity() {
    for direction in [
        kdma::DmaDirection::DriverToDevice,
        kdma::DmaDirection::DeviceToDriver,
        kdma::DmaDirection::Bidirectional,
    ] {
        let mut original = alloc::vec![0xa5u8; 65536];
        for iteration in 0..80 {
            let buffer = NonNull::from(original.as_mut_slice());
            // SAFETY: the buffer stays live and exclusive until publication
            // rejects and discards this never-submitted mapping.
            let native = unsafe { kdma::map_dma_buffer(buffer, direction) }.unwrap();
            let physical =
                v2p(VirtAddr::from_usize(native.cpu_addr.as_ptr() as usize)).as_usize() as u64;
            let view = if iteration % 2 == 0 {
                DmaInfo {
                    window: DmaWindow::new(physical, 0, 65535).unwrap(),
                    ..DmaInfo::DEFAULT
                }
            } else {
                DmaInfo {
                    window: DmaWindow::new(physical, 1, 65536).unwrap(),
                    mask: 0xffff,
                    coherent: true,
                }
            };
            // SAFETY: the native mapping owns a distinct writable bounce
            // buffer; dirtying it detects accidental receive copy-back.
            unsafe { native.cpu_addr.as_ptr().write_bytes(0x37, 65536) };
            // SAFETY: the mapping is active, never submitted, and consumed
            // once; rejection must discard it before the original is read.
            let result = unsafe { publish_streaming(&view, native, buffer, direction) };
            let error = result.err();
            assert_eq!(error, Some(driver_os_api::ResError::NoMemory));
            assert!(original.iter().all(|byte| *byte == 0xa5));
        }
    }
}

#[def_test(serial)]
fn receive_mapping_without_device_writes_preserves_original_bytes() {
    let mut original = [0xa5u8; 64];
    let buffer = NonNull::from(original.as_mut_slice());
    // SAFETY: the original buffer remains live and exclusive through guard
    // cleanup. No device ever receives this mapping, so there are no writes.
    let mapping = unsafe {
        map_streaming_for_view(
            Some(&DmaInfo::DEFAULT),
            buffer,
            driver_os_api::DmaDirection::DeviceToDriver,
        )
    }
    .unwrap();
    drop(mapping);
    assert_eq!(original, [0xa5; 64]);
}

#[def_test(serial)]
fn receive_mapping_partial_device_write_preserves_unwritten_suffix() {
    let mut original = [0xa5u8; 64];
    let buffer = NonNull::from(original.as_mut_slice());
    let direction = kdma::DmaDirection::DeviceToDriver;
    // SAFETY: the initialized original remains live and exclusive through
    // publication and guard cleanup. No hardware receives this mapping.
    let native = unsafe { map_native_streaming(buffer, direction) }.unwrap();
    // SAFETY: native owns a distinct writable 64-byte bounce interval. Only
    // its first 13 bytes are changed to simulate a partial device write.
    unsafe { native.cpu_addr.as_ptr().write_bytes(0x37, 13) };
    // SAFETY: this exact unpublished mapping has fully initialized bounce
    // bytes and is consumed once; no device access needs to be stopped.
    let result = unsafe { publish_streaming(&DmaInfo::DEFAULT, native, buffer, direction) };
    let error = result.err();
    assert_eq!(error, None);
    assert_eq!(&original[..13], &[0x37; 13]);
    assert_eq!(&original[13..], &[0xa5; 51]);
}
