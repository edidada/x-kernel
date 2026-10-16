// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Interrupt-driven VirtIO RNG adapter.

use alloc::{boxed::Box, sync::Arc, vec};
use core::{mem::ManuallyDrop, ops::Range};

use kerrno::{KError, KResult};
use kspin::SpinNoIrq;
use subsys_hwrng::{HwrngDevice, completion::RngCompletion};
use virtio_drivers::{
    Hal,
    device::rng::VirtIORng,
    transport::{InterruptStatus, Transport},
};

use crate::as_driver_error;

const MAX_REQUEST_BYTES: usize = 256;

enum RequestState {
    Idle,
    Pending(u16),
    Readable(Range<usize>),
}

/// Queue state and DMA storage that a submitted request must retain together.
struct RequestResources<H: Hal, T: Transport> {
    device: VirtIORng<H, T>,
    buffer: Box<[u8]>,
}

struct RngRequest<H: Hal, T: Transport> {
    resources: ManuallyDrop<RequestResources<H, T>>,
    state: RequestState,
    fault: Option<KError>,
}

// SAFETY: H supplies kernel-global mappings through dma_alloc and
// mmio_phys_to_virt: queue/DMA pointers remain valid across threads and CPUs,
// rather than being private to the creating thread or CPU. The queue, its state,
// and the owned Box<[u8]> move together without relocating their backing storage.
// H: Send and T: Send also require the HAL and transport themselves to be
// transferable; moving the request therefore preserves its pointer validity.
unsafe impl<H: Hal + Send, T: Transport + Send> Send for RngRequest<H, T> {}

impl<H: Hal, T: Transport> RngRequest<H, T> {
    fn new(transport: T) -> KResult<Self> {
        let buffer = vec![0; MAX_REQUEST_BYTES].into_boxed_slice();
        let device = VirtIORng::new(transport).map_err(as_driver_error)?;
        Ok(Self {
            resources: ManuallyDrop::new(RequestResources { device, buffer }),
            state: RequestState::Idle,
            fault: None,
        })
    }

    /// Only the waiting-read path submits; cached or in-flight data is preserved.
    fn submit(&mut self) -> KResult {
        if let Some(error) = self.fault {
            return Err(error);
        }
        // Cached bytes share the DMA buffer; do not resubmit until they are fully consumed.
        if !matches!(self.state, RequestState::Idle) {
            return Ok(());
        }
        let RequestResources { device, buffer } = &mut *self.resources;
        // SAFETY: the private Box has a stable address/length. Pending prevents
        // CPU reads and resubmission; Drop retains both resources if still live.
        match unsafe { device.request_entropy_nb(buffer) } {
            Ok(token) => {
                self.state = RequestState::Pending(token);
                Ok(())
            }
            Err(error) => {
                // A fixed nonempty buffer and one outstanding request cannot
                // exhaust this queue. These errors indicate broken invariants.
                if matches!(
                    error,
                    virtio_drivers::Error::QueueFull | virtio_drivers::Error::InvalidParam
                ) {
                    self.fault = Some(as_driver_error(error));
                }
                Err(as_driver_error(error))
            }
        }
    }

    /// Attempts one reclamation. The IRQ adapter, never a reader, calls this.
    /// Returns true only when an outstanding request was actually reclaimed.
    fn complete(&mut self) -> KResult<bool> {
        let RequestState::Pending(token) = self.state else {
            return Ok(false);
        };
        let RequestResources { device, buffer } = &mut *self.resources;
        // SAFETY: Pending holds the original live token; buffer is the original
        // Box at its full submitted length. Exclusive self serializes reclaim.
        let len = match unsafe { device.complete_request_entropy(token, buffer) } {
            Ok(len) => len,
            // A notification is only a hint; ownership stays with the device
            // until a matching used entry arrives in a later completion IRQ.
            Err(virtio_drivers::Error::NotReady) => return Ok(false),
            Err(error) => {
                let error = as_driver_error(error);
                self.fault.get_or_insert(error);
                return Err(error);
            }
        };
        self.state = RequestState::Idle;
        if len == 0 || len > MAX_REQUEST_BYTES {
            return Ok(true);
        }
        if self.fault.is_none() {
            self.state = RequestState::Readable(0..len);
        }
        Ok(true)
    }

    /// Consumes cached bytes only, including retaining a short read's remainder.
    fn read(&mut self, dst: &mut [u8]) -> usize {
        if let Some(_error) = self.fault {
            return 0;
        }
        let RequestState::Readable(remaining) = &mut self.state else {
            return 0;
        };

        let count = dst.len().min(remaining.len());
        dst[..count]
            .copy_from_slice(&self.resources.buffer[remaining.start..remaining.start + count]);
        remaining.start += count;
        if Range::is_empty(remaining) {
            self.state = RequestState::Idle;
        }
        count
    }
}

