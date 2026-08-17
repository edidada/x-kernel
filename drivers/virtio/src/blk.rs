// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO block driver adapter.
use alloc::{string::String, sync::Arc};
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

/// hardirq-facing view of one virtio-blk device.
struct VirtIoBlkIrqDevice<H: Hal, T: Transport> {
    inner: Arc<SpinNoIrq<InnerDev<H, T>>>,
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

    fn entry(&self) -> &Arc<IrqCompletionEntry> {
        &self.entry
    }
}

/// Serializes device requests: at most one request is in flight at a time.
///
/// This restores the request serialization the polling path always had (a
/// whole request ran under the device lock), for two reasons:
///
/// - the `virtio-drivers` synchronous helpers (used by `flush`) require that
///   the device is not processing any other buffer, and popping a used-ring
///   entry whose head token differs from the caller's returns `WrongToken`
///   *without recycling the descriptor chain*;
/// - the completion wake is a broadcast without per-request accounting, and
///   with a single in-flight request the used-ring head can only be the
///   waiter's own token, so a wake can never be "consumed" by an unrelated
///   requester.
///
/// Unlike the old serialization, the requester sleeps between submit and
/// completion instead of busy-polling with local interrupts masked.
struct IrqRequestGate {
    busy: AtomicBool,
    wake: PollSet,
}

impl IrqRequestGate {
    fn new() -> Self {
        Self {
            busy: AtomicBool::new(false),
            wake: PollSet::new(),
        }
    }

    /// Acquires the gate, sleeping until the current holder releases it.
    fn acquire(&self) -> BlkRequestGuard<'_> {
        if !self.busy.swap(true, Ordering::AcqRel) {
            return BlkRequestGuard(self);
        }
        // The predicate performs the acquisition swap itself; the swap is
        // idempotent while the gate is already held, so repeated polls are
        // safe and exactly one waiter can observe a false previous value.
        if !wait_poll_set(&self.wake, || !self.busy.swap(true, Ordering::AcqRel)) {
            // No sleepable context: spin until the swap succeeds. The holder
            // completes through its own wait path, so the gate is released.
            while self.busy.swap(true, Ordering::AcqRel) {
                core::hint::spin_loop();
            }
        }
        BlkRequestGuard(self)
    }

    fn release(&self) {
        self.busy.store(false, Ordering::Release);
        self.wake.wake();
    }
}

/// RAII guard for one held [`IrqRequestGate`] acquisition.
struct BlkRequestGuard<'a>(&'a IrqRequestGate);

impl Drop for BlkRequestGuard<'_> {
    fn drop(&mut self) {
        self.0.release();
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
/// the Linux-style top/bottom-half split:
///
/// - the hardirq handler acknowledges the device interrupt status, marks the
///   device pending, and raises the Block softirq vector;
/// - the Block softirq action broadcasts a wake for sleeping requesters;
/// - the requester task, having submitted the request with the non-blocking
///   virtqueue API, rechecks the used ring on every wake and completes the
///   request itself once its token reaches the ring head.
///
/// Requests are serialized by an internal gate (one in flight at a time),
/// matching the request ordering the previous synchronous driver had, while
/// the requester sleeps instead of busy-polling with interrupts masked.
///
/// Without an IRQ number (or when IRQ registration fails), requests keep the
/// original synchronous busy-poll path under [`SpinNoIrq`].
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
    /// Request gate; only meaningful in the IRQ-driven mode, where it
    /// serializes submit/wait/complete and the sync flush against each other.
    request_gate: IrqRequestGate,
    /// Shared completion entry; `None` keeps the synchronous busy-poll path.
    irq_completion: Option<Arc<IrqCompletionEntry>>,
    /// Keeps the hardirq-facing device object registered until drop.
    irq_device: Option<Arc<dyn IrqCompletionDevice>>,
}

