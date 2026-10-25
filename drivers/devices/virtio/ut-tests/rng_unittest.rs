// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use kspin::SpinNoIrq;
use unittest::{assert, assert_eq, assert_ne, def_test};
use virtio_drivers::{BufferDirection, PhysAddr, transport::DeviceType};

use super::*;
use crate::mock_virtio::{MockHal, MockQueue, MockReset, MockTransport};

static MAPPINGS: AtomicUsize = AtomicUsize::new(0);
static DMA_BLOCKS: AtomicUsize = AtomicUsize::new(0);
static BUFFER_ADDRESS: AtomicUsize = AtomicUsize::new(0);
struct TrackedHal;

// SAFETY: allocation/translation delegate to the identity-mapped test HAL.
// Serial tests have at most one active mapping in addition to retained leaks.
unsafe impl Hal for TrackedHal {
    fn dma_alloc(
        pages: usize,
        direction: BufferDirection,
        platform: bool,
    ) -> (PhysAddr, NonNull<u8>) {
        DMA_BLOCKS.fetch_add(1, Ordering::Relaxed);
        MockHal::dma_alloc(pages, direction, platform)
    }

    unsafe fn dma_dealloc(
        paddr: PhysAddr,
        vaddr: NonNull<u8>,
        pages: usize,
        platform: bool,
    ) -> i32 {
        DMA_BLOCKS.fetch_sub(1, Ordering::Relaxed);
        // SAFETY: forwards the exact live allocation tuple to its allocator.
        unsafe { MockHal::dma_dealloc(paddr, vaddr, pages, platform) }
    }

    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, size: usize) -> NonNull<u8> {
        // SAFETY: forwards the identity-mapped valid region supplied by the test.
        unsafe { MockHal::mmio_phys_to_virt(paddr, size) }
    }

    unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection, platform: bool) -> PhysAddr {
        core::assert_eq!(buffer.len(), MAX_REQUEST_BYTES);
        BUFFER_ADDRESS.store(buffer.as_ptr() as *mut u8 as usize, Ordering::Relaxed);
        MAPPINGS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: RngRequest retains this Box until successful reclamation.
        unsafe { MockHal::share(buffer, direction, platform) }
    }

    unsafe fn unshare(
        paddr: PhysAddr,
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        platform: bool,
    ) {
        core::assert_eq!(buffer.len(), MAX_REQUEST_BYTES);
        core::assert_eq!(
            buffer.as_ptr() as *mut u8 as usize,
            BUFFER_ADDRESS.load(Ordering::Relaxed)
        );
        MAPPINGS.fetch_sub(1, Ordering::Relaxed);
        // SAFETY: pop_used supplies the original tuple of the outstanding share.
        unsafe { MockHal::unshare(paddr, buffer, direction, platform) };
    }
}

type Request = RngRequest<TrackedHal, MockTransport>;

fn request(features: u64) -> (Request, Arc<SpinNoIrq<MockQueue>>) {
    let queue = Arc::new(SpinNoIrq::new(MockQueue::default()));
    let mut transport = MockTransport::new_with_type(DeviceType::EntropySource);
    transport.features = features;
    transport.queue = Some(queue.clone());
    (Request::new(transport).unwrap(), queue)
}

