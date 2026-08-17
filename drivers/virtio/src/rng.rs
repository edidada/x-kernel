// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO entropy source (hardware RNG) driver adapter.
//!
//! Two request paths exist:
//!
//! - **IRQ-driven** (an IRQ number is available and registration succeeds):
//!   the hardirq handler acknowledges the device interrupt status and raises
//!   the Tasklet softirq; the softirq action broadcasts a wake, and the
//!   requester sleeps between submit and completion (rechecking the used
//!   ring on every wake) instead of spinning with local interrupts masked.
//! - **Polled fallback**: the legacy bounded busy-wait under [`SpinNoIrq`],
//!   kept for platforms without a usable IRQ so a wedged host/device cannot
//!   hang the caller indefinitely.

use alloc::{boxed::Box, sync::Arc};
use core::hint::spin_loop;

use char_driver::CharDevice;
use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use khal::time::monotonic_time;
use kirq::softirq::SoftirqVec;
use kspin::SpinNoIrq;
use ktime_types::TimeSpan;
use virtio_drivers::{
    Hal, Result as VirtIoResult, device::common::Feature, queue::VirtQueue, transport::Transport,
};

use crate::{
    as_driver_error,
    irq_completion::{IrqCompletionDevice, IrqCompletionEntry, IrqCompletionHub},
};

const QUEUE_IDX: u16 = 0;
const QUEUE_SIZE: usize = 8;
const SUPPORTED_FEATURES: Feature = Feature::RING_INDIRECT_DESC
    .union(Feature::RING_EVENT_IDX)
    .union(Feature::VERSION_1)
    .union(Feature::ACCESS_PLATFORM);

/// Upper bound on a single entropy request while local IRQs are masked
/// (polled fallback).
///
/// Tuned to tolerate a slow host `/dev/urandom` backend without hanging the
/// guest forever if the virtqueue never completes.
const REQUEST_TIMEOUT: TimeSpan = TimeSpan::from_millis(10);

/// Upper bound on the sleeping wait for one entropy request (IRQ-driven
/// path). Far more generous than the polled budget because the CPU is free
/// while waiting; the entropy layer disables the source after repeated
/// timeouts anyway.
const IRQ_REQUEST_TIMEOUT: TimeSpan = TimeSpan::from_millis(50);

/// Cap per request so the owned DMA scratch buffer stays modest.
const MAX_REQUEST_BYTES: usize = 256;

/// Hub deferring virtio-rng completions through the Tasklet softirq vector.
static RNG_COMPLETION_HUB: IrqCompletionHub = IrqCompletionHub::new(SoftirqVec::Tasklet);

fn rng_completion_softirq() {
    RNG_COMPLETION_HUB.run_softirq();
}

/// hardirq-facing view of one virtio-rng device.
struct VirtIoRngIrqDevice<H: Hal, T: Transport> {
    inner: Arc<SpinNoIrq<RngInner<H, T>>>,
    entry: Arc<IrqCompletionEntry>,
}

// SAFETY: the inner device is only reached through its `SpinNoIrq` lock,
// which provides the mutual exclusion required for cross-thread transfer.
unsafe impl<H: Hal, T: Transport> Send for VirtIoRngIrqDevice<H, T> {}
// SAFETY: shared references only reach the device through the same
// `SpinNoIrq` lock, so synchronization is preserved.
unsafe impl<H: Hal, T: Transport> Sync for VirtIoRngIrqDevice<H, T> {}

impl<H: Hal, T: Transport> IrqCompletionDevice for VirtIoRngIrqDevice<H, T> {
    fn ack_interrupt(&self) -> bool {
        self.inner.lock().ack_interrupt_raised()
    }

    fn run_bottom_half(&self) {
        // Single-request device: a broadcast wake plus a used-ring recheck
        // by the requester is equivalent to a per-token dispatch.
        self.entry.wake_source().wake();
    }

    fn entry(&self) -> &Arc<IrqCompletionEntry> {
        &self.entry
    }
}

struct PendingRequest {
    token: u16,
    buf: Box<[u8]>,
}

struct RngInner<H: Hal, T: Transport> {
    transport: T,
    queue: VirtQueue<H, QUEUE_SIZE>,
    /// In-flight request left after a timeout; buffer must stay alive until
    /// the device completes (or the driver is dropped).
    pending: Option<PendingRequest>,
}

