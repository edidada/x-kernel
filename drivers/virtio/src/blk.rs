// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO block driver adapter.
use alloc::{string::String, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use block::BlockDeviceOperations;
use driver_base::{Device, DeviceKind, DriverResult};
use kirq::softirq::SoftirqVec;
use kpoll::PollSet;
use kspin::SpinNoIrq;
use virtio_drivers::{
    Hal,
    device::blk::{BlkReq, BlkResp, SECTOR_SIZE, VirtIOBlk as InnerDev},
    transport::Transport,
};

use crate::{
    as_driver_error,
    irq_completion::{IrqCompletionDevice, IrqCompletionEntry, IrqCompletionHub, wait_poll_set},
};

/// Hub deferring virtio-blk completions through the Block softirq vector.
static BLK_COMPLETION_HUB: IrqCompletionHub = IrqCompletionHub::new(SoftirqVec::Block);

fn blk_completion_softirq() {
    BLK_COMPLETION_HUB.run_softirq();
}

/// Per-request completion slot, indexed by the request's used-ring token.
///
/// The bottom half marks the slot owning the used-ring head, so each in-flight
/// request is completed by its own token (Linux `virtqueue_get_buf` push
/// semantics) instead of sharing one device-wide wake.
struct BlkSlot {
    is_done: AtomicBool,
    wake: PollSet,
}

impl BlkSlot {
    fn new() -> Self {
        Self {
            is_done: AtomicBool::new(false),
            wake: PollSet::new(),
        }
    }

    fn is_done(&self) -> bool {
        self.is_done.load(Ordering::Acquire)
    }

    /// Marks completion once; returns whether this call was the first to do
    /// so (only the first marker wakes).
    fn mark_done(&self) -> bool {
        !self.is_done.swap(true, Ordering::AcqRel)
    }
}

/// Token-indexed registry of in-flight request slots for one device.
struct BlkCompletionSlots {
    slots: SpinNoIrq<Vec<Option<Arc<BlkSlot>>>>,
}

impl BlkCompletionSlots {
    fn new(queue_size: u16) -> Self {
        Self {
            slots: SpinNoIrq::new(vec![None; queue_size as usize]),
        }
    }

    fn register(&self, token: u16) -> Arc<BlkSlot> {
        let slot = Arc::new(BlkSlot::new());
        let mut slots = self.slots.lock();
        let entry = &mut slots[token as usize];
        debug_assert!(
            entry.is_none(),
            "virtio-blk completion slot {token} was still occupied"
        );
        *entry = Some(slot.clone());
        slot
    }

    fn remove(&self, token: u16) {
        self.slots.lock()[token as usize] = None;
    }

    /// Push-dispatches completions: marks the slot owning the used-ring head
    /// and wakes it, repeating while the peeked head advances.
    ///
    /// `peek` is evaluated under the caller's device lock, so it cannot
    /// observe a head that a concurrent completion is currently popping. With
    /// the real device the head stays put until its owner pops it, so one
    /// call marks at most one fresh completion and then stops; the woken
    /// owner pops its entry and calls this again to advance the chain. A
    /// scripted peek (unit tests) can model immediate pops and drain several
    /// tokens in one call.
    fn dispatch_with(&self, mut peek: impl FnMut() -> Option<u16>) {
        while let Some(head) = peek() {
            let slot = self.slots.lock()[head as usize].clone();
            let Some(slot) = slot else {
                // The head belongs to a request that has not registered its
                // slot yet (submit-to-register race); its own post-register
                // dispatch will catch the completion.
                break;
            };
            if !slot.mark_done() {
                // Already marked; its owner is on the way to pop it.
                break;
            }
            slot.wake.wake();
        }
    }

    fn dispatch_from_device<H: Hal, T: Transport>(&self, inner: &SpinNoIrq<InnerDev<H, T>>) {
        self.dispatch_with(|| inner.lock().peek_used());
    }
}

/// Exclusive-flush barrier over the set of in-flight requests.
///
/// Read/write requests may run concurrently (multi-request in flight); a flush
/// first claims the `is_flushing` flag so no *new* request starts, then waits
/// for the in-flight set to drain. This upholds the `virtio-drivers`
/// synchronous flush helper's requirement that the device processes no other
/// buffer, while bounding flush starvation to one request duration under a
/// continuous request stream.
struct RequestGate {
    state: SpinNoIrq<RequestGateState>,
    wake: PollSet,
}

#[derive(Clone, Copy)]
struct RequestGateState {
    in_flight: usize,
    is_flushing: bool,
}

impl RequestGate {
    fn new() -> Self {
        Self {
            state: SpinNoIrq::new(RequestGateState {
                in_flight: 0,
                is_flushing: false,
            }),
            wake: PollSet::new(),
        }
    }

    /// Joins the in-flight set, waiting while a flush holds the gate.
    fn acquire_request(&self) -> RequestGuard<'_> {
        if !wait_poll_set(&self.wake, || {
            let mut state = self.state.lock();
            if state.is_flushing {
                false
            } else {
                state.in_flight += 1;
                true
            }
        }) {
            // No sleepable context (early boot); a flush cannot be running
            // concurrently in such a context, so a bounded spin suffices.
            while {
                let mut state = self.state.lock();
                if state.is_flushing {
                    true
                } else {
                    state.in_flight += 1;
                    false
                }
            } {
                core::hint::spin_loop();
            }
        }
        RequestGuard(self)
    }

    fn release_request(&self) {
        self.state.lock().in_flight -= 1;
        self.wake.wake();
    }

    /// Claims the flush barrier: first the exclusive flag (holding off new
    /// requests), then the drain of the in-flight set.
    fn begin_flush(&self) -> FlushGuard<'_> {
        if !wait_poll_set(&self.wake, || {
            let mut state = self.state.lock();
            if state.is_flushing {
                false
            } else {
                state.is_flushing = true;
                true
            }
        }) {
            while {
                let mut state = self.state.lock();
                if state.is_flushing {
                    true
                } else {
                    state.is_flushing = true;
                    false
                }
            } {
                core::hint::spin_loop();
            }
        }
        if !wait_poll_set(&self.wake, || self.state.lock().in_flight == 0) {
            while self.state.lock().in_flight != 0 {
                core::hint::spin_loop();
            }
        }
        FlushGuard(self)
    }

    fn end_flush(&self) {
        self.state.lock().is_flushing = false;
        self.wake.wake();
    }

    #[cfg(unittest)]
    fn snapshot(&self) -> RequestGateState {
        *self.state.lock()
    }
}

