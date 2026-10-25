// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Interrupt-driven VirtIO RNG adapter.

use alloc::{boxed::Box, sync::Arc, vec};
use core::{mem::ManuallyDrop, ops::Range};

use device_res::{Irq, IrqEvent, IrqHandler, IrqOp, IrqResource, ResError};
use driver_base::{Device, DeviceKind, DriverError, DriverResult};
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

/// Map the driver-internal error vocabulary onto the kernel error set the
/// hwrng class consumes (`subsys-hwrng` speaks `KError`; the device keeps its
/// `DriverError` fault state for `is_failed`).
fn map_driver_error(error: DriverError) -> KError {
    match error {
        DriverError::InvalidInput => KError::InvalidInput,
        DriverError::NoMemory => KError::NoMemory,
        DriverError::Io => KError::Io,
        DriverError::ResourceBusy => KError::ResourceBusy,
        DriverError::BadState => KError::BadState,
        DriverError::Unsupported => KError::Unsupported,
        _ => KError::Io,
    }
}

fn map_irq_error(error: ResError) -> DriverError {
    match error {
        ResError::InvalidResource => DriverError::InvalidInput,
        ResError::MappingFailed => DriverError::Io,
        ResError::NoMemory => DriverError::NoMemory,
        ResError::Busy => DriverError::ResourceBusy,
        ResError::Unsupported => DriverError::Unsupported,
    }
}

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
    resources: Option<ManuallyDrop<RequestResources<H, T>>>,
    state: RequestState,
    fault: Option<DriverError>,
    is_closed: bool,
}

// SAFETY: H supplies kernel-global mappings through dma_alloc and
// mmio_phys_to_virt: queue/DMA pointers remain valid across threads and CPUs,
// rather than being private to the creating thread or CPU. The queue, its state,
// and the owned Box<[u8]> move together without relocating their backing storage.
// H: Send and T: Send also require the HAL and transport themselves to be
// transferable; moving the request therefore preserves its pointer validity.
unsafe impl<H: Hal + Send, T: Transport + Send> Send for RngRequest<H, T> {}

impl<H: Hal, T: Transport> RngRequest<H, T> {
    fn new(transport: T) -> DriverResult<Self> {
        let buffer = vec![0; MAX_REQUEST_BYTES].into_boxed_slice();
        let device = VirtIORng::new(transport).map_err(as_driver_error)?;
        Ok(Self {
            resources: Some(ManuallyDrop::new(RequestResources { device, buffer })),
            state: RequestState::Idle,
            fault: None,
            is_closed: false,
        })
    }

    /// Only the waiting-read path submits; cached or in-flight data is preserved.
    fn submit(&mut self) -> DriverResult {
        if self.is_closed {
            return Err(DriverError::BadState);
        }
        if let Some(error) = self.fault {
            return Err(error);
        }
        // Cached bytes share the DMA buffer; do not resubmit until they are fully consumed.
        if !matches!(self.state, RequestState::Idle) {
            return Ok(());
        }
        let resources = self.resources.as_mut().ok_or(DriverError::BadState)?;
        let RequestResources { device, buffer } = &mut **resources;
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
    fn complete(&mut self) -> DriverResult<bool> {
        let RequestState::Pending(token) = self.state else {
            return Ok(false);
        };
        let resources = self.resources.as_mut().ok_or(DriverError::BadState)?;
        let RequestResources { device, buffer } = &mut **resources;
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
        if self.is_closed {
            return 0;
        }
        if let Some(_error) = self.fault {
            return 0;
        }
        let RequestState::Readable(remaining) = &mut self.state else {
            return 0;
        };

        let Some(resources) = self.resources.as_ref() else {
            return 0;
        };
        let count = dst.len().min(remaining.len());
        dst[..count].copy_from_slice(&resources.buffer[remaining.start..remaining.start + count]);
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
        if let Some(resources) = self.resources.as_mut() {
            // SAFETY: No pending submission remains, and this is the sole resource owner.
            unsafe { ManuallyDrop::drop(resources) };
        }
    }
}

/// IRQ-driven hardware entropy source using a driver-owned DMA buffer.
///
/// While the published instance exists, it owns its IRQ registration and mappings.
/// The removal owner closes reads, drains publication, then calls `finish_stop`.
/// Production bus removal/rebinding integration remains separate.
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
    request: SpinNoIrq<RngRequest<H, T>>,
    completion: Arc<dyn RngCompletion>,
    // Drop after `request`, so a normal request cleanup runs before this guard
    // unregisters its completion handler.
    irq_guard: SpinNoIrq<Option<Irq>>,
}

impl<H: Hal + Send + 'static, T: Transport + Send + 'static> VirtIoRngDev<H, T> {
    /// Register completion delivery before allowing reads.
    ///
    /// The returned device owns the IRQ registration. Explicit teardown uses
    /// `close_reads`, publication drain, then `finish_stop`; dropping an active
    /// device is not a substitute for that sequence.
    ///
    /// Requires sleepable context. If IRQ registration fails after queue
    /// creation, closes the unpublished device and confirms reset before
    /// freeing its DMA resources. `wait_reset` has the same contract as
    /// `finish_stop` and is used only for that rollback.
    pub fn try_new_irq(
        transport: T,
        completion: Arc<dyn RngCompletion>,
        provider: &'static dyn IrqOp,
        resource: IrqResource,
        wait_reset: impl FnMut(),
    ) -> DriverResult<Arc<Self>> {
        let device = Arc::new(Self::new(transport, completion)?);
        let handler: Arc<dyn IrqHandler> = device.clone();
        let weak = Arc::downgrade(&handler);
        let irq = match Irq::request_with(
            provider,
            resource,
            Arc::new(move |number| {
                weak.upgrade()
                    .map_or(IrqEvent::NOT_HANDLED, |device| device.handle(number))
            }),
        ) {
            Ok(irq) => irq,
            Err(error) => {
                device.close_reads();
                device.finish_stop(wait_reset)?;
                return Err(map_irq_error(error));
            }
        };
        *device.irq_guard.lock() = Some(irq);
        if let Some(resources) = device.request.lock().resources.as_mut() {
            resources.device.enable_interrupts();
        }
        Ok(device)
    }
}