/// Emulate one device completion, optionally reporting a malformed used element.
///
/// # Safety
/// The queue must belong to a live (or deliberately retained) Request with one
/// uncompleted submission. The test must not access it concurrently.
unsafe fn finish(queue: &MockQueue, len: u32, is_wrong_token: bool) {
    core::assert_eq!(queue.size, 8);
    core::assert_ne!(queue.descriptors, 0);
    // The caller retains the published queue and outstanding Box. Ring slots
    // are bounded by depth. Writes are bounded by the descriptor, even when
    // testing a dishonest reported length. No CPU reader runs concurrently.
    let available = queue.available as *const u16;
    // SAFETY: the live available ring's `idx` is the aligned u16 at index 1.
    let index = unsafe { available.wrapping_add(1).read_volatile() }.wrapping_sub(1);
    // SAFETY: `index % size` selects an aligned u16 entry of the live ring.
    let token = unsafe {
        available
            .wrapping_add(2 + usize::from(index) % queue.size)
            .read_volatile()
    };
    core::assert!(usize::from(token) < queue.size);
    let descriptor = (queue.descriptors as *const u8).wrapping_add(usize::from(token) * 16);
    // SAFETY: `token` is bounded by depth; offset 0 is the descriptor's aligned u64 address.
    let address = unsafe { descriptor.cast::<u64>().read_volatile() } as *mut u8;
    // SAFETY: offset 8 of the same live descriptor is its aligned u32 length.
    let capacity = unsafe { descriptor.wrapping_add(8).cast::<u32>().read_volatile() };
    core::assert_eq!(capacity as usize, MAX_REQUEST_BYTES);
    // SAFETY: offset 12 of the same live descriptor is its aligned u16 flags.
    let flags = unsafe { descriptor.wrapping_add(12).cast::<u16>().read_volatile() };
    core::assert_eq!(flags, 2);
    for offset in 0..len.min(capacity) as usize {
        // SAFETY: `offset` is below the descriptor capacity of the retained Box.
        unsafe { address.wrapping_add(offset).write_volatile(offset as u8) };
    }
    let used = queue.used as *mut u8;
    // SAFETY: offset 2 of the live used ring is its aligned u16 `idx`.
    let used_index = unsafe { used.wrapping_add(2).cast::<u16>().read_volatile() };
    let slot = used
        .wrapping_add(4 + usize::from(used_index) % queue.size * 8)
        .cast::<u32>();
    // SAFETY: `slot` is the aligned id field of a used element bounded by depth.
    unsafe { slot.write_volatile(u32::from(token) + u32::from(is_wrong_token)) };
    // SAFETY: the length field directly follows the id in the same used element.
    unsafe { slot.wrapping_add(1).write_volatile(len) };
    core::sync::atomic::fence(Ordering::Release);
    // SAFETY: publish the element through the same aligned u16 used `idx`.
    unsafe {
        used.wrapping_add(2)
            .cast::<u16>()
            .write_volatile(used_index.wrapping_add(1))
    };
}

type Device = VirtIoRngDev<TrackedHal, MockTransport>;
type Action = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Completion {
    before: Option<DriverError>,
    after: Option<DriverError>,
    has_returned_error: AtomicBool,
    action: SpinNoIrq<Option<Action>>,
    on_notify: SpinNoIrq<Option<Action>>,
    notifications: AtomicUsize,
}
impl RngCompletion for Completion {
    fn wait_until(&self, ready: &mut dyn FnMut() -> bool) -> KResult {
        if let Some(error) = self.before {
            return Err(map_driver_error(error));
        }
        if ready() {
            return Ok(());
        }
        if let Some(error) = self.after
            && !self.has_returned_error.swap(true, Ordering::Relaxed)
        {
            return Err(map_driver_error(error));
        }
        let action = self
            .action
            .lock()
            .take()
            .expect("unplanned wait in controlled test");
        action();
        core::assert!(ready(), "IRQ must make the predicate ready");
        Ok(())
    }

    fn notify(&self) {
        self.notifications.fetch_add(1, Ordering::Relaxed);
        let action = self.on_notify.lock().take();
        if let Some(action) = action {
            action();
        }
    }
}
fn device(
    completion: Arc<Completion>,
) -> (Arc<Device>, Arc<SpinNoIrq<MockQueue>>, Arc<AtomicUsize>) {
    let queue = Arc::new(SpinNoIrq::new(MockQueue::default()));
    let mut transport = MockTransport::new_with_type(DeviceType::EntropySource);
    transport.queue = Some(queue.clone());
    let irq = transport.interrupt_status.clone();
    let device = Device::new(transport, completion).unwrap();
    (Arc::new(device), queue, irq)
}