/// RAII guard for one in-flight request.
struct RequestGuard<'a>(&'a RequestGate);

impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        self.0.release_request();
    }
}

/// RAII guard for a held flush barrier.
struct FlushGuard<'a>(&'a RequestGate);

impl Drop for FlushGuard<'_> {
    fn drop(&mut self) {
        self.0.end_flush();
    }
}

/// hardirq-facing view of one virtio-blk device.
struct VirtIoBlkIrqDevice<H: Hal, T: Transport> {
    inner: Arc<SpinNoIrq<InnerDev<H, T>>>,
    slots: Arc<BlkCompletionSlots>,
    entry: Arc<IrqCompletionEntry>,
}

// SAFETY: the inner device is only reached through its `SpinNoIrq` lock,
// which provides the mutual exclusion required for cross-thread transfer.
unsafe impl<H: Hal, T: Transport> Send for VirtIoBlkIrqDevice<H, T> {}
// SAFETY: shared references only reach the device through the same
// `SpinNoIrq` lock, so synchronization is preserved.
unsafe impl<H: Hal, T: Transport> Sync for VirtIoBlkIrqDevice<H, T> {}

impl<H: Hal, T: Transport> IrqCompletionDevice for VirtIoBlkIrqDevice<H, T> {
    fn ack_interrupt(&self) -> bool {
        !self.inner.lock().ack_interrupt().is_empty()
    }

    fn run_bottom_half(&self) {
        // Push completion: mark the slot owning the used-ring head so its
        // waiter is woken by its own token; the woken owner pops its entry
        // and re-dispatches to advance the chain.
        self.slots.dispatch_from_device(&self.inner);
    }

    fn entry(&self) -> &Arc<IrqCompletionEntry> {
        &self.entry
    }
}

/// Number of minor bits reserved for partitions of one virtio disk.
pub const PART_BITS: u32 = 4;
/// Block major used by the current X-Kernel virtio disk namespace.
pub const VIRTIO_BLK_MAJOR: u32 = 254;

/// Global virtio-blk index allocator.
///
/// Assigned in discovery order by [`VirtIoBlkDev::try_new`] and used to derive
/// the Linux-style `vdX` device name. `Relaxed` ordering suffices because each
/// device only needs a unique index, not a globally synchronized counter value.
static VD_INDEX: AtomicU32 = AtomicU32::new(0);

