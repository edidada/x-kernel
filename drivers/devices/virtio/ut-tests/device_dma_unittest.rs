// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-instance OS DMA guard ownership and real vendor queue rollback.
//!
//! All mock device accesses are synchronous. Relaxed counters record resource
//! accounting only; they are not publication or task-synchronization flags.
//! Receive bounce buffers start as copies of the initialized original bytes,
//! so unpublished rollback and unwritten tails survive ordinary guard release.

use alloc::{
    alloc::{alloc_zeroed, dealloc},
    boxed::Box,
    sync::Arc,
    vec::Vec,
};
use core::{
    alloc::Layout,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use driver_os_api::{CoherentMem, DeviceDma, DmaDirection, ResError, ResResult, StreamingMapping};
use kspin::{SpinNoIrq, SpinNoPreempt};
use unittest::{assert, assert_eq, def_test};
use virtio_drivers::{BufferDirection, DmaBackend, queue::VirtQueue, transport::Transport};

use crate::{
    device_dma::DeviceDmaBackend,
    mock_virtio::{MockHal, MockQueue, MockTransport},
};

#[derive(Default)]
struct Counts {
    allocation_calls: AtomicUsize,
    allocations: AtomicUsize,
    frees: AtomicUsize,
    share_calls: AtomicUsize,
    mappings: AtomicUsize,
    unmaps: AtomicUsize,
    fail_allocation: AtomicUsize,
    fail_share: AtomicUsize,
    has_zero_bus: AtomicBool,
    released_directions: SpinNoPreempt<Vec<DmaDirection>>,
}

struct MockDma(Arc<Counts>);

// SAFETY: Native allocations are zeroed, exclusive, and Layout-aligned. Each
// guard owns its exact deallocator. Streaming buffers are initialized copies;
// their unique release hooks copy back only receive directions and free once.
// Callers retain original buffers until release; tests never run real hardware.
unsafe impl DeviceDma for MockDma {
    fn alloc_coherent(&self, len: usize, align: usize) -> ResResult<CoherentMem> {
        let call = self.0.allocation_calls.fetch_add(1, Ordering::Relaxed) + 1;
        if call == self.0.fail_allocation.load(Ordering::Relaxed) {
            return Err(ResError::NoMemory);
        }
        let layout = Layout::from_size_align(len, align).map_err(|_| ResError::InvalidResource)?;
        if len == 0 {
            return Err(ResError::InvalidResource);
        }
        // SAFETY: layout has nonzero size and valid alignment, checked above.
        let pointer = unsafe { alloc_zeroed(layout) };
        let pointer = NonNull::new(pointer).ok_or(ResError::NoMemory)?;
        let cpu = pointer.as_ptr() as usize;
        let bus = if self.0.has_zero_bus.load(Ordering::Relaxed) {
            0
        } else {
            cpu as u64
        };
        self.0.allocations.fetch_add(1, Ordering::Relaxed);
        let counts = self.0.clone();
        Ok(CoherentMem::new(
            cpu,
            bus,
            len,
            Some(Arc::new(move || {
                // SAFETY: This sole guard owns the unchanged alloc_zeroed pointer
                // and Layout; all queue/device accesses have ended before release.
                unsafe { dealloc(cpu as *mut u8, layout) };
                counts.frees.fetch_add(1, Ordering::Relaxed);
            })),
        ))
    }

    unsafe fn map_streaming(
        &self,
        buffer: NonNull<[u8]>,
        direction: DmaDirection,
    ) -> ResResult<StreamingMapping> {
        let call = self.0.share_calls.fetch_add(1, Ordering::Relaxed) + 1;
        if call == self.0.fail_share.load(Ordering::Relaxed) {
            return Err(ResError::NoMemory);
        }
        let original = buffer.as_ptr() as *mut u8 as usize;
        let len = buffer.len();
        if len == 0 {
            return Err(ResError::InvalidResource);
        }
        // SAFETY: DeviceDma's caller supplies initialized bytes, exclusive and
        // live through release. This temporary shared borrow ends before return.
        let bounce = unsafe { buffer.as_ref() }.to_vec().into_boxed_slice();
        let bounce_cpu = Box::into_raw(bounce) as *mut u8 as usize;
        let bus = if self.0.has_zero_bus.load(Ordering::Relaxed) {
            0
        } else {
            bounce_cpu as u64
        };
        let counts = self.0.clone();
        counts.mappings.fetch_add(1, Ordering::Relaxed);
        let release = Arc::new(move || {
            if direction != DmaDirection::DriverToDevice {
                // SAFETY: Original storage remains exclusive and live for len
                // bytes; the initialized bounce allocation is disjoint. Mock
                // device writes ended before this unique release hook runs.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        bounce_cpu as *const u8,
                        original as *mut u8,
                        len,
                    )
                };
            }
            let raw_slice = core::ptr::slice_from_raw_parts_mut(bounce_cpu as *mut u8, len);
            // SAFETY: This is the exact pointer and length from Box::into_raw.
            // The unique mapping guard invokes this hook once, after mock device
            // writes and copyback end; reconstructing the Box frees it once.
            drop(unsafe { Box::from_raw(raw_slice) });
            counts.released_directions.lock().push(direction);
            counts.unmaps.fetch_add(1, Ordering::Relaxed);
        });
        // SAFETY: The retained bounce allocation spans len bytes at bus, and
        // its unique hook performs the matching direction-aware copyback once.
        Ok(unsafe { StreamingMapping::new(bus, len, release) })
    }
}