#[def_test(serial)]
fn hwrng_irq_reclaims_before_waking_waiter() {
    let completion = Arc::new(Completion::default());
    let (device, queue, irq) = device(completion.clone());
    let mappings = MAPPINGS.load(Ordering::Relaxed);
    assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings);
    assert!(!device.handle(0).handled());
    let weak = Arc::downgrade(&device);
    *completion.action.lock() = Some(Box::new(move || {
        let device = weak.upgrade().unwrap();
        irq.store(2, Ordering::Relaxed);
        core::assert!(device.handle(0).handled()); // Configuration only.
        // SAFETY: the waiting-read predicate published one live request.
        unsafe { finish(&queue.lock(), 13, false) };
        irq.store(1, Ordering::Relaxed);
        core::assert!(device.handle(0).handled());
        core::assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings);
        irq.store(1, Ordering::Relaxed);
        core::assert!(device.handle(0).handled()); // No duplicate reclaim.
    }));
    let mut output = [0xff; 8];
    assert_eq!(device.read(&mut output), Ok(8));
    assert_eq!(output, [0, 1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(completion.notifications.load(Ordering::Relaxed), 1);
    assert_eq!(device.read(&mut output), Ok(5));
    assert_eq!(&output[..5], &[8, 9, 10, 11, 12]);
    assert_eq!(device.read(&mut []), Ok(0));
}

#[def_test(serial)]
fn hwrng_not_ready_preserves_request_until_later_completion() {
    let completion = Arc::new(Completion::default());
    let (device, queue, irq) = device(completion.clone());
    let mappings = MAPPINGS.load(Ordering::Relaxed);
    let weak = Arc::downgrade(&device);
    let queue_for_irq = queue.clone();
    let completion_for_irq = completion.clone();
    *completion.action.lock() = Some(Box::new(move || {
        let device = weak.upgrade().unwrap();
        for _ in 0..4 {
            irq.store(1, Ordering::Relaxed);
            core::assert!(device.handle(0).handled());
            core::assert!(!device.is_failed());
            core::assert!(matches!(
                device.request.lock().state,
                RequestState::Pending(_)
            ));
            core::assert_eq!(completion_for_irq.notifications.load(Ordering::Relaxed), 0);
            core::assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings + 1);
        }
        // SAFETY: empty notifications retained the original live submission.
        unsafe { finish(&queue_for_irq.lock(), 8, false) };
        irq.store(1, Ordering::Relaxed);
        device.handle(0);
    }));
    let mut output = [0xaa; 8];
    assert_eq!(device.read(&mut output), Ok(8));
    assert_eq!(output, [0, 1, 2, 3, 4, 5, 6, 7]);
    assert!(!device.is_failed());
    assert_eq!(completion.notifications.load(Ordering::Relaxed), 1);
    assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings);
    drop(device);
    assert_eq!(queue.lock().descriptors, 0);
}

#[def_test(serial)]
fn hwrng_wait_failure_preserves_dma_and_validates_before_consuming() {
    for error in [DriverError::InvalidInput, DriverError::NoMemory] {
        let completion = Arc::new(Completion {
            before: Some(error),
            ..Default::default()
        });
        let (device, queue, irq) = device(completion);
        let mappings = MAPPINGS.load(Ordering::Relaxed);
        assert_eq!(device.read(&mut [0; 8]), Err(map_driver_error(error)));
        assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings);
        device.request.lock().submit().unwrap();
        // SAFETY: the controlled test published one device-owned buffer.
        unsafe { finish(&queue.lock(), 8, false) };
        irq.store(1, Ordering::Relaxed);
        device.handle(0);
        assert_eq!(device.read(&mut [0; 8]), Err(map_driver_error(error)));
        assert_eq!(device.request.lock().read(&mut [0; 8]), 8);
    }
    let completion = Arc::new(Completion {
        after: Some(DriverError::NoMemory),
        ..Default::default()
    });
    let (device, queue, irq) = device(completion);
    let mut output = [0xaa; 8];
    let mappings = MAPPINGS.load(Ordering::Relaxed);
    assert_eq!(device.read(&mut output), Err(KError::NoMemory));
    assert_eq!(output, [0xaa; 8]);
    assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings + 1);
    // SAFETY: the wait error kept the device-owned submission alive.
    unsafe { finish(&queue.lock(), 8, false) };
    irq.store(1, Ordering::Relaxed);
    device.handle(0);
    assert_eq!(output, [0xaa; 8]);
    assert!(!device.is_failed());
    assert_eq!(device.read(&mut output), Ok(8));
    assert_eq!(output, [0, 1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings);
}

#[def_test(serial)]
fn hwrng_wait_retry_joins_pending_request() {
    let completion = Arc::new(Completion {
        after: Some(DriverError::NoMemory),
        ..Default::default()
    });
    let (device, queue, irq) = device(completion.clone());
    let mappings = MAPPINGS.load(Ordering::Relaxed);
    let mut output = [0xaa; 8];
    assert_eq!(device.read(&mut output), Err(KError::NoMemory));
    assert!(!device.is_failed());
    let token = match device.request.lock().state {
        RequestState::Pending(token) => token,
        _ => panic!("wait failure must retain the submission"),
    };
    let weak = Arc::downgrade(&device);
    *completion.action.lock() = Some(Box::new(move || {
        let device = weak.upgrade().unwrap();
        core::assert!(
            matches!(device.request.lock().state, RequestState::Pending(current) if current == token)
        );
        core::assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings + 1);
        // SAFETY: retry kept the original submitted buffer and token live.
        unsafe { finish(&queue.lock(), 8, false) };
        irq.store(1, Ordering::Relaxed);
        device.handle(0);
    }));
    assert_eq!(device.read(&mut output), Ok(8));
    assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings);
}