/// Converts a zero-based index into a base26 suffix for Linux-style
/// virtio-blk device names (0 -> "a", 25 -> "z", 26 -> "aa").
/// The caller will prepend the "vd" prefix separately.
fn vd_name(index: u32) -> String {
    let mut suffix = String::new();
    let mut n = index;
    loop {
        let digit = (n % 26) as u8;
        suffix.push((b'a' + digit) as char);
        n /= 26;
        if n == 0 {
            break;
        }
        n -= 1;
    }
    suffix.chars().rev().collect()
}

/// The VirtIO block device driver.
///
/// Wraps `VirtIOBlk` from `virtio-drivers` and implements the
/// [`BlockDeviceOperations`] trait, providing sector-level read/write access to a
/// virtual block device.
///
/// # Interrupt model
///
/// When an IRQ number is available and registration succeeds, the driver uses
/// the Linux-style top/bottom-half split with **push completions**:
///
/// - the hardirq handler acknowledges the device interrupt status, marks the
///   device pending, and raises the Block softirq vector;
/// - the Block softirq action dispatches completions per request: it marks
///   the completion slot owning the used-ring head and wakes that request's
///   waiter (each in-flight request owns a token-indexed slot — the
///   equivalent of Linux's `virtqueue_get_buf` loop driving per-request
///   endio completions);
/// - the woken requester pops its own used-ring entry (recycling its
///   descriptor chain and its caller-owned buffers), re-dispatches so the
///   next completed token is marked, and returns.
///
/// Multiple requests may be in flight concurrently. A flush claims an
/// exclusive barrier that first holds off new requests and then waits for the
/// in-flight set to drain, upholding the synchronous flush helper's
/// single-buffer contract. Without an IRQ number (or when IRQ registration
/// fails), requests keep the original synchronous busy-poll path under
/// [`SpinNoIrq`].
///
/// # Type Parameters
///
/// - `H` - VirtIO HAL implementation for DMA allocation.
/// - `T` - Transport layer (MMIO or PCI).
///
/// # Example
///
/// ```ignore
/// let (kind, transport, irq) = virtio::probe_pci_device::<HalImpl, _>(...).unwrap();
/// let mut blk = VirtIoBlkDev::<HalImpl, _>::try_new(transport, Some(irq))?;
/// let mut buf = [0u8; 512];
/// blk.read_block(0, &mut buf)?;
/// ```
pub struct VirtIoBlkDev<H: Hal, T: Transport> {
    device: Arc<SpinNoIrq<InnerDev<H, T>>>,
    sector_size: usize,
    num_blocks: u64,
    name: String,
    index: u32,
    /// Exclusive-flush barrier over the in-flight request set (IRQ mode).
    request_gate: RequestGate,
    /// Token-indexed completion slots (IRQ mode).
    completion_slots: Arc<BlkCompletionSlots>,
    /// Shared completion entry; `None` keeps the synchronous busy-poll path.
    irq_completion: Option<Arc<IrqCompletionEntry>>,
    /// Keeps the hardirq-facing device object registered until drop.
    irq_device: Option<Arc<dyn IrqCompletionDevice>>,
}

// SAFETY: VirtIoBlkDev serializes device access through its `SpinNoIrq` lock
// in polling mode, and through short lock-held critical sections (submit,
// pop) around a sleeping wait in IRQ mode. The inner VirtIOBlk is not auto
// Send due to PhantomData, but it is safe to transfer across threads behind
// that synchronization. The IRQ-driven wait runs with the device lock
// released and local interrupts enabled: the registered hardirq handler
// acknowledges the device ISR, so a shared level-triggered INTx line cannot
// livelock the CPU. The polling fallback still spins while holding the lock,
// which masks local interrupts for the duration of each request.
unsafe impl<H: Hal, T: Transport> Send for VirtIoBlkDev<H, T> {}
// SAFETY: shared access to the device is serialized as described above, so
// immutable references may be shared across threads safely.
unsafe impl<H: Hal, T: Transport> Sync for VirtIoBlkDev<H, T> {}