fn backend(counts: &Arc<Counts>) -> DeviceDmaBackend {
    // SAFETY: This private mock provides coherent queue memory and initialized
    // streaming buffers; all tests stop device access before releasing tuples.
    // Kernel heap allocation and spin-locked accounting are non-sleeping and
    // IRQ-legal. The mock has no IOMMU: direct mock bus addresses have identical
    // meaning in both platform modes. Zero-bus tests do not emulate device access.
    unsafe { DeviceDmaBackend::new(Arc::new(MockDma(counts.clone()))) }
}

#[def_test]
fn allocation_can_be_released_by_clone_and_zero_bus_is_valid() {
    let counts = Arc::new(Counts::default());
    counts.has_zero_bus.store(true, Ordering::Relaxed);
    let dma = backend(&counts);
    let clone = dma.clone();
    let allocation = dma.allocate(2, BufferDirection::Both, true).unwrap();
    drop(dma);
    let retained = counts.frees.load(Ordering::Relaxed);
    // SAFETY: The clone shares the allocating instance; this exact allocation
    // was never published or dereferenced and is released once with its tuple.
    unsafe { clone.deallocate(allocation.0, allocation.1, 2, BufferDirection::Both, true) };
    drop(clone);
    assert_eq!(allocation.0, 0);
    assert_eq!(retained, 0);
    assert_eq!(counts.allocations.load(Ordering::Relaxed), 1);
    assert_eq!(counts.frees.load(Ordering::Relaxed), 1);
}

#[def_test]
fn allocation_validation_and_provider_failure_leave_no_stale_guard() {
    let counts = Arc::new(Counts::default());
    let dma = backend(&counts);
    let zero = dma.allocate(0, BufferDirection::Both, false);
    let overflow = dma.allocate(usize::MAX, BufferDirection::Both, false);
    let calls_after_invalid = counts.allocation_calls.load(Ordering::Relaxed);
    counts
        .fail_allocation
        .store(calls_after_invalid + 1, Ordering::Relaxed);
    let failed = dma.allocate(1, BufferDirection::Both, false);
    let allocation = dma.allocate(1, BufferDirection::Both, false).unwrap();
    // SAFETY: This is the sole live successful allocation and its exact tuple.
    unsafe { dma.deallocate(allocation.0, allocation.1, 1, BufferDirection::Both, false) };
    drop(dma);
    assert!(zero.is_err() && overflow.is_err() && failed.is_err());
    assert_eq!(calls_after_invalid, 0);
    assert_eq!(counts.allocations.load(Ordering::Relaxed), 1);
    assert_eq!(counts.frees.load(Ordering::Relaxed), 1);
}

#[def_test]
fn streaming_clone_release_preserves_unwritten_bytes_and_direction() {
    for (direction, os_direction) in [
        (
            BufferDirection::DriverToDevice,
            DmaDirection::DriverToDevice,
        ),
        (
            BufferDirection::DeviceToDriver,
            DmaDirection::DeviceToDriver,
        ),
        (BufferDirection::Both, DmaDirection::Bidirectional),
    ] {
        let counts = Arc::new(Counts::default());
        let dma = backend(&counts);
        let clone = dma.clone();
        let mut bytes = [0x55_u8; 16];
        let buffer = NonNull::from(&mut bytes[..]);
        // SAFETY: bytes stays initialized, live and exclusive through unshare;
        // no CPU access to original storage occurs while mapped.
        let address = unsafe { dma.share(buffer, direction, true) }.unwrap();
        if direction != BufferDirection::DriverToDevice {
            // SAFETY: Mock bus addresses are live bounce pointers. Offset zero
            // lies in the 16-byte mapping; this is the sole mock-device write.
            unsafe { (address as *mut u8).write(0xa7) };
        }
        drop(dma);
        let retained = counts.unmaps.load(Ordering::Relaxed);
        // SAFETY: Device access has stopped; clone releases the same live tuple
        // once while original bytes remain exclusively owned by this test.
        unsafe { clone.unshare(address, buffer, direction, true) };
        drop(clone);
        let mut expected = [0x55; 16];
        if direction != BufferDirection::DriverToDevice {
            expected[0] = 0xa7;
        }
        let directions = counts.released_directions.lock().clone();
        assert_eq!(bytes, expected);
        assert_eq!(retained, 0);
        assert_eq!(counts.mappings.load(Ordering::Relaxed), 1);
        assert_eq!(counts.unmaps.load(Ordering::Relaxed), 1);
        assert_eq!(directions.as_slice(), &[os_direction]);
    }
}

