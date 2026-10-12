// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO network driver adapter.
use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::Ordering;

use device_traits::{Device, DeviceKind, DriverError, DriverResult};
use driver_net::{
    MacAddress, NetBuf, NetBufBox, NetBufHandle, NetBufPool, NetDevice, NetRxScheduler,
};
use kspin::{SpinNoIrq, SpinNoPreempt};
use virtio_drivers::{Hal, device::net::VirtIONetRaw as InnerDev, transport::Transport};

use crate::as_driver_error;

const NET_BUF_LEN: usize = 1526;

/// Per-device interrupt worker: acknowledges this device's virtqueues and
/// schedules RX if a scheduler has been installed.
///
/// Captures only shared internals (`Arc`s of the inner device and the RX
/// scheduler slot), so it stays valid independently of who owns the
/// published device. The caller registers it with the device's interrupt
/// through its own device model — there is no global IRQ routing here.
#[derive(Clone)]
pub struct VirtIoNetIrqWorker {
    service: Arc<dyn Fn() -> bool + Send + Sync>,
}

/// The worker's captured share of the device: the lock-protected inner
/// device and the RX scheduler slot.
struct IrqWorkerCore<H: Hal, T: Transport, const QS: usize> {
    inner: Arc<SpinNoIrq<InnerDev<H, T, QS>>>,
    rx_scheduler: Arc<SpinNoIrq<Option<Arc<dyn NetRxScheduler>>>>,
}

// SAFETY: every access to `inner` is serialized by its SpinNoIrq (IRQ context
// excluded), and the scheduler slot likewise; sharing across threads is the
// same guarantee `VirtIoNetDev` itself documents.
unsafe impl<H: Hal, T: Transport, const QS: usize> Send for IrqWorkerCore<H, T, QS> {}
// SAFETY: see the `Send` impl.
unsafe impl<H: Hal, T: Transport, const QS: usize> Sync for IrqWorkerCore<H, T, QS> {}

impl VirtIoNetIrqWorker {
    /// Acknowledge and schedule; returns whether the interrupt was ours.
    pub fn service(&self) -> bool {
        (self.service)()
    }
}

/// The VirtIO network device driver.
///
/// Wraps `VirtIONetRaw` from `virtio-drivers` and implements the
/// [`NetDevice`] trait, providing packet-level send/receive with buffer
/// pool management, interrupt-driven IRQ acknowledgment, and a polling
/// fallback when no IRQ is registered.
///
/// `QS` is the VirtIO queue size.
///
/// # Type Parameters
///
/// - `H` - VirtIO HAL implementation for DMA allocation.
/// - `T` - Transport layer (MMIO or PCI).
/// - `QS` - VirtIO queue size (number of descriptors).
///
/// # Concurrency
///
/// The inner device is wrapped in `Arc<SpinNoIrq<...>>` because a configured
/// IRQ callback runs in interrupt context and must access the device
/// concurrently with normal send/receive operations.
///
/// # Example
///
/// ```ignore
/// // Transport probing goes through the virtio extension service
/// // (`DeviceCtx::extension::<PciTransportService>()` in drv/virtio);
/// // the device takes only the transport and an optional MMIO register
/// // lease (virtio-mmio path):
/// let mut net = VirtIoNetDev::<HalImpl, _, 256>::try_new(transport, None)?;
/// let tx_buf = net.alloc_tx_buf(1500)?;
/// // ... fill tx_buf and send ...
/// net.send(tx_buf)?;
/// let rx_buf = net.recv()?;
/// ```
pub struct VirtIoNetDev<H: Hal, T: Transport, const QS: usize> {
    /// MMIO register lease: the mapping stays mapped as long as ANY
    /// reference to this device object (class publication, consumer clones)
    /// lives — the last drop releases the mapping (Linux: pci_release_regions
    /// runs after the last put_device).
    _mmio_regs: Option<driver_core::IoMap>,
    bufs: SpinNoPreempt<NetBufState<QS>>,
    // Kept alive so that `NetBufBox`es in `bufs` remain valid.
    #[expect(
        dead_code,
        reason = "CG-024-lint-exceptions: dead_code: Own the RX buffer pool for the lifetime of \
                  the network device even when no direct field read is needed"
    )]
    buf_pool: Arc<NetBufPool>,
    inner: Arc<SpinNoIrq<InnerDev<H, T, QS>>>,
    mac: MacAddress,
    rx_scheduler: Arc<SpinNoIrq<Option<Arc<dyn NetRxScheduler>>>>,
    /// Set once an interrupt worker has been handed out, so `recv()` knows
    /// the ISR is serviced from interrupt context.
    irq_backed: core::sync::atomic::AtomicBool,
}