impl<H: Hal, T: Transport> RngInner<H, T> {
    fn try_new(mut transport: T) -> VirtIoResult<Self> {
        let feat = transport.begin_init(SUPPORTED_FEATURES);
        let queue = VirtQueue::new(
            &mut transport,
            QUEUE_IDX,
            feat.contains(Feature::RING_INDIRECT_DESC),
            feat.contains(Feature::RING_EVENT_IDX),
            feat.contains(Feature::ACCESS_PLATFORM),
        )?;
        transport.finish_init();
        Ok(Self {
            transport,
            queue,
            pending: None,
        })
    }

    /// Acknowledges the transport interrupt status; reports whether the
    /// device actually raised this IRQ.
    fn ack_interrupt_raised(&mut self) -> bool {
        !self.transport.ack_interrupt().is_empty()
    }

    /// Acknowledges any pending transport interrupt status from task context
    /// (polled fallback only; the IRQ-driven path keeps acknowledgement in
    /// the hardirq handler).
    fn ack_interrupt_from_task(&mut self) {
        let _ = self.transport.ack_interrupt();
    }

    /// Whether the single in-flight request (if any) has completed and its
    /// token reached the used-ring head.
    fn pending_is_complete(&self) -> bool {
        match &self.pending {
            Some(pending) => self.queue.peek_used() == Some(pending.token),
            None => true,
        }
    }

    /// Reclaims a previously timed-out request if the device has completed it.
    ///
    /// Returns `false` when a pending request is still outstanding.
    fn reclaim_pending_if_ready(&mut self) -> bool {
        if self.pending.is_none() || !self.pending_is_complete() {
            return self.pending.is_none();
        }
        self.pop_pending_into(|_| {}).is_ok()
    }

    /// Pops the completed pending request and passes its bytes to `sink`.
    fn pop_pending_into(&mut self, mut sink: impl FnMut(&[u8])) -> VirtIoResult<usize> {
        let Some(pending) = self.pending.as_mut() else {
            return Err(virtio_drivers::Error::NotReady);
        };
        let token = pending.token;
        let mut outputs = [pending.buf.as_mut()];
        // SAFETY: `pending.buf` is the same allocation passed to `add` for
        // `token`, and the used-ring head reports `token` complete.
        let written = unsafe { self.queue.pop_used(token, &[], &mut outputs)? } as usize;
        sink(&pending.buf[..written]);
        self.pending = None;
        Ok(written)
    }

    /// Submits one entropy request and records it as the single in-flight
    /// request.
    fn submit_entropy(&mut self, len: usize) -> DriverResult<()> {
        debug_assert!(self.pending.is_none());
        let mut dma_buf = alloc::vec![0u8; len].into_boxed_slice();
        // SAFETY: `dma_buf` stays valid until `pop_used` in the completion
        // path; it is owned by `self.pending` for the whole in-flight window.
        let token =
            unsafe { self.queue.add(&[], &mut [dma_buf.as_mut()]) }.map_err(as_driver_error)?;
        self.pending = Some(PendingRequest {
            token,
            buf: dma_buf,
        });
        if self.queue.should_notify() {
            self.transport.notify(QUEUE_IDX);
        }
        Ok(())
    }

    /// Legacy bounded busy-wait used when no IRQ handler is registered.
    ///
    /// Unlike the IRQ-driven path, the whole request runs under one
    /// `SpinNoIrq` critical section, so the spin deadline bounds how long
    /// local interrupts stay masked.
    fn request_entropy_timed(&mut self, dst: &mut [u8]) -> DriverResult<usize> {
        if dst.is_empty() {
            return Ok(0);
        }
        if !self.reclaim_pending_if_ready() {
            // Still waiting on a prior timed-out descriptor; do not enqueue more.
            return Err(DriverError::WouldBlock);
        }
        // Without a hardirq handler, task context owns ISR acknowledgement so
        // a shared level-triggered line cannot stay asserted.
        self.ack_interrupt_from_task();

        let len = dst.len().min(MAX_REQUEST_BYTES);
        self.submit_entropy(len)?;

        let deadline = monotonic_time() + REQUEST_TIMEOUT;
        while !self.pending_is_complete() {
            if monotonic_time() >= deadline {
                // The in-flight request stays in `self.pending` so the device
                // may still complete it safely after this IRQ-off section.
                return Err(DriverError::WouldBlock);
            }
            spin_loop();
        }

        let copy_len = self.take_completed(dst)?;
        self.ack_interrupt_from_task();
        Ok(copy_len)
    }