impl<H: Hal, T: Transport> Drop for RngRequest<H, T> {
    fn drop(&mut self) {
        if matches!(self.state, RequestState::Pending(_)) {
            // Emergency fallback for a lost owner, which
            // must not free DMA memory, even when completion was malformed.
            log::error!("virtio-rng: lost request owner; retaining live DMA resources");
            return;
        }
        // SAFETY: no outstanding submission can access these request resources.
        // They are dropped exactly once here; active transport lifetime is a host duty.
        unsafe { ManuallyDrop::drop(&mut self.resources) };
    }
}

/// IRQ-driven hardware entropy source using a driver-owned DMA buffer.
///
/// While the published instance exists, it owns its IRQ registration and mappings.
/// Device removal and rebinding are not supported.
/// Completion reclaims at most one request inline in hard IRQ context, including
/// HAL unshare, which must be IRQ-safe and non-sleeping. This limits the amount
/// of request work, not lock-wait latency: the X-Kernel HAL unmaps through the
/// global kdma allocator lock, shared with other DMA operations. No worst-case
/// IRQ latency bound has been established for that backend.
/// HAL DMA and MMIO mappings must be kernel-global, independent of the creating
/// thread or CPU, so request ownership can move across threads.
/// The `request` SpinNoIrq lock serializes readers and IRQ completion, providing
/// exclusive access to the queue, buffer, and request state through shared device
/// references; RngRequest itself does not implement Sync.
pub struct VirtIoRngDev<H: Hal, T: Transport> {
    /// MMIO register lease: the mapping stays mapped as long as ANY
    /// reference to this device object (class publication, consumer clones)
    /// lives — the last drop releases the mapping (Linux: pci_release_regions
    /// runs after the last put_device).
    _mmio_regs: Option<driver_base::IoMap>,
    request: SpinNoIrq<RngRequest<H, T>>,
    completion: Arc<dyn RngCompletion>,
    // Drop after `request`, so a normal request cleanup runs before this guard
    // unregisters its completion handler.
}

impl<H: Hal + Send + 'static, T: Transport + Send + 'static> VirtIoRngDev<H, T> {
    /// Construct without registering the completion IRQ — the driver layer
    /// owns registration through its device services (`DeviceCtx::
    /// request_irq`, the single entry point), then calls
    /// [`Self::enable_completion_irq`]. The registration closure calls
    /// [`Self::ack_irq`].
    pub fn try_new(
        transport: T,
        completion: Arc<dyn RngCompletion>,
        mmio_regs: Option<driver_base::IoMap>,
    ) -> KResult<Arc<Self>> {
        Ok(Arc::new(Self::new(transport, completion, mmio_regs)?))
    }

    /// Start completion delivery. Call once the IRQ handler is registered.
    pub fn enable_completion_irq(&self) {
        self.request.lock().resources.device.enable_interrupts();
    }
}

impl<H: Hal, T: Transport> VirtIoRngDev<H, T> {
    // Tests drive the IRQ path explicitly; production registration goes through DeviceCtx::request_irq.
    fn new(
        transport: T,
        completion: Arc<dyn RngCompletion>,
        mmio_regs: Option<driver_base::IoMap>,
    ) -> KResult<Self> {
        Ok(Self {
            _mmio_regs: mmio_regs,
            request: SpinNoIrq::new(RngRequest::new(transport)?),
            completion,
        })
    }
}

impl<H: Hal + Send, T: Transport + Send> HwrngDevice for VirtIoRngDev<H, T> {
    fn is_failed(&self) -> bool {
        self.request.lock().fault.is_some()
    }

    fn read(&self, buf: &mut [u8]) -> KResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut result = Ok(0);
        self.completion.wait_until(&mut || {
            let mut request = self.request.lock();
            let count = request.read(buf);
            if count > 0 {
                result = Ok(count);
                return true;
            }

            if let Err(error) = request.submit() {
                result = Err(error);
                drop(request);
                self.completion.notify();
                return true;
            }
            false
        })?;
        result
    }
}

impl<H: Hal + Send, T: Transport + Send> VirtIoRngDev<H, T> {
    /// Acknowledge one interrupt and report whether it was ours.
    ///
    /// The family-side half of `DeviceCtx::request_irq` — see
    /// `VirtIoBlkDev::ack_irq` for why this is an inherent method and not a
    /// host registration trait. `irq` is unused: the RNG function owns its
    /// line, so there is no shared-line filter to apply.
    ///
    /// Runs in hard-IRQ context: acks the transport, completes the pending
    /// request, and notifies the completion waiter. No allocation.
    pub fn ack_irq(&self, _irq: usize) -> bool {
        let mut request = self.request.lock();
        let status = request.resources.device.ack_interrupt();
        if status.is_empty() {
            return false;
        }
        if !status.contains(InterruptStatus::QUEUE_INTERRUPT) {
            return true;
        }
        let completed = request.complete();
        let has_fault = request.fault.is_some();
        drop(request);
        if has_fault || completed == Ok(true) {
            self.completion.notify();
        }
        true
    }
}