#[def_test]
fn same_bus_allocations_are_isolated_between_instances_and_their_clones() {
    let first_counts = Arc::new(Counts::default());
    let second_counts = Arc::new(Counts::default());
    first_counts.has_zero_bus.store(true, Ordering::Relaxed);
    second_counts.has_zero_bus.store(true, Ordering::Relaxed);
    let first = backend(&first_counts);
    let second = backend(&second_counts);
    let first_clone = first.clone();
    let second_clone = second.clone();
    let first_tuple = first.allocate(1, BufferDirection::Both, false).unwrap();
    let second_tuple = second.allocate(1, BufferDirection::Both, false).unwrap();
    let same_bus_distinct_cpu = first_tuple.0 == second_tuple.0 && first_tuple.1 != second_tuple.1;
    drop(first);
    drop(second);
    // SAFETY: The first instance's clone consumes its unchanged live tuple once.
    unsafe {
        first_clone.deallocate(
            first_tuple.0,
            first_tuple.1,
            1,
            BufferDirection::Both,
            false,
        )
    };
    let first_after_release = first_counts.frees.load(Ordering::Relaxed);
    let second_still_retained = second_counts.frees.load(Ordering::Relaxed);
    drop(first_clone);
    // SAFETY: This second instance's live allocation is released through its
    // own clone using the exact tuple, independently of the first instance.
    unsafe {
        second_clone.deallocate(
            second_tuple.0,
            second_tuple.1,
            1,
            BufferDirection::Both,
            false,
        )
    };
    drop(second_clone);
    assert!(same_bus_distinct_cpu);
    assert_eq!(first_after_release, 1);
    assert_eq!(second_still_retained, 0);
    assert_eq!(first_counts.frees.load(Ordering::Relaxed), 1);
    assert_eq!(second_counts.frees.load(Ordering::Relaxed), 1);
}

#[def_test]
fn streaming_provider_failure_leaves_no_guard_and_zero_bus_can_be_retried() {
    let counts = Arc::new(Counts::default());
    counts.fail_share.store(1, Ordering::Relaxed);
    counts.has_zero_bus.store(true, Ordering::Relaxed);
    let dma = backend(&counts);
    let mut bytes = [0x62_u8; 32];
    let buffer = NonNull::from(&mut bytes[..]);
    // SAFETY: Initialized original bytes remain live and exclusively held;
    // the failing provider retains nothing and performs no device access.
    let failed = unsafe { dma.share(buffer, BufferDirection::DeviceToDriver, false) };
    let retained_after_failure = counts.mappings.load(Ordering::Relaxed);
    // SAFETY: The same initialized buffer is retried and remains untouched
    // until matching unshare; address zero is never dereferenced by this test.
    let address = unsafe { dma.share(buffer, BufferDirection::DeviceToDriver, false) }.unwrap();
    // SAFETY: No device access occurred; all arguments are the successful
    // mapping's original tuple and original storage is still live/exclusive.
    unsafe { dma.unshare(address, buffer, BufferDirection::DeviceToDriver, false) };
    drop(dma);
    assert!(failed.is_err());
    assert_eq!(retained_after_failure, 0);
    assert_eq!(address, 0);
    assert_eq!(bytes, [0x62; 32]);
    assert_eq!(counts.mappings.load(Ordering::Relaxed), 1);
    assert_eq!(counts.unmaps.load(Ordering::Relaxed), 1);
}

type Queue = VirtQueue<MockHal, 8, DeviceDmaBackend>;

/// Complete a published mock request without writing its receive buffers.
///
/// # Safety
/// rings must identify this queue's live, aligned allocations and token its
/// sole outstanding request. outputs must be that request's retained buffers.
/// No other CPU or device may access the rings during this emulation.
unsafe fn complete<'buffer>(
    queue: &mut Queue,
    rings: &MockQueue,
    token: u16,
    outputs: &'buffer mut [&'buffer mut [u8]],
) -> virtio_drivers::Result<u32> {
    let used = rings.used as *mut u8;
    // SAFETY: The live used ring has an aligned u16 index at offset two.
    let index = unsafe { used.wrapping_add(2).cast::<u16>().read_volatile() };
    let slot = used
        .wrapping_add(4 + usize::from(index % 8) * 8)
        .cast::<u32>();
    // SAFETY: This bounded ring slot has an aligned u32 descriptor id.
    unsafe { slot.write_volatile(u32::from(token)) };
    // SAFETY: The next u32 in this same live used element is its length.
    unsafe { slot.wrapping_add(1).write_volatile(0) };
    // SAFETY: Publishing the used index completes this sole outstanding token.
    unsafe {
        used.wrapping_add(2)
            .cast::<u16>()
            .write_volatile(index.wrapping_add(1))
    };
    // SAFETY: The emulated device is stopped and these are the original outputs.
    unsafe { queue.pop_used(token, &[], outputs) }
}