#[def_test(serial)]
fn hwrng_wrong_token_reports_terminal_failure() {
    let completion = Arc::new(Completion::default());
    let (device, queue, irq) = device(completion.clone());
    let weak = Arc::downgrade(&device);
    *completion.action.lock() = Some(Box::new(move || {
        let device = weak.upgrade().unwrap();
        // SAFETY: the malformed token does not change the live DMA bounds.
        unsafe { finish(&queue.lock(), 8, true) };
        irq.store(1, Ordering::Relaxed);
        device.handle(0);
    }));
    let mut output = [0xaa; 8];
    assert_eq!(device.read(&mut output), Err(KError::BadState));
    assert!(device.is_failed());
    assert_eq!(device.read(&mut output), Err(KError::BadState));
    assert_eq!(output, [0xaa; 8]);
}

#[def_test(serial)]
fn hwrng_bad_completion_discards_bytes_and_retries() {
    for len in [0, MAX_REQUEST_BYTES as u32 + 1] {
        let completion = Arc::new(Completion::default());
        let (device, queue, irq) = device(completion.clone());
        let mappings = MAPPINGS.load(Ordering::Relaxed);
        device.request.lock().submit().unwrap();
        // SAFETY: the request owns the live buffer until IRQ reclamation.
        unsafe { finish(&queue.lock(), len, false) };
        irq.store(1, Ordering::Relaxed);
        device.handle(0);
        let mut output = [0xaa; 8];
        assert_eq!(device.request.lock().read(&mut output), 0);
        assert_eq!(output, [0xaa; 8]);
        assert_eq!(completion.notifications.load(Ordering::Relaxed), 1);
        assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings);
        assert!(matches!(device.request.lock().state, RequestState::Idle));
        device.request.lock().submit().unwrap();
        // SAFETY: the replacement request still owns the same live buffer.
        unsafe { finish(&queue.lock(), 8, false) };
        irq.store(1, Ordering::Relaxed);
        device.handle(0);
        assert_eq!(device.request.lock().read(&mut output), 8);
        assert_eq!(completion.notifications.load(Ordering::Relaxed), 2);
        assert_eq!(device.read(&mut []), Ok(0));
    }
}

#[def_test(serial)]
fn completion_owns_buffer_and_preserves_unread_bytes() {
    for features in [0, 1 << 28, 1 << 29, (1 << 28) | (1 << 29)] {
        let mappings = MAPPINGS.load(Ordering::Relaxed);
        let dma = DMA_BLOCKS.load(Ordering::Relaxed);
        let (mut request, queue) = request(features);
        let mut output = [0xff; 8];
        assert_eq!(request.read(&mut output), 0);
        assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings);
        for _ in 0..12 {
            request.submit().unwrap();
            let address = BUFFER_ADDRESS.load(Ordering::Relaxed);
            request.submit().unwrap();
            assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings + 1);
            assert_eq!(request.read(&mut output), 0);
            // SAFETY: the request owns one live submission; test drives it serially.
            unsafe { finish(&queue.lock(), 13, false) };
            // Device completion alone cannot make a read reclaim the queue.
            assert_eq!(request.read(&mut output), 0);
            assert_eq!(request.complete(), Ok(true));
            assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings);
            assert_eq!(request.complete(), Ok(false));
            assert_eq!(request.read(&mut output), 8);
            assert_eq!(output, [0, 1, 2, 3, 4, 5, 6, 7]);
            request.submit().unwrap();
            assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings);
            assert_eq!(request.read(&mut output), 5);
            assert_eq!(&output[..5], &[8, 9, 10, 11, 12]);
            assert_eq!(BUFFER_ADDRESS.load(Ordering::Relaxed), address);
            assert_eq!(request.read(&mut output), 0);
        }
        drop(request);
        assert_eq!(queue.lock().descriptors, 0);
        assert_eq!(DMA_BLOCKS.load(Ordering::Relaxed), dma);
    }
}