#[unittest::mod_test]
mod tests {
    use alloc::sync::Arc;
    use core::{
        ptr::NonNull,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use kspin::SpinNoIrq;
    use unittest::{assert, assert_eq, assert_ne, def_test};
    use virtio_drivers::{BufferDirection, PhysAddr, transport::DeviceType};

    use super::*;
    use crate::mock_virtio::{MockHal, MockQueue, MockTransport};

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

        unsafe fn share(
            buffer: NonNull<[u8]>,
            direction: BufferDirection,
            platform: bool,
        ) -> PhysAddr {
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
        before: Option<KError>,
        after: Option<KError>,
        has_returned_error: AtomicBool,
        action: SpinNoIrq<Option<Action>>,
        notifications: AtomicUsize,
    }
    impl RngCompletion for Completion {
        fn wait_until(&self, ready: &mut dyn FnMut() -> bool) -> KResult {
            if let Some(error) = self.before {
                return Err(error);
            }
            if ready() {
                return Ok(());
            }
            if let Some(error) = self.after
                && !self.has_returned_error.swap(true, Ordering::Relaxed)
            {
                return Err(error);
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
        }
    }
    fn device(
        completion: Arc<Completion>,
    ) -> (Arc<Device>, Arc<SpinNoIrq<MockQueue>>, Arc<AtomicUsize>) {
        let queue = Arc::new(SpinNoIrq::new(MockQueue::default()));
        let mut transport = MockTransport::new_with_type(DeviceType::EntropySource);
        transport.queue = Some(queue.clone());
        let irq = transport.interrupt_status.clone();
        let device = Device::new(transport, completion, None).unwrap();
        (Arc::new(device), queue, irq)
    }

    #[def_test(serial)]
    fn hwrng_irq_reclaims_before_waking_waiter() {
        let completion = Arc::new(Completion::default());
        let (device, queue, irq) = device(completion.clone());
        let mappings = MAPPINGS.load(Ordering::Relaxed);
        assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings);
        assert!(!device.ack_irq(0));
        let weak = Arc::downgrade(&device);
        *completion.action.lock() = Some(Box::new(move || {
            let device = weak.upgrade().unwrap();
            irq.store(2, Ordering::Relaxed);
            core::assert!(device.ack_irq(0)); // Configuration only.
            // SAFETY: the waiting-read predicate published one live request.
            unsafe { finish(&queue.lock(), 13, false) };
            irq.store(1, Ordering::Relaxed);
            core::assert!(device.ack_irq(0));
            core::assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings);
            irq.store(1, Ordering::Relaxed);
            core::assert!(device.ack_irq(0)); // No duplicate reclaim.
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
                core::assert!(device.ack_irq(0));
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
            device.ack_irq(0);
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
        for error in [KError::InvalidInput, KError::NoMemory] {
            let completion = Arc::new(Completion {
                before: Some(error),
                ..Default::default()
            });
            let (device, queue, irq) = device(completion);
            let mappings = MAPPINGS.load(Ordering::Relaxed);
            assert_eq!(device.read(&mut [0; 8]), Err(error));
            assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings);
            device.request.lock().submit().unwrap();
            // SAFETY: the controlled test published one device-owned buffer.
            unsafe { finish(&queue.lock(), 8, false) };
            irq.store(1, Ordering::Relaxed);
            device.ack_irq(0);
            assert_eq!(device.read(&mut [0; 8]), Err(error));
            assert_eq!(device.request.lock().read(&mut [0; 8]), 8);
        }
        let completion = Arc::new(Completion {
            after: Some(KError::NoMemory),
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
        device.ack_irq(0);
        assert_eq!(output, [0xaa; 8]);
        assert!(!device.is_failed());
        assert_eq!(device.read(&mut output), Ok(8));
        assert_eq!(output, [0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(MAPPINGS.load(Ordering::Relaxed), mappings);
    }

    #[def_test(serial)]
    fn hwrng_wait_retry_joins_pending_request() {
        let completion = Arc::new(Completion {
            after: Some(KError::NoMemory),
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
            device.ack_irq(0);
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
            device.ack_irq(0);
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
            device.ack_irq(0);
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
            device.ack_irq(0);
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
}