#[def_test]
fn real_queue_direct_and_indirect_share_failures_roll_back_initialized_rx() {
    for (indirect, fail_share) in [(false, 2), (true, 2), (true, 3)] {
        let counts = Arc::new(Counts::default());
        counts.fail_share.store(fail_share, Ordering::Relaxed);
        let dma = backend(&counts);
        let rings = Arc::new(SpinNoIrq::new(MockQueue::default()));
        let mut transport = MockTransport::new();
        transport.queue = Some(rings.clone());
        let mut queue =
            Queue::new_with_dma(&mut transport, 0, indirect, false, true, dma.clone()).unwrap();
        let available_before = queue.available_desc();
        let mut first = [0x39_u8; 19];
        let mut second = [0x72_u8; 27];
        let result = {
            let mut outputs: [&mut [u8]; 2] = [&mut first, &mut second];
            // SAFETY: Both initialized outputs remain live and exclusive until
            // rollback or the explicit completion below; no real device runs.
            let result = unsafe { queue.add(&[], &mut outputs) };
            if let Ok(token) = result {
                // SAFETY: Unexpected success still owns the live queue buffers;
                // reclaim them before reporting the failed test expectation.
                let _ = unsafe { complete(&mut queue, &rings.lock(), token, &mut outputs) };
            }
            result
        };
        let calls_after_failure = counts.share_calls.load(Ordering::Relaxed);
        let mapped_after_failure = counts.mappings.load(Ordering::Relaxed);
        let unmapped_after_failure = counts.unmaps.load(Ordering::Relaxed);
        let available_after_failure = queue.available_desc();
        let available_ring = rings.lock().available as *const u16;
        // SAFETY: The live queue owns the page-aligned available ring; index
        // is its second u16 and no emulated device accesses this queue now.
        let published_after_failure = unsafe { available_ring.wrapping_add(1).read_volatile() };
        let unchanged_after_failure = first == [0x39; 19] && second == [0x72; 27];
        let retry_result = {
            let mut outputs: [&mut [u8]; 2] = [&mut first, &mut second];
            // SAFETY: Failed submission has relinquished its buffers; these
            // initialized outputs stay exclusive through completion below.
            let retry = unsafe { queue.add(&[], &mut outputs) };
            retry.and_then(|token| {
                // SAFETY: This token is the only outstanding request and the
                // supplied outputs are retained unchanged until pop_used.
                unsafe { complete(&mut queue, &rings.lock(), token, &mut outputs) }
            })
        };
        let available_after_retry = queue.available_desc();
        transport.queue_unset(0);
        drop(queue);
        drop(dma);
        assert!(result.is_err());
        assert_eq!(calls_after_failure, fail_share);
        assert_eq!(mapped_after_failure, fail_share - 1);
        assert_eq!(unmapped_after_failure, mapped_after_failure);
        assert!(unchanged_after_failure);
        assert_eq!(available_after_failure, available_before);
        assert_eq!(published_after_failure, 0);
        assert_eq!(retry_result, Ok(0));
        assert_eq!(available_after_retry, available_before);
        assert_eq!(first, [0x39; 19]);
        assert_eq!(second, [0x72; 27]);
        assert_eq!(
            counts.mappings.load(Ordering::Relaxed),
            counts.unmaps.load(Ordering::Relaxed)
        );
        assert_eq!(
            counts.allocations.load(Ordering::Relaxed),
            counts.frees.load(Ordering::Relaxed)
        );
    }
}

#[def_test]
fn real_queue_second_ring_allocation_failure_releases_first_ring() {
    let counts = Arc::new(Counts::default());
    counts.fail_allocation.store(2, Ordering::Relaxed);
    let dma = backend(&counts);
    let rings = Arc::new(SpinNoIrq::new(MockQueue::default()));
    let mut transport = MockTransport::new();
    transport.queue = Some(rings.clone());
    let result = Queue::new_with_dma(&mut transport, 0, false, false, true, dma.clone());
    let failed = result.is_err();
    drop(result);
    drop(dma);
    let was_unpublished = rings.lock().size == 0;
    assert!(failed && was_unpublished);
    assert_eq!(counts.allocation_calls.load(Ordering::Relaxed), 2);
    assert_eq!(counts.allocations.load(Ordering::Relaxed), 1);
    assert_eq!(counts.frees.load(Ordering::Relaxed), 1);
}