#[def_test(serial)]
fn invalid_length_does_not_copy_and_allows_resubmission() {
    for len in [0, MAX_REQUEST_BYTES as u32 + 1] {
        let mappings = MAPPINGS.load(Ordering::Relaxed);
        let (mut request, queue) = request(0);
        request.submit().unwrap();
        // SAFETY: live single submission; finish only writes within capacity.
        unsafe { finish(&queue.lock(), len, false) };
        assert_eq!(request.complete(), Ok(true));
        let mut output = [0xaa; 8];
        assert_eq!(request.read(&mut output), 0);
        assert_eq!(output, [0xaa; 8]);
        assert_eq!(request.submit(), Ok(()));
        // SAFETY: the new request owns the buffer until it is reclaimed.
        unsafe { finish(&queue.lock(), 8, false) };
        assert_eq!(request.complete(), Ok(true));
        assert_eq!(request.read(&mut output), 8);
        assert_eq!(request.complete(), Ok(false));
        assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings);
    }
}

#[def_test(serial)]
fn reader_departure_and_owner_move_do_not_release_submission() {
    let mappings = MAPPINGS.load(Ordering::Relaxed);
    let (mut request, queue) = request(0);
    {
        let mut departed_reader = alloc::vec![0xaa; 8];
        request.submit().unwrap();
        assert_ne!(
            BUFFER_ADDRESS.load(Ordering::Relaxed),
            departed_reader.as_ptr() as usize
        );
        assert_eq!(request.read(&mut departed_reader), 0);
        assert_eq!(&*departed_reader, &[0xaa; 8]);
    }
    let mut moved = request;
    assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings + 1);
    // SAFETY: moving the owner and dropping the reader leave the DMA Box live.
    unsafe { finish(&queue.lock(), MAX_REQUEST_BYTES as u32, false) };
    assert_eq!(moved.complete(), Ok(true));
    let mut output = [0xff; MAX_REQUEST_BYTES + 1];
    assert_eq!(moved.read(&mut output), MAX_REQUEST_BYTES);
    assert!(
        output[..MAX_REQUEST_BYTES]
            .iter()
            .enumerate()
            .all(|(i, byte)| *byte == i as u8)
    );
    assert_eq!(output[MAX_REQUEST_BYTES], 0xff);
    assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings);
}

#[def_test(serial)]
fn lost_owner_retains_pending_dma_even_after_wrong_token() {
    for is_wrong_token in [false, true] {
        let mappings = MAPPINGS.load(Ordering::Relaxed);
        let (mut request, queue) = request(0);
        request.submit().unwrap();
        if is_wrong_token {
            // SAFETY: malformed used id does not change the live buffer's bounds.
            unsafe { finish(&queue.lock(), 8, true) };
            assert!(request.complete().is_err());
            assert!(request.submit().is_err());
            assert!(matches!(request.state, RequestState::Pending(_)));
        }
        let dma = DMA_BLOCKS.load(Ordering::Relaxed);
        drop(request);
        assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings + 1);
        assert_eq!(DMA_BLOCKS.load(Ordering::Relaxed), dma);
        assert_ne!(queue.lock().descriptors, 0);
    }
}

#[def_test(serial)]
fn closing_reads_wakes_pending_reader_without_reclaiming_dma() {
    let completion = Arc::new(Completion::default());
    let (device, queue, irq) = device(completion.clone());
    let mappings = MAPPINGS.load(Ordering::Relaxed);
    let dma = DMA_BLOCKS.load(Ordering::Relaxed);
    let notified_after_close = Arc::new(AtomicBool::new(false));
    let observed = notified_after_close.clone();
    let weak_for_notify = Arc::downgrade(&device);
    *completion.on_notify.lock() = Some(Box::new(move || {
        let device = weak_for_notify.upgrade().unwrap();
        observed.store(
            device.request.lock().is_closed && MAPPINGS.load(Ordering::Relaxed) == mappings + 1,
            Ordering::Relaxed,
        );
    }));
    let weak = Arc::downgrade(&device);
    *completion.action.lock() = Some(Box::new(move || {
        weak.upgrade().unwrap().close_reads();
    }));
    let mut output = [0xaa; 8];
    let read_result = device.read(&mut output);
    let notifications = completion.notifications.load(Ordering::Relaxed);
    let retained_mapping = MAPPINGS.load(Ordering::Relaxed);
    let retained_dma = DMA_BLOCKS.load(Ordering::Relaxed);
    let is_pending = matches!(device.request.lock().state, RequestState::Pending(_));
    device.close_reads();
    let repeated_notifications = completion.notifications.load(Ordering::Relaxed);
    irq.store(1, Ordering::Relaxed);
    let handled = device.handle(0).handled();
    let pending_irq = irq.load(Ordering::Relaxed);
    let mapping_after_irq = MAPPINGS.load(Ordering::Relaxed);
    let is_failed = device.is_failed();
    let submit_result = device.request.lock().submit();
    // SAFETY: The test owns the retained queue and original buffer; no IRQ runs concurrently.
    unsafe { finish(&queue.lock(), 8, false) };
    let completed = device.request.lock().complete();
    let closed_read = device.read(&mut output);
    drop(device);
    assert_eq!(read_result, Err(KError::BadState));
    assert_eq!(output, [0xaa; 8]);
    assert!(notified_after_close.load(Ordering::Relaxed));
    assert_eq!(notifications, 1);
    assert_eq!(retained_mapping, mappings + 1);
    assert_eq!(retained_dma, dma);
    assert!(is_pending);
    assert_eq!(repeated_notifications, 1);
    assert!(!handled);
    assert_eq!(pending_irq, 1);
    assert_eq!(mapping_after_irq, mappings + 1);
    assert!(!is_failed);
    assert_eq!(submit_result, Err(DriverError::BadState));
    assert_eq!(completed, Ok(true));
    assert_eq!(closed_read, Err(KError::BadState));
    assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings);
}