    /// Pops the completed in-flight request and copies its bytes to `dst`.
    fn take_completed(&mut self, dst: &mut [u8]) -> DriverResult<usize> {
        let mut copy_len = 0usize;
        let result = self.pop_pending_into(|bytes| {
            copy_len = bytes.len().min(dst.len());
            dst[..copy_len].copy_from_slice(&bytes[..copy_len]);
        });
        match result {
            Ok(_) => Ok(copy_len),
            Err(err) => Err(as_driver_error(err)),
        }
    }
}

impl<H: Hal, T: Transport> Drop for RngInner<H, T> {
    fn drop(&mut self) {
        self.transport.queue_unset(QUEUE_IDX);
    }
}

/// VirtIO entropy source device driver.
///
/// Exposes the device through [`CharDevice`] so upper layers can collect
/// hardware entropy.
pub struct VirtIoRngDev<H: Hal, T: Transport> {
    device: Arc<SpinNoIrq<RngInner<H, T>>>,
    /// Shared completion entry; `None` keeps the bounded busy-poll path.
    irq_completion: Option<Arc<IrqCompletionEntry>>,
    /// Keeps the hardirq-facing device object registered until drop.
    irq_device: Option<Arc<dyn IrqCompletionDevice>>,
}

// SAFETY: VirtIoRngDev serializes all access to the inner device through
// its own `SpinNoIrq` lock. The inner type is not auto Send due to
// PhantomData, but it is safe to transfer across threads behind that lock.
// The IRQ-driven wait runs with the lock released and local interrupts
// enabled, so it cannot extend an IRQ-off section across the wait.
unsafe impl<H: Hal, T: Transport> Send for VirtIoRngDev<H, T> {}
// SAFETY: shared access to the device is serialized by the IRQ-safe lock
// described above, so shared references remain synchronized.
unsafe impl<H: Hal, T: Transport> Sync for VirtIoRngDev<H, T> {}

impl<H: Hal + 'static, T: Transport + 'static> VirtIoRngDev<H, T> {
    /// Create and initialize a VirtIO RNG device.
    ///
    /// # Arguments
    ///
    /// - `transport` - The VirtIO transport (MMIO or PCI) for this device.
    /// - `irq` - Optional IRQ number. When provided and registration
    ///   succeeds, entropy requests sleep on a Tasklet-softirq completion
    ///   wake instead of busy-polling with local interrupts masked. When
    ///   omitted, the bounded busy-poll fallback is used.
    pub fn try_new(transport: T, irq: Option<usize>) -> DriverResult<Self> {
        let inner = Arc::new(SpinNoIrq::new(
            RngInner::try_new(transport).map_err(as_driver_error)?,
        ));

        let (irq_completion, irq_device) = match irq {
            Some(irq) => {
                let entry = Arc::new(IrqCompletionEntry::new(irq));
                let irq_device: Arc<dyn IrqCompletionDevice> = Arc::new(VirtIoRngIrqDevice {
                    inner: inner.clone(),
                    entry: entry.clone(),
                });
                match RNG_COMPLETION_HUB.register(rng_completion_softirq, irq_device.clone()) {
                    Some(_) => (Some(entry), Some(irq_device)),
                    None => (None, None),
                }
            }
            None => (None, None),
        };

        Ok(Self {
            device: inner,
            irq_completion,
            irq_device,
        })
    }

    fn request_entropy(&self, buf: &mut [u8]) -> DriverResult<usize> {
        match &self.irq_completion {
            Some(entry) => self.request_entropy_irq_driven(entry, buf),
            None => self.device.lock().request_entropy_timed(buf),
        }
    }