impl<H: Hal + 'static, T: Transport + 'static> VirtIoBlkDev<H, T> {
    /// Creates a new driver instance and initializes the device, or returns
    /// an error if any step fails.
    ///
    /// # Arguments
    ///
    /// - `transport` - The VirtIO transport (MMIO or PCI) for this device.
    /// - `irq` - Optional IRQ number. When provided and registration
    ///   succeeds, request completions are dispatched through the Block
    ///   softirq as described in the [interrupt model](#interrupt-model).
    ///   When omitted, the driver keeps the synchronous busy-poll behavior.
    ///
    /// # Errors
    ///
    /// Returns `DriverError` if the device fails to initialize (e.g. feature
    /// negotiation failure, queue allocation failure, DMA error).
    pub fn try_new(transport: T, irq: Option<usize>) -> DriverResult<Self> {
        let device = Self::init_device(transport)?;
        let num_blocks = device.capacity();
        let index = VD_INDEX.fetch_add(1, Ordering::Relaxed);

        let device = Arc::new(SpinNoIrq::new(device));
        let completion_slots = Arc::new(BlkCompletionSlots::new(device.lock().virt_queue_size()));
        let (irq_completion, irq_device) = match irq {
            Some(irq) => {
                let entry = Arc::new(IrqCompletionEntry::new(irq));
                let irq_device: Arc<dyn IrqCompletionDevice> = Arc::new(VirtIoBlkIrqDevice {
                    inner: device.clone(),
                    slots: completion_slots.clone(),
                    entry: entry.clone(),
                });
                match BLK_COMPLETION_HUB.register(blk_completion_softirq, irq_device.clone()) {
                    Some(_) => (Some(entry), Some(irq_device)),
                    None => (None, None),
                }
            }
            None => (None, None),
        };

        Ok(Self {
            device,
            sector_size: SECTOR_SIZE,
            num_blocks,
            name: alloc::format!("vd{}", vd_name(index)),
            index,
            request_gate: RequestGate::new(),
            completion_slots,
            irq_completion,
            irq_device,
        })
    }

    /// Returns the discovery-order disk index retained for driver cleanup and
    /// `gendisk` minor allocation.
    pub const fn index(&self) -> u32 {
        self.index
    }

    fn init_device(transport: T) -> DriverResult<InnerDev<H, T>> {
        InnerDev::new(transport).map_err(as_driver_error)
    }

    fn read_sector(&self, sector: u64, out_buf: &mut [u8]) -> DriverResult {
        self.device
            .lock()
            .read_blocks(sector as usize, out_buf)
            .map_err(as_driver_error)
    }

    fn write_sector(&self, sector: u64, in_buf: &[u8]) -> DriverResult {
        self.device
            .lock()
            .write_blocks(sector as usize, in_buf)
            .map_err(as_driver_error)
    }

    /// Sleeps until the bottom half marks this request's token done.
    ///
    /// When the current context cannot sleep (early boot or an IRQ-context
    /// caller), polls the slot with local interrupts enabled; the registered
    /// hardirq handler acknowledges the device ISR, so a shared
    /// level-triggered line cannot livelock the CPU.
    fn wait_for_slot(&self, slot: &Arc<BlkSlot>) {
        if wait_poll_set(&slot.wake, || slot.is_done()) {
            return;
        }
        while !slot.is_done() {
            core::hint::spin_loop();
        }
    }

    /// Pops this request's entry after its slot was marked done: completes
    /// the request, releases the slot, and re-dispatches so the next
    /// completed token is marked.
    ///
    /// `complete` runs under the device lock and receives that lock's guard;
    /// the slot is removed while the lock is still held so the token cannot
    /// be recycled by a concurrent submit before the registry is cleaned.
    fn finish_request(
        &self,
        token: u16,
        complete: impl FnOnce(&mut InnerDev<H, T>) -> DriverResult,
    ) -> DriverResult {
        let result = {
            let mut inner = self.device.lock();
            let result = complete(&mut inner);
            self.completion_slots.remove(token);
            result
        };
        // The pop advanced the used-ring head; dispatch the next completion.
        self.completion_slots.dispatch_from_device(&self.device);
        result
    }