#[def_test(serial)]
fn closing_reads_rejects_cached_bytes_and_retained_references() {
    let completion = Arc::new(Completion::default());
    let (device, queue, irq) = device(completion.clone());
    let retained = device.clone();
    let mappings = MAPPINGS.load(Ordering::Relaxed);
    device.request.lock().submit().unwrap();
    // SAFETY: A single live submission owns this test queue and its buffer.
    unsafe { finish(&queue.lock(), 8, false) };
    irq.store(1, Ordering::Relaxed);
    assert!(device.handle(0).handled());
    assert!(matches!(
        device.request.lock().state,
        RequestState::Readable(_)
    ));
    device.close_reads();
    drop(device);
    let mut output = [0xaa; 8];
    assert_eq!(retained.read(&mut output), Err(KError::BadState));
    assert_eq!(retained.request.lock().read(&mut output), 0);
    assert_eq!(output, [0xaa; 8]);
    assert_eq!(retained.read(&mut []), Ok(0));
    assert!(!retained.is_failed());
    assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings);
}

#[def_test(serial)]
fn closing_idle_rng_never_submits_or_acknowledges() {
    let completion = Arc::new(Completion::default());
    let (device, _queue, irq) = device(completion);
    let mappings = MAPPINGS.load(Ordering::Relaxed);
    device.close_reads();
    assert_eq!(device.read(&mut [0; 8]), Err(KError::BadState));
    assert!(matches!(device.request.lock().state, RequestState::Idle));
    irq.store(3, Ordering::Relaxed);
    assert!(!device.handle(0).handled());
    assert_eq!(irq.load(Ordering::Relaxed), 3);
    assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings);
}

struct StopIrqProvider {
    fail_request: AtomicBool,
    handler: SpinNoIrq<Option<Arc<dyn IrqHandler>>>,
    on_release: SpinNoIrq<Option<Action>>,
    releases: AtomicUsize,
    line_changes: AtomicUsize,
}

static STOP_IRQ: StopIrqProvider = StopIrqProvider {
    fail_request: AtomicBool::new(false),
    handler: SpinNoIrq::new(None),
    on_release: SpinNoIrq::new(None),
    releases: AtomicUsize::new(0),
    line_changes: AtomicUsize::new(0),
};

impl IrqOp for StopIrqProvider {
    fn request_irq(
        &self,
        _irq: IrqResource,
        handler: Arc<dyn IrqHandler>,
    ) -> device_res::ResResult<device_res::IrqHandlerToken> {
        if self.fail_request.load(Ordering::Relaxed) {
            return Err(ResError::Busy);
        }
        *self.handler.lock() = Some(handler);
        Ok(device_res::IrqHandlerToken::shared_action(42))
    }

    fn release_irq(&self, _irq: IrqResource, token: device_res::IrqHandlerToken) {
        core::assert_eq!(token, device_res::IrqHandlerToken::shared_action(42));
        let handler = self.handler.lock().take();
        let action = self.on_release.lock().take();
        if let Some(action) = action {
            action();
        }
        drop(handler);
        self.releases.fetch_add(1, Ordering::Relaxed);
    }