    /// Submits one entropy request, sleeps until the Tasklet softirq wake or
    /// the bounded deadline, then collects the result.
    ///
    /// The device lock is only held for submit and completion, never across
    /// the sleep. A concurrent caller observes the single in-flight request
    /// and gets [`DriverError::WouldBlock`], matching the legacy timeout
    /// contract for a busy device. The wake is a broadcast without tokens,
    /// so every wake rechecks the used ring; a late completion of a
    /// previously timed-out request cannot make the next read return early.
    fn request_entropy_irq_driven(
        &self,
        entry: &Arc<IrqCompletionEntry>,
        dst: &mut [u8],
    ) -> DriverResult<usize> {
        if dst.is_empty() {
            return Ok(0);
        }
        {
            let mut inner = self.device.lock();
            if !inner.reclaim_pending_if_ready() {
                return Err(DriverError::WouldBlock);
            }
            let len = dst.len().min(MAX_REQUEST_BYTES);
            inner.submit_entropy(len)?;
        }

        let deadline = monotonic_time() + IRQ_REQUEST_TIMEOUT;
        let is_complete = || self.device.lock().pending_is_complete();
        if !entry.wait_until_deadline(deadline, is_complete) {
            // No sleepable context (early boot or an IRQ-context caller):
            // bounded spin with local interrupts enabled; the registered
            // hardirq handler acknowledges the device ISR, so a shared
            // level-triggered line cannot livelock the CPU.
            while !is_complete() && monotonic_time() < deadline {
                spin_loop();
            }
        }

        let mut inner = self.device.lock();
        if inner.pending_is_complete() {
            inner.take_completed(dst)
        } else {
            // Timed out with the request still in flight; it stays recorded
            // as pending so a later read reclaims it once complete.
            Err(DriverError::WouldBlock)
        }
    }
}

impl<H: Hal, T: Transport> Drop for VirtIoRngDev<H, T> {
    fn drop(&mut self) {
        if let Some(irq_device) = self.irq_device.take() {
            RNG_COMPLETION_HUB.unregister(&irq_device);
        }
    }
}

impl<H: Hal + 'static, T: Transport + 'static> Device for VirtIoRngDev<H, T> {
    fn name(&self) -> &str {
        "virtio-rng"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }
}

impl<H: Hal + 'static, T: Transport + 'static> CharDevice for VirtIoRngDev<H, T> {
    fn read(&self, buf: &mut [u8]) -> DriverResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.request_entropy(buf)
    }

    fn write(&self, _buf: &[u8]) -> DriverResult<usize> {
        Err(DriverError::Unsupported)
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, assert_eq, def_test};
    use virtio_drivers::transport::DeviceType;

    use super::*;
    use crate::{
        irq_completion::test_support::ScopedFailingIrqProvider,
        mock_virtio::{MockHal, MockTransport},
    };

    #[def_test]
    fn test_virtio_rng_init_failure_handling() {
        let transport = MockTransport::new_with_type(DeviceType::EntropySource);
        let dev = VirtIoRngDev::<MockHal, MockTransport>::try_new(transport, None);

        if let Ok(d) = dev {
            assert_eq!(d.name(), "virtio-rng");
            assert_eq!(d.device_kind(), DeviceKind::Char);
        } else {
            assert!(dev.is_err());
        }
    }

    #[def_test(serial)]
    fn test_irq_registration_failure_falls_back_to_polling() {
        // With a provider whose line requests always fail, IRQ registration
        // must roll back and the device must keep the polling path.
        let _failing_provider = ScopedFailingIrqProvider::new();
        let transport = MockTransport::new_with_type(DeviceType::EntropySource);
        let dev = VirtIoRngDev::<MockHal, MockTransport>::try_new(transport, Some(66))
            .expect("device init succeeds with a mock transport");

        assert!(dev.irq_completion.is_none());
        assert!(dev.irq_device.is_none());
    }

    #[def_test]
    fn test_virtio_rng_concurrency_traits() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<VirtIoRngDev<MockHal, MockTransport>>();
    }

    #[def_test]
    fn test_request_timeout_constants_are_bounded() {
        // Guard against accidentally restoring an unbounded wait.
        assert!(REQUEST_TIMEOUT > TimeSpan::from_millis(0));
        assert!(REQUEST_TIMEOUT <= TimeSpan::from_secs(1));
        assert!(IRQ_REQUEST_TIMEOUT > TimeSpan::from_millis(0));
        assert!(IRQ_REQUEST_TIMEOUT <= TimeSpan::from_secs(1));
    }
}