impl<H: Hal, T: Transport> VirtIoRngDev<H, T> {
    /// Permanently closes read admission and wakes blocked readers.
    ///
    /// Call before unregistering the hwrng publication so admitted readers can
    /// return `BadState` and drain. Repeated calls are harmless. Empty reads
    /// remain no-ops; closure is not reported as a hardware fault.
    ///
    /// This only closes software access: it neither resets hardware nor
    /// unregisters/synchronizes IRQ handlers. Pending DMA, transport and IRQ
    /// resources remain owned. A handler already past the gate may still be
    /// finishing its notification tail. The removal owner must serialize the
    /// remaining teardown and retain this device until it completes.
    pub fn close_reads(&self) {
        let should_notify = {
            let mut request = self.request.lock();
            if request.is_closed {
                false
            } else {
                request.is_closed = true;
                true
            }
        };
        if should_notify {
            self.completion.notify();
        }
    }

    /// Releases hardware resources after read closure and publication drain.
    ///
    /// The removal owner must serialize this operation and first call
    /// `close_reads`, then unregister/drain its hwrng publication. Requires
    /// sleepable task context without locks needed by readers or IRQ handlers.
    /// Serial repeats after successful completion are no-ops.
    ///
    /// Takes request resources out of the lock, initiates reset and polls the
    /// transport's completion contract. `wait_reset` supplies the host's
    /// sleep/yield interval between unsuccessful polls; it must return normally
    /// with the same sleepable context and must not reenter removal. There is
    /// no timeout permitting DMA release. IRQ release synchronizes the callback
    /// including its notification tail before pending cancellation and unmap.
    ///
    /// Returns BadState without changing resources if reads are still open.
    /// A cancellation error indicates a broken private token invariant; the
    /// closed request retains its resources for diagnosis rather than freeing
    /// an unresolved mapping. Retained references cannot resume hardware I/O.
    pub fn finish_stop(&self, mut wait_reset: impl FnMut()) -> DriverResult {
        let (mut resources, state) = {
            let mut request = self.request.lock();
            if !request.is_closed {
                return Err(DriverError::BadState);
            }
            let Some(resources) = request.resources.take() else {
                return Ok(());
            };
            let state = core::mem::replace(&mut request.state, RequestState::Idle);
            (resources, state)
        };
        resources.device.begin_stop();
        while !resources.device.poll_stopped() {
            wait_reset();
        }
        let irq = self.irq_guard.lock().take();
        drop(irq);
        if let RequestState::Pending(token) = state {
            let RequestResources { device, buffer } = &mut *resources;
            // SAFETY: Reset is confirmed, IRQ release has drained callbacks, and
            // the closed request exclusively owns this original token and buffer.
            if let Err(error) = unsafe { device.cancel_request_entropy(token, buffer) } {
                let mut request = self.request.lock();
                request.state = state;
                request.resources = Some(resources);
                return Err(as_driver_error(error));
            }
        }
        // SAFETY: Confirmed reset and synchronized IRQ release end hardware and
        // callback access; any pending mapping was canceled using its original buffer.
        unsafe { ManuallyDrop::drop(&mut resources) };
        Ok(())
    }

    // Only try_new_irq publishes this object; tests drive its IRQ explicitly.
    fn new(transport: T, completion: Arc<dyn RngCompletion>) -> DriverResult<Self> {
        Ok(Self {
            request: SpinNoIrq::new(RngRequest::new(transport)?),
            completion,
            irq_guard: SpinNoIrq::new(None),
        })
    }
}

impl<H: Hal + Send, T: Transport + Send> Device for VirtIoRngDev<H, T> {
    fn name(&self) -> &str {
        "virtio-rng"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Hwrng
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
            if request.is_closed {
                result = Err(DriverError::BadState);
                return true;
            }
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
        result.map_err(map_driver_error)
    }
}

impl<H: Hal + Send, T: Transport + Send> IrqHandler for VirtIoRngDev<H, T> {
    fn handle(&self, _irq: usize) -> IrqEvent {
        let mut request = self.request.lock();
        if request.is_closed {
            return IrqEvent::NOT_HANDLED;
        }
        let Some(resources) = request.resources.as_mut() else {
            return IrqEvent::NOT_HANDLED;
        };
        let status = resources.device.ack_interrupt();
        if status.is_empty() {
            return IrqEvent::NOT_HANDLED;
        }
        if !status.contains(InterruptStatus::QUEUE_INTERRUPT) {
            return IrqEvent::HANDLED;
        }
        let completed = request.complete();
        let has_fault = request.fault.is_some();
        drop(request);
        if has_fault || completed == Ok(true) {
            self.completion.notify();
        }
        IrqEvent::HANDLED
    }
}

unittest::path_mod_test!(rng_unittest, "../ut-tests/rng_unittest.rs");