    fn set_irq_enabled(&self, _irq: IrqResource, _enabled: bool) {
        self.line_changes.fetch_add(1, Ordering::Relaxed);
    }
}

struct QuiescentDrain;

impl subsys_hwrng::completion::RngDrain for QuiescentDrain {
    fn wait_until(&self, drained: &mut dyn FnMut() -> bool) -> KResult {
        core::assert!(drained(), "test readers must return before unregister");
        Ok(())
    }

    fn notify(&self) {}
}

#[def_test(serial)]
fn rng_removal_resets_then_releases_irq_before_canceling_pending_dma() {
    static REGISTRY: subsys_hwrng::HwrngRegistry = subsys_hwrng::HwrngRegistry::new();
    for has_used_entry in [false, true] {
        let mappings = MAPPINGS.load(Ordering::Relaxed);
        let dma = DMA_BLOCKS.load(Ordering::Relaxed);
        let releases = STOP_IRQ.releases.load(Ordering::Relaxed);
        let line_changes = STOP_IRQ.line_changes.load(Ordering::Relaxed);
        let queue = Arc::new(SpinNoIrq::new(MockQueue::default()));
        let reset = Arc::new(MockReset::default());
        reset.is_pending.store(true, Ordering::Relaxed);
        let mut transport = MockTransport::new_with_type(DeviceType::EntropySource);
        transport.queue = Some(queue.clone());
        transport.reset = Some(reset.clone());
        let completion = Arc::new(Completion {
            after: Some(DriverError::NoMemory),
            ..Default::default()
        });
        let device = Device::try_new_irq(
            transport,
            completion,
            &STOP_IRQ,
            IrqResource::new(7, device_res::IrqTrigger::Unknown(0)),
            || {},
        )
        .unwrap();
        let mut registration = REGISTRY
            .attach_legacy(
                "test-rng-stop",
                alloc::string::String::new(),
                device.clone(),
                Arc::new(QuiescentDrain),
            )
            .unwrap();
        let consumer = registration.publication();
        let mut output = [0xaa; 8];
        let initial_read = consumer.read(&mut output);
        if has_used_entry {
            // SAFETY: The failed wait retained the original live submission and no IRQ runs here.
            unsafe { finish(&queue.lock(), 8, false) };
        }
        device.close_reads();
        let unregister_result = registration.unregister();
        let mappings_after_drain = MAPPINGS.load(Ordering::Relaxed);
        let checked_release = Arc::new(AtomicBool::new(false));
        let observation = checked_release.clone();
        let weak = Arc::downgrade(&device);
        let reset_at_release = reset.clone();
        *STOP_IRQ.on_release.lock() = Some(Box::new(move || {
            let device = weak.upgrade().unwrap();
            observation.store(
                reset_at_release.confirmations.load(Ordering::Relaxed) > 0
                    && device.request.lock().resources.is_none()
                    && device.irq_guard.lock().is_none()
                    && !device.handle(7).handled()
                    && MAPPINGS.load(Ordering::Relaxed) == mappings + 1
                    && DMA_BLOCKS.load(Ordering::Relaxed) > dma,
                Ordering::Relaxed,
            );
        }));
        let mut waits = 0;
        let mut retained_while_waiting = true;
        let result = device.finish_stop(|| {
            waits += 1;
            retained_while_waiting &= device.request.lock().resources.is_none()
                && device.irq_guard.lock().is_some()
                && MAPPINGS.load(Ordering::Relaxed) == mappings + 1
                && STOP_IRQ.releases.load(Ordering::Relaxed) == releases;
            if waits == 2 {
                reset.is_pending.store(false, Ordering::Relaxed);
            }
        });
        let mut repeated_waits = 0;
        let repeated = device.finish_stop(|| repeated_waits += 1);
        let late_read = device.read(&mut output);
        let consumer_read = consumer.read(&mut output);
        let resources_gone = device.request.lock().resources.is_none();
        let closed_without_fault = !device.is_failed();
        let mappings_after_stop = MAPPINGS.load(Ordering::Relaxed);
        let dma_after_stop = DMA_BLOCKS.load(Ordering::Relaxed);
        drop(device);
        drop(registration);
        drop(consumer);
        assert_eq!(initial_read, Err(KError::NoMemory));
        assert_eq!(unregister_result, Ok(()));
        assert_eq!(mappings_after_drain, mappings + 1);
        assert_eq!(result, Ok(()));
        assert_eq!(waits, 2);
        assert!(retained_while_waiting);
        assert!(checked_release.load(Ordering::Relaxed));
        assert!(resources_gone);
        assert!(closed_without_fault);
        assert_eq!(mappings_after_stop, mappings);
        assert_eq!(dma_after_stop, dma);
        assert_eq!(repeated, Ok(()));
        assert_eq!(repeated_waits, 0);
        assert_eq!(late_read, Err(KError::BadState));
        assert_eq!(consumer_read, Err(KError::BadState));
        assert_eq!(output, [0xaa; 8]);
        assert_eq!(reset.requests.load(Ordering::Relaxed), 1);
        assert_eq!(STOP_IRQ.releases.load(Ordering::Relaxed), releases + 1);
        assert_eq!(STOP_IRQ.line_changes.load(Ordering::Relaxed), line_changes);
        assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings);
        assert_eq!(DMA_BLOCKS.load(Ordering::Relaxed), dma);
        assert!(REGISTRY.devices().is_empty());
    }
}