/// Buffer bookkeeping for in-flight and free network buffers.
///
/// This state is only touched from task context, so it is guarded by a
/// `SpinNoPreempt` lock. A configured IRQ handler only acks the device via
/// `inner` and never touches these buffers, so the two locks never deadlock.
struct NetBufState<const QS: usize> {
    rx_buffers: [Option<NetBufBox>; QS],
    tx_buffers: [Option<NetBufBox>; QS],
    free_tx_bufs: Vec<NetBufBox>,
}

// SAFETY: VirtIoNetDev's device state (InnerDev) is protected by SpinNoIrq and
// its buffer bookkeeping by SpinNoPreempt. Both locks provide the mutual
// exclusion required to transfer across threads and share immutable references.
unsafe impl<H: Hal, T: Transport, const QS: usize> Send for VirtIoNetDev<H, T, QS> {}
// SAFETY: VirtIoNetDev's device state (InnerDev) is protected by SpinNoIrq and
// its buffer bookkeeping by SpinNoPreempt, so shared references remain synchronized.
unsafe impl<H: Hal, T: Transport, const QS: usize> Sync for VirtIoNetDev<H, T, QS> {}

impl<H: Hal + 'static, T: Transport + 'static, const QS: usize> VirtIoNetDev<H, T, QS> {
    /// Creates a new driver instance and initializes the device, or returns
    /// an error if any step fails.
    ///
    /// # Arguments
    ///
    /// - `transport` - The VirtIO transport (MMIO or PCI) for this device.
    /// - `irq` - Optional IRQ number. If provided, an interrupt handler is
    ///   registered to acknowledge device interrupts. If omitted, `recv()`
    ///   acknowledges pending interrupts while polling the receive queue.
    ///
    /// # Errors
    ///
    /// Returns `DriverError` if:
    /// - Device initialization fails (feature negotiation, queue setup).
    /// - Buffer pool allocation fails (`NoMemory`).
    /// - IRQ registration fails (`ResourceBusy`).
    ///
    /// # Panics
    ///
    /// Panics if `receive_begin` returns a token that does not match the
    /// expected sequential index (asserts internal consistency).
    pub fn try_new(transport: T, mmio_regs: Option<driver_core::IoMap>) -> DriverResult<Self> {
        // 0. Create a new driver instance.
        const NONE_BUF: Option<NetBufBox> = None;
        let inner = Arc::new(SpinNoIrq::new(
            InnerDev::new(transport).map_err(as_driver_error)?,
        ));
        let mut rx_buffers = [NONE_BUF; QS];
        let tx_buffers = [NONE_BUF; QS];
        let buf_pool = NetBufPool::new(2 * QS, NET_BUF_LEN)?;
        let mut free_tx_bufs = Vec::with_capacity(QS);

        // 1. Fill all rx buffers.
        for (i, rx_buf_place) in rx_buffers.iter_mut().enumerate() {
            let mut rx_buf = buf_pool.alloc_boxed().ok_or(DriverError::NoMemory)?;
            // SAFETY: `receive_begin` requires exclusive access to the buffer
            // for the duration it is in the VirtIO queue. The buffer is owned
            // by `rx_buf` and stored in `rx_buffers[i]` immediately after,
            // ensuring it lives as long as the device holds the token.
            let token = unsafe {
                inner
                    .lock()
                    .receive_begin(rx_buf.buffer_mut())
                    .map_err(as_driver_error)?
            };
            assert_eq!(token, i as u16);
            *rx_buf_place = Some(rx_buf);
        }

        // 2. Allocate all tx buffers.
        for _ in 0..QS {
            let mut tx_buf = buf_pool.alloc_boxed().ok_or(DriverError::NoMemory)?;
            // Fill header
            let hdr_len = inner
                .lock()
                .fill_buffer_header(tx_buf.buffer_mut())
                .or(Err(DriverError::InvalidInput))?;
            tx_buf.set_hdr_len(hdr_len)?;
            free_tx_bufs.push(tx_buf);
        }

        let rx_scheduler = Arc::new(SpinNoIrq::new(None));

        // 3. Cache immutable device properties.
        let mac = MacAddress(inner.lock().mac_address());

        // 4. Return the driver instance.
        Ok(Self {
            _mmio_regs: mmio_regs,
            bufs: SpinNoPreempt::new(NetBufState {
                rx_buffers,
                tx_buffers,
                free_tx_bufs,
            }),
            buf_pool,
            inner,
            mac,
            rx_scheduler,
            irq_backed: core::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Mark the device as interrupt-driven.
    ///
    /// Call ONLY after the irq registration succeeded; `recv()` and the
    /// rx-scheduler contract depend on it (see `irq_worker`).
    pub fn mark_irq_backed(&self) {
        self.irq_backed.store(true, Ordering::Release);
    }

    /// Build this device's interrupt worker (see [`VirtIoNetIrqWorker`]).
    ///
    /// The worker acknowledges only this device's queues and kicks the RX
    /// scheduler when one is installed; callers wire it to the device's
    /// interrupt through their device model.
    pub fn irq_worker(self: &Arc<Self>) -> VirtIoNetIrqWorker {
        // NOTE: this does NOT mark the device irq-backed. The caller wires
        // the worker to an actual interrupt; only a SUCCESSFUL registration
        // may flip the flag (mark_irq_backed). Building a worker before
        // knowing the irq outcome used to leave interrupt-less devices
        // pretending to be irq-backed: recv() skipped its task-context ack
        // and the RX scheduler was armed but never fired.
        let core = Arc::new(IrqWorkerCore {
            inner: self.inner.clone(),
            rx_scheduler: self.rx_scheduler.clone(),
        });
        VirtIoNetIrqWorker {
            service: Arc::new(move || {
                let handled = !core.inner.lock().ack_interrupt().is_empty();
                if handled {
                    let scheduler = core.rx_scheduler.lock().clone();
                    if let Some(scheduler) = scheduler {
                        scheduler.schedule_rx();
                    }
                }
                handled
            }),
        }
    }
}

impl<H: Hal, T: Transport, const QS: usize> Device for VirtIoNetDev<H, T, QS> {
    fn name(&self) -> &str {
        "virtio-net"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Net
    }

    fn irq(&self) -> Option<usize> {
        None
    }
}

impl<H: Hal, T: Transport, const QS: usize> NetDevice for VirtIoNetDev<H, T, QS> {
    #[inline]
    fn mac(&self) -> MacAddress {
        self.mac
    }

    #[inline]
    fn can_tx(&self) -> bool {
        let has_free = !self.bufs.lock().free_tx_bufs.is_empty();
        has_free && self.inner.lock().can_send()
    }

    #[inline]
    fn can_rx(&self) -> bool {
        self.inner.lock().poll_receive().is_some()
    }

    #[inline]
    fn rx_queue_len(&self) -> usize {
        QS
    }

    #[inline]
    fn tx_queue_len(&self) -> usize {
        QS
    }

    fn recycle_rx(&self, rx_buf: NetBufHandle) -> DriverResult {
        // SAFETY: `from_handle` converts a handle back to an owned `NetBuf`.
        // The caller guarantees the handle is valid and not used elsewhere.
        let mut rx_buf = unsafe { NetBuf::from_handle(rx_buf) };
        let mut bufs = self.bufs.lock();
        // SAFETY: `receive_begin` requires exclusive access to the buffer.
        // We own `rx_buf` and will store it in `rx_buffers[new_token]`
        // immediately after, ensuring it lives as long as the device holds
        // the token.
        let new_token = unsafe {
            self.inner
                .lock()
                .receive_begin(rx_buf.buffer_mut())
                .map_err(as_driver_error)?
        };
        // `rx_buffers[new_token]` is expected to be `None` since it was taken
        // away at `Self::recv()` and has not been added back.
        if bufs.rx_buffers[new_token as usize].is_some() {
            return Err(DriverError::BadState);
        }
        bufs.rx_buffers[new_token as usize] = Some(rx_buf);
        Ok(())
    }

    fn recycle_tx(&self) -> DriverResult {
        loop {
            let token = {
                let mut inner = self.inner.lock();
                inner.poll_transmit()
            };
            let Some(token) = token else {
                break;
            };
            // Take the `bufs` lock only for the duration of a single token's
            // bookkeeping so concurrent network I/O on this device is not
            // blocked for the whole drain loop.
            let mut bufs = self.bufs.lock();
            let tx_buf = bufs.tx_buffers[token as usize]
                .take()
                .ok_or(DriverError::BadState)?;
            // SAFETY: `transmit_complete` requires that the buffer frame
            // matches the token's previously submitted buffer. We obtained
            // `tx_buf` from `tx_buffers[token]`, which was placed there by
            // `send()`, so the mapping is correct.
            unsafe {
                self.inner
                    .lock()
                    .transmit_complete(token, tx_buf.frame())
                    .map_err(as_driver_error)?;
            }
            // Recycle the buffer.
            bufs.free_tx_bufs.push(tx_buf);
        }
        Ok(())
    }

    fn send(&self, tx_buf: NetBufHandle) -> DriverResult {
        // SAFETY: `from_handle` converts a handle back to an owned `NetBuf`.
        // The caller guarantees the handle is valid and not used elsewhere.
        let tx_buf = unsafe { NetBuf::from_handle(tx_buf) };
        let mut bufs = self.bufs.lock();
        // SAFETY: `transmit_begin` requires exclusive access to the buffer
        // frame for the duration it is in the VirtIO queue. We store the
        // buffer in `tx_buffers[token]` immediately after, ensuring it
        // remains valid until `recycle_tx()` retrieves it.
        let token = unsafe {
            self.inner
                .lock()
                .transmit_begin(tx_buf.frame())
                .map_err(as_driver_error)?
        };
        bufs.tx_buffers[token as usize] = Some(tx_buf);
        Ok(())
    }

    fn recv(&self) -> DriverResult<NetBufHandle> {
        if !self.irq_backed.load(Ordering::Acquire) {
            // Task context owns ISR acknowledgment when no IRQ handler exists.
            self.inner.lock().ack_interrupt();
        }
        // For IRQ-backed devices, reading the ISR here could clear an event
        // that arrived after the handler scheduled RX.
        let mut bufs = self.bufs.lock();
        let token = {
            let inner = self.inner.lock();
            inner.poll_receive()
        };
        if let Some(token) = token {
            let mut rx_buf = bufs.rx_buffers[token as usize]
                .take()
                .ok_or(DriverError::BadState)?;
            // SAFETY: `receive_complete` requires that the buffer matches the
            // token's previously submitted buffer. We obtained `rx_buf` from
            // `rx_buffers[token]`, which was placed there during `try_new()`
            // or `recycle_rx()`, so the mapping is correct.
            let (hdr_len, pkt_len) = unsafe {
                self.inner
                    .lock()
                    .receive_complete(token, rx_buf.buffer_mut())
                    .map_err(as_driver_error)?
            };
            rx_buf.set_hdr_len(hdr_len)?;
            rx_buf.set_payload_len(pkt_len)?;

            Ok(rx_buf.into_handle())
        } else {
            Err(DriverError::WouldBlock)
        }
    }

    fn alloc_tx_buf(&self, size: usize) -> DriverResult<NetBufHandle> {
        // 0. Allocate a buffer from the queue.
        let mut net_buf = self
            .bufs
            .lock()
            .free_tx_bufs
            .pop()
            .ok_or(DriverError::NoMemory)?;
        let pkt_len = size;

        net_buf.set_payload_len(pkt_len)?;

        Ok(net_buf.into_handle())
    }

    fn set_rx_scheduler(&self, scheduler: Option<Arc<dyn NetRxScheduler>>) -> DriverResult {
        // Only meaningful with interrupt delivery: a scheduler without an
        // IRQ worker would never run.
        if scheduler.is_some() && !self.irq_backed.load(Ordering::Acquire) {
            return Err(DriverError::Unsupported);
        }
        *self.rx_scheduler.lock() = scheduler;
        Ok(())
    }
}

#[unittest::mod_test]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::{assert_eq, def_test};

    use super::*;
    use crate::mock_virtio::{MockHal, MockTransport};

    struct MockRxScheduler {
        schedules: AtomicUsize,
    }

    impl NetRxScheduler for MockRxScheduler {
        fn schedule_rx(&self) {
            self.schedules.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn net_dev() -> (
        Arc<VirtIoNetDev<MockHal, MockTransport, 32>>,
        Arc<AtomicUsize>,
    ) {
        let mut transport = MockTransport::new();
        transport.device_type = virtio_drivers::transport::DeviceType::Network;
        let interrupt_ack_count = transport.interrupt_ack_count.clone();
        (
            Arc::new(VirtIoNetDev::try_new(transport, None).unwrap()),
            interrupt_ack_count,
        )
    }

    #[def_test(serial)]
    fn test_rx_scheduler_requires_irq_worker_but_allows_detach() {
        let (dev, _) = net_dev();
        let scheduler = Arc::new(MockRxScheduler {
            schedules: AtomicUsize::new(0),
        });

        // Without an interrupt worker there is nothing to drive the scheduler.
        assert_eq!(
            dev.set_rx_scheduler(Some(scheduler.clone())),
            Err(DriverError::Unsupported)
        );

        // The worker alone does NOT mark the device interrupt-backed —
        // only a successful irq registration may (the driver calls
        // mark_irq_backed).
        let worker = dev.irq_worker();
        assert_eq!(
            dev.set_rx_scheduler(Some(scheduler.clone())),
            Err(DriverError::Unsupported)
        );
        dev.mark_irq_backed();
        assert_eq!(dev.set_rx_scheduler(Some(scheduler.clone())), Ok(()));
        assert_eq!(dev.set_rx_scheduler(None), Ok(()));
        drop(worker);
    }

    #[def_test]
    fn test_recv_acknowledges_interrupt_in_poll_mode() {
        let (dev, interrupt_ack_count) = net_dev();
        assert!(matches!(dev.recv(), Err(DriverError::WouldBlock)));
        assert_eq!(interrupt_ack_count.load(Ordering::Relaxed), 1);
    }

    #[def_test(serial)]
    fn test_recv_leaves_interrupt_ack_to_worker() {
        let (dev, interrupt_ack_count) = net_dev();
        let worker = dev.irq_worker();
        dev.mark_irq_backed();

        assert!(matches!(dev.recv(), Err(DriverError::WouldBlock)));
        assert_eq!(interrupt_ack_count.load(Ordering::Relaxed), 0);
        drop(worker);
    }

    #[def_test(serial)]
    fn test_worker_services_and_schedules_rx() {
        let (dev, interrupt_ack_count) = net_dev();
        let worker = dev.irq_worker();
        dev.mark_irq_backed();
        let scheduler = Arc::new(MockRxScheduler {
            schedules: AtomicUsize::new(0),
        });
        assert_eq!(dev.set_rx_scheduler(Some(scheduler.clone())), Ok(()));

        // Mock transport acks report a pending interrupt exactly once.
        let handled = worker.service();
        assert_eq!(interrupt_ack_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            scheduler.schedules.load(Ordering::Relaxed),
            if handled { 1 } else { 0 }
        );

        // A second service with nothing pending must not schedule again.
        worker.service();
        assert_eq!(
            scheduler.schedules.load(Ordering::Relaxed),
            if handled { 1 } else { 0 }
        );
    }

    #[def_test]
    fn test_virtio_net_concurrency_traits() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<VirtIoNetDev<MockHal, MockTransport, 32>>();
    }
}