    /// Submits a read request and sleeps until the Block softirq marks this
    /// request's token, then completes the request.
    fn read_block_irq_driven(&self, block_id: u64, out_buf: &mut [u8]) -> DriverResult {
        let _request = self.request_gate.acquire_request();
        let mut req = BlkReq::default();
        let mut resp = BlkResp::default();
        let token = {
            let mut inner = self.device.lock();
            // SAFETY: `req`, `out_buf`, and `resp` stay on this task's stack
            // and are not accessed between this submit and the matching
            // `complete_read_blocks`; the task blocks in the wait below
            // without returning or unwinding, so the borrows stay valid for
            // the whole in-flight window.
            unsafe { inner.read_blocks_nb(block_id as usize, &mut req, out_buf, &mut resp) }
        }
        .map_err(as_driver_error)?;

        let slot = self.completion_slots.register(token);
        // Catch a completion that raced between submit and registration; the
        // used-ring head cannot have moved past this token (only its owner
        // pops it), so this dispatch observes it directly.
        self.completion_slots.dispatch_from_device(&self.device);
        self.wait_for_slot(&slot);

        self.finish_request(token, |inner| {
            // SAFETY: the same buffers and token as the `read_blocks_nb`
            // call above, and the used-ring head reported this token
            // complete, so the device has finished the request.
            unsafe { inner.complete_read_blocks(token, &req, out_buf, &mut resp) }
                .map_err(as_driver_error)
        })
    }

    /// Submits a write request and sleeps until the Block softirq marks this
    /// request's token, then completes the request.
    fn write_block_irq_driven(&self, block_id: u64, in_buf: &[u8]) -> DriverResult {
        let _request = self.request_gate.acquire_request();
        let mut req = BlkReq::default();
        let mut resp = BlkResp::default();
        let token = {
            let mut inner = self.device.lock();
            // SAFETY: same borrow contract as `read_block_irq_driven`; the
            // buffers stay valid on this task's stack until completion.
            unsafe { inner.write_blocks_nb(block_id as usize, &mut req, in_buf, &mut resp) }
        }
        .map_err(as_driver_error)?;

        let slot = self.completion_slots.register(token);
        self.completion_slots.dispatch_from_device(&self.device);
        self.wait_for_slot(&slot);

        self.finish_request(token, |inner| {
            // SAFETY: the same buffers and token as the `write_blocks_nb`
            // call above, and the used-ring head reported this token
            // complete.
            unsafe { inner.complete_write_blocks(token, &req, in_buf, &mut resp) }
                .map_err(as_driver_error)
        })
    }
}

impl<H: Hal, T: Transport> Drop for VirtIoBlkDev<H, T> {
    fn drop(&mut self) {
        if let Some(irq_device) = self.irq_device.take() {
            BLK_COMPLETION_HUB.unregister(&irq_device);
        }
    }
}

impl<H: Hal, T: Transport> Device for VirtIoBlkDev<H, T> {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }
}

impl<H: Hal + 'static, T: Transport + 'static> BlockDeviceOperations for VirtIoBlkDev<H, T> {
    #[inline]
    fn num_blocks(&self) -> u64 {
        self.num_blocks
    }