#[def_test(serial)]
fn rng_finish_stop_rejects_open_device_and_releases_idle_or_completed_resources() {
    for has_completed in [false, true] {
        let mappings = MAPPINGS.load(Ordering::Relaxed);
        let dma = DMA_BLOCKS.load(Ordering::Relaxed);
        let (device, queue, irq) = device(Arc::new(Completion::default()));
        let premature = device.finish_stop(|| {});
        let retained_open = device.request.lock().resources.is_some();
        if has_completed {
            device.request.lock().submit().unwrap();
            // SAFETY: The single original submission remains alive until the IRQ completion.
            unsafe { finish(&queue.lock(), 8, false) };
            irq.store(1, Ordering::Relaxed);
            device.handle(0);
        }
        device.close_reads();
        let mut waits = 0;
        let result = device.finish_stop(|| waits += 1);
        let resources_gone = device.request.lock().resources.is_none();
        drop(device);
        assert_eq!(premature, Err(DriverError::BadState));
        assert!(retained_open);
        assert_eq!(result, Ok(()));
        assert_eq!(waits, 0);
        assert!(resources_gone);
        assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings);
        assert_eq!(DMA_BLOCKS.load(Ordering::Relaxed), dma);
    }
}

#[def_test(serial)]
fn rng_irq_registration_failure_confirms_reset_before_freeing_queue() {
    let mappings = MAPPINGS.load(Ordering::Relaxed);
    let dma = DMA_BLOCKS.load(Ordering::Relaxed);
    let releases = STOP_IRQ.releases.load(Ordering::Relaxed);
    let queue = Arc::new(SpinNoIrq::new(MockQueue::default()));
    let reset = Arc::new(MockReset::default());
    reset.is_pending.store(true, Ordering::Relaxed);
    let mut transport = MockTransport::new_with_type(DeviceType::EntropySource);
    transport.queue = Some(queue.clone());
    transport.reset = Some(reset.clone());
    let completion = Arc::new(Completion::default());
    STOP_IRQ.fail_request.store(true, Ordering::Relaxed);
    let mut waits = 0;
    let mut retained_until_reset = true;
    let result = Device::try_new_irq(
        transport,
        completion.clone(),
        &STOP_IRQ,
        IrqResource::new(7, device_res::IrqTrigger::Unknown(0)),
        || {
            waits += 1;
            retained_until_reset &= DMA_BLOCKS.load(Ordering::Relaxed) == dma + 2
                && MAPPINGS.load(Ordering::Relaxed) == mappings
                && queue.lock().descriptors != 0
                && STOP_IRQ.handler.lock().is_none()
                && STOP_IRQ.releases.load(Ordering::Relaxed) == releases;
            if waits == 2 {
                reset.is_pending.store(false, Ordering::Relaxed);
            }
        },
    );
    STOP_IRQ.fail_request.store(false, Ordering::Relaxed);
    let error = result.err();
    assert_eq!(error, Some(DriverError::ResourceBusy));
    assert_eq!(waits, 2);
    assert!(retained_until_reset);
    assert_eq!(reset.requests.load(Ordering::Relaxed), 1);
    assert_eq!(reset.confirmations.load(Ordering::Relaxed), 1);
    assert_eq!(completion.notifications.load(Ordering::Relaxed), 1);
    assert_eq!(STOP_IRQ.releases.load(Ordering::Relaxed), releases);
    assert!(STOP_IRQ.handler.lock().is_none());
    assert_eq!(DMA_BLOCKS.load(Ordering::Relaxed), dma);
    assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings);
}