// SAFETY: VirtIoBlkDev serializes all access to the inner VirtIOBlk through
// its own `SpinNoIrq` lock (polling mode) or the request gate plus short
// lock-held critical sections (IRQ mode). The inner VirtIOBlk is not auto
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
    ///   succeeds, request completions are deferred through the Block softirq
    ///   as described in the [interrupt model](#interrupt-model). When
    ///   omitted, the driver keeps the synchronous busy-poll behavior.
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
        let (irq_completion, irq_device) = match irq {
            Some(irq) => {
                let entry = Arc::new(IrqCompletionEntry::new(irq));
                let irq_device: Arc<dyn IrqCompletionDevice> = Arc::new(VirtIoBlkIrqDevice {
                    inner: device.clone(),
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
            request_gate: IrqRequestGate::new(),
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

    /// Submits a read request and sleeps until the Block softirq wake, then
    /// completes the request.
    fn read_block_irq_driven(
        &self,
        entry: &Arc<IrqCompletionEntry>,
        block_id: u64,
        out_buf: &mut [u8],
    ) -> DriverResult {
        let _gate = self.request_gate.acquire();
        let mut req = BlkReq::default();
        let mut resp = BlkResp::default();
        let token = {
            let mut inner = self.device.lock();
            // SAFETY: `req`, `out_buf`, and `resp` stay on this task's stack
            // and are not accessed between this submit and the matching
            // `complete_read_blocks`; the task blocks in the wait loop below
            // without returning or unwinding, so the borrows stay valid for
            // the whole in-flight window.
            unsafe { inner.read_blocks_nb(block_id as usize, &mut req, out_buf, &mut resp) }
        }
        .map_err(as_driver_error)?;

        self.wait_until_request_completes(entry, token);

        let mut inner = self.device.lock();
        // SAFETY: the same buffers and token as the `read_blocks_nb` call
        // above, and the used-ring head now reports `token`, so the device
        // has finished the request.
        unsafe { inner.complete_read_blocks(token, &req, out_buf, &mut resp) }
            .map_err(as_driver_error)
    }

    /// Submits a write request and sleeps until the Block softirq wake, then
    /// completes the request.
    fn write_block_irq_driven(
        &self,
        entry: &Arc<IrqCompletionEntry>,
        block_id: u64,
        in_buf: &[u8],
    ) -> DriverResult {
        let _gate = self.request_gate.acquire();
        let mut req = BlkReq::default();
        let mut resp = BlkResp::default();
        let token = {
            let mut inner = self.device.lock();
            // SAFETY: same borrow contract as `read_block_irq_driven`; the
            // buffers stay valid on this task's stack until completion.
            unsafe { inner.write_blocks_nb(block_id as usize, &mut req, in_buf, &mut resp) }
        }
        .map_err(as_driver_error)?;

        self.wait_until_request_completes(entry, token);

        let mut inner = self.device.lock();
        // SAFETY: the same buffers and token as the `write_blocks_nb` call
        // above, and the used-ring head now reports `token`.
        unsafe { inner.complete_write_blocks(token, &req, in_buf, &mut resp) }
            .map_err(as_driver_error)
    }

    /// Sleeps until the used-ring head reports `token`.
    ///
    /// The softirq wake is a broadcast without per-request accounting, so the
    /// device state is rechecked on every wake; with the request gate held,
    /// the head can only be this requester's own token. When the current
    /// context cannot sleep (early boot or an IRQ-context caller), the used
    /// ring is polled with local interrupts enabled; the registered hardirq
    /// handler keeps acknowledging the device ISR, so a shared
    /// level-triggered line cannot livelock the CPU.
    fn wait_until_request_completes(&self, entry: &Arc<IrqCompletionEntry>, token: u16) {
        if entry.wait_until(|| self.device.lock().peek_used() == Some(token)) {
            return;
        }
        while self.device.lock().peek_used() != Some(token) {
            core::hint::spin_loop();
        }
    }
}

impl<H: Hal, T: Transport> Drop for VirtIoBlkDev<H, T> {
    fn drop(&mut self) {
        if let Some(irq_device) = self.irq_device.take() {
            BLK_COMPLETION_HUB.unregister(&irq_device);
        }
    }
}

impl<H: Hal + 'static, T: Transport + 'static> Device for VirtIoBlkDev<H, T> {
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
            Some(entry) => self.read_block_irq_driven(entry, block_id, buf),
            None => self.read_sector(block_id, buf),
        }
    }

    fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult {
        match &self.irq_completion {
            Some(entry) => self.write_block_irq_driven(entry, block_id, buf),
            None => self.write_sector(block_id, buf),
        }
    }

    /// Flush keeps the synchronous helper: `virtio-drivers` offers no
    /// non-blocking flush submit. In IRQ mode the request gate is held for
    /// the flush, so no other request is in flight and the helper's
    /// single-buffer assumption holds; polling mode is already serialized by
    /// the device lock.
    fn flush(&self) -> DriverResult {
        let _gate = self
            .irq_completion
            .as_ref()
            .map(|_| self.request_gate.acquire());
        self.device.lock().flush().map_err(as_driver_error)
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

    #[def_test(serial)]
    fn test_request_gate_release_wakes_waiter_and_reacquires() {
        let gate = IrqRequestGate::new();
        let guard = gate.acquire();
        assert!(gate.busy.load(Ordering::Acquire));

        // A waiter registered on the gate's broadcast source must be woken
        // by release, and the gate must become acquirable again.
        let wakes = Arc::new(AtomicUsize::new(0));
        let registration = register_gate_waker(&gate, &wakes);
        drop(guard);
        assert!(!gate.busy.load(Ordering::Acquire));
        assert_eq!(wakes.load(Ordering::SeqCst), 1);

        let _regained = gate.acquire();
        drop(registration);
    }

    fn register_gate_waker(
        gate: &IrqRequestGate,
        counter: &Arc<AtomicUsize>,
    ) -> kpoll::PollRegistration {
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
        gate.wake
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