    #[inline]
    fn block_size(&self) -> usize {
        self.sector_size
    }

    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        match &self.irq_completion {
            Some(_) => self.read_block_irq_driven(block_id, buf),
            None => self.read_sector(block_id, buf),
        }
    }

    fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult {
        match &self.irq_completion {
            Some(_) => self.write_block_irq_driven(block_id, buf),
            None => self.write_sector(block_id, buf),
        }
    }

    /// Flush keeps the synchronous helper (`virtio-drivers` offers no
    /// non-blocking flush submit): the flush barrier guarantees no other
    /// request is in flight, so the helper's single-buffer assumption holds.
    /// Polling mode is already serialized by the device lock.
    fn flush(&self) -> DriverResult {
        if self.irq_completion.is_some() {
            let _flush = self.request_gate.begin_flush();
            self.device.lock().flush().map_err(as_driver_error)
        } else {
            self.device.lock().flush().map_err(as_driver_error)
        }
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::{
        irq_completion::test_support::ScopedFailingIrqProvider,
        mock_virtio::{MockHal, MockTransport},
    };

    #[def_test]
    fn test_virtio_blk_init_failure_handling() {
        let transport = MockTransport::new();
        let dev = VirtIoBlkDev::<MockHal, MockTransport>::try_new(transport, None);

        if let Ok(d) = dev {
            assert!(d.name().starts_with("vd"));
            assert!(d.name().len() >= 3);
            assert_eq!(d.device_kind(), DeviceKind::Block);
            assert_eq!(d.block_size(), 512);
        } else {
            assert!(dev.is_err());
        }
    }

    #[def_test(serial)]
    fn test_irq_registration_failure_falls_back_to_polling() {
        // With a provider whose line requests always fail, IRQ registration
        // must roll back and the device must keep the polling path.
        let _failing_provider = ScopedFailingIrqProvider::new();
        let transport = MockTransport::new();
        let dev = VirtIoBlkDev::<MockHal, MockTransport>::try_new(transport, Some(77))
            .expect("device init succeeds with a mock transport");

        assert!(dev.irq_completion.is_none());
        assert!(dev.irq_device.is_none());
    }

    #[def_test]
    fn test_dispatch_marks_each_registered_token_once() {
        let slots = BlkCompletionSlots::new(16);
        let first = slots.register(3);
        let second = slots.register(7);
        let first_wakes = Arc::new(AtomicUsize::new(0));
        let registration = register_slot_waker(&first, &first_wakes);

        // Scripted peek models a device whose head advances immediately (as
        // if each owner popped): one dispatch drains both tokens.
        let script = [Some(3u16), Some(7), None, None];
        let mut cursor = 0;
        slots.dispatch_with(|| {
            let token = script[cursor];
            cursor += 1;
            token
        });

        assert!(first.is_done());
        assert!(second.is_done());
        assert_eq!(first_wakes.load(Ordering::SeqCst), 1);

        drop(registration);
    }

    #[def_test]
    fn test_dispatch_stops_at_unregistered_head() {
        let slots = BlkCompletionSlots::new(16);
        let slot = slots.register(7);

        // Head 5 has no registered slot (submit-to-register race window);
        // dispatch must stop without touching the later token.
        let script = [Some(5u16), Some(7), None];
        let mut cursor = 0;
        slots.dispatch_with(|| {
            let token = script[cursor];
            cursor += 1;
            token
        });

        assert!(!slot.is_done());
    }

    #[def_test]
    fn test_dispatch_remarks_nothing_for_stationary_head() {
        let slots = BlkCompletionSlots::new(16);
        let slot = slots.register(3);
        let wakes = Arc::new(AtomicUsize::new(0));
        let registration = register_slot_waker(&slot, &wakes);

        // The real device keeps the head until its owner pops it: repeated
        // dispatches see the same token and must not wake again.
        slots.dispatch_with(|| Some(3));
        slots.dispatch_with(|| Some(3));
        slots.dispatch_with(|| Some(3));

        assert!(slot.is_done());
        assert_eq!(wakes.load(Ordering::SeqCst), 1);

        drop(registration);
    }

    #[def_test]
    fn test_request_gate_flush_barrier() {
        let gate = RequestGate::new();
        let first = gate.acquire_request();
        let second = gate.acquire_request();
        assert_eq!(gate.snapshot().in_flight, 2);

        drop(first);
        drop(second);
        assert_eq!(gate.snapshot().in_flight, 0);

        let flush = gate.begin_flush();
        let snapshot = gate.snapshot();
        assert!(snapshot.is_flushing);
        assert_eq!(snapshot.in_flight, 0);

        // While a flush holds the gate, a fresh request claim (evaluated
        // inline as the claim predicate would) must be rejected.
        let state = gate.state.lock();
        let can_start_request = !state.is_flushing;
        drop(state);
        assert!(!can_start_request);

        drop(flush);
        assert!(!gate.snapshot().is_flushing);
    }

    fn register_slot_waker(slot: &BlkSlot, counter: &Arc<AtomicUsize>) -> kpoll::PollRegistration {
        struct CountingWaker {
            counter: Arc<AtomicUsize>,
        }
        impl alloc::task::Wake for CountingWaker {
            fn wake(self: Arc<Self>) {
                self.counter.fetch_add(1, Ordering::SeqCst);
            }
        }
        let waker = core::task::Waker::from(Arc::new(CountingWaker {
            counter: counter.clone(),
        }));
        slot.wake
            .register(&waker)
            .expect("test waker registration must succeed")
    }

    #[def_test]
    fn test_vd_name_base26() {
        assert_eq!(vd_name(0), "a");
        assert_eq!(vd_name(1), "b");
        assert_eq!(vd_name(25), "z");
        assert_eq!(vd_name(26), "aa");
        assert_eq!(vd_name(27), "ab");
        assert_eq!(vd_name(51), "az");
        assert_eq!(vd_name(52), "ba");
        assert_eq!(vd_name(701), "zz");
    }

    #[def_test]
    fn test_virtio_blk_concurrency_traits() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<VirtIoBlkDev<MockHal, MockTransport>>();
    }
}
