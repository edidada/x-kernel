//! Driver for VirtIO random number generator devices.
use super::common::Feature;
use crate::{
    DmaBackend, Error, Hal, Result, StaticHal,
    queue::VirtQueue,
    transport::{InterruptStatus, Transport},
};

// VirtioRNG only uses one queue
const QUEUE_IDX: u16 = 0;
const QUEUE_SIZE: usize = 8;
const SUPPORTED_FEATURES: Feature = Feature::RING_INDIRECT_DESC
    .union(Feature::RING_EVENT_IDX)
    .union(Feature::VERSION_1)
    .union(Feature::ACCESS_PLATFORM);

/// Driver for a VirtIO random number generator device.
pub struct VirtIORng<H: Hal, T: Transport, B: DmaBackend = StaticHal<H>> {
    transport: T,
    queue: VirtQueue<H, QUEUE_SIZE, B>,
    is_closed: bool,
    is_reset_confirmed: bool,
}

impl<H: Hal, T: Transport> VirtIORng<H, T> {
    /// Create a new driver with the given transport.
    pub fn new(transport: T) -> Result<Self> {
        Self::new_with_dma(transport, StaticHal::default())
    }
}

impl<H: Hal, T: Transport, B: DmaBackend> VirtIORng<H, T, B> {
    /// Creates an RNG using one device backend for rings and request mappings.
    pub fn new_with_dma(mut transport: T, backend: B) -> Result<Self> {
        let feat = transport.begin_init(SUPPORTED_FEATURES);
        let queue = VirtQueue::new_with_dma(
            &mut transport,
            QUEUE_IDX,
            feat.contains(Feature::RING_INDIRECT_DESC),
            feat.contains(Feature::RING_EVENT_IDX),
            feat.contains(Feature::ACCESS_PLATFORM),
            backend,
        )?;
        transport.finish_init();
        Ok(Self {
            transport,
            queue,
            is_closed: false,
            is_reset_confirmed: false,
        })
    }

    /// Request random bytes from the device to be stored into `dst`.
    pub fn request_entropy(&mut self, dst: &mut [u8]) -> Result<usize> {
        if self.is_closed {
            return Err(Error::NotReady);
        }
        let num = self
            .queue
            .add_notify_wait_pop(&[], &mut [dst], &mut self.transport)?;
        Ok(num as usize)
    }

    /// Submit a buffer and return its token without waiting for device completion.
    ///
    /// Returns [`Error::InvalidParam`] for an empty buffer and propagates queue
    /// submission errors. A successful submission does not make the data readable.
    ///
    /// # Safety
    ///
    /// Keep `dst` allocated at the same address and length, without CPU access,
    /// until [`Self::complete_request_entropy`] or [`Self::cancel_request_entropy`]
    /// successfully reclaims the request.
    /// Do not drop this driver or release the buffer while the device can access
    /// it. Cancelling a caller's wait does not discharge these obligations.
    pub unsafe fn request_entropy_nb(&mut self, dst: &mut [u8]) -> Result<u16> {
        if self.is_closed {
            return Err(Error::NotReady);
        }
        if dst.is_empty() {
            return Err(Error::InvalidParam);
        }
        // SAFETY: the caller retains exclusive device access to this same buffer
        // until the returned token is reclaimed with complete_request_entropy.
        let token = unsafe { self.queue.add(&[], &mut [dst]) }?;
        if self.queue.should_notify() {
            self.transport.notify(QUEUE_IDX);
        }
        Ok(token)
    }

    /// Reclaim a submitted request and return the device-reported byte count.
    ///
    /// This does not wait. [`Error::NotReady`] and [`Error::WrongToken`] leave the
    /// request outstanding; retain its buffer for a later completion attempt.
    /// On success, DMA has been unshared and CPU access may resume. The caller
    /// must validate the reported length against the submitted buffer capacity.
    /// Execution-context requirements include those of [`Hal::unshare`].
    ///
    /// # Safety
    ///
    /// While running, `token` and `dst` must identify the same outstanding submission made by
    /// [`Self::request_entropy_nb`], with the original address and length. Do not
    /// reclaim a successfully completed token again or concurrently access `dst`.
    /// Once stopping has begun, this method returns `NotReady` without accessing `dst`.
    pub unsafe fn complete_request_entropy(&mut self, token: u16, dst: &mut [u8]) -> Result<usize> {
        if self.is_closed {
            return Err(Error::NotReady);
        }
        // SAFETY: the caller provides the original output buffer for this live
        // token; pop_used only unshares it after observing a matching completion.
        let num = unsafe { self.queue.pop_used(token, &[], &mut [dst]) }?;
        Ok(num as usize)
    }

    /// Enable interrupts.
    pub fn enable_interrupts(&mut self) {
        if !self.is_closed {
            self.queue.set_dev_notify(true);
        }
    }

    /// Disable interrupts.
    pub fn disable_interrupts(&mut self) {
        if !self.is_closed {
            self.queue.set_dev_notify(false);
        }
    }

    /// Acknowledge interrupt.
    pub fn ack_interrupt(&mut self) -> InterruptStatus {
        if !self.is_closed {
            self.transport.ack_interrupt()
        } else {
            InterruptStatus::empty()
        }
    }

    /// Closes submission/completion and requests a device reset, without waiting.
    ///
    /// Repeated calls do nothing. The owner must retain the transport, rings,
    /// request buffers and DMA backend until `poll_stopped` confirms reset.
    /// This does not wake readers or synchronize host IRQ handlers; the OS
    /// adapter owns those operations and must not hold a request lock while waiting.
    pub fn begin_stop(&mut self) {
        if !self.is_closed {
            self.is_closed = true;
            self.transport.begin_reset();
        }
    }

    /// Checks reset completion through the transport's device-quiescence contract.
    ///
    /// Returns false before `begin_stop` or while the device has not acknowledged
    /// reset. The caller chooses how to wait and must not free resources on a
    /// timeout. A completed stop is permanent; this object cannot be restarted.
    pub fn poll_stopped(&mut self) -> bool {
        if self.is_closed && !self.is_reset_confirmed {
            self.is_reset_confirmed = self.transport.poll_reset();
        }
        self.is_reset_confirmed
    }

    /// Reclaims a pending request after confirmed reset, without returning entropy.
    ///
    /// Returns `NotReady` before reset confirmation, or `WrongToken` for an
    /// unoccupied or out-of-range descriptor slot. Tokens can be reused while
    /// running: this is not a stale-token detector. Copy-back performed by the DMA backend
    /// does not make canceled bytes valid entropy. There is no used-ring check.
    ///
    /// # Safety
    ///
    /// If the slot is occupied, `token` and `dst` must identify its CURRENT
    /// outstanding submission with the original buffer address and length.
    /// A token from an older, completed submission must not be used after that
    /// slot has been reused. Unoccupied/out-of-range slots are rejected without
    /// accessing the buffer; after stop, slots cannot be reused, so repeating a
    /// successful cancellation is harmless. Host IRQ handlers and all
    /// other users of these resources must be quiesced before reclamation.
    /// The transport must implement the VirtIO reset-completion guarantee;
    /// software interrupt synchronization alone is not a device stop guarantee.
    pub unsafe fn cancel_request_entropy(&mut self, token: u16, dst: &mut [u8]) -> Result<()> {
        if !self.is_reset_confirmed {
            return Err(Error::NotReady);
        }
        // SAFETY: Confirmed reset ends device access; the caller supplies the
        // original buffer and has quiesced software users before this reclamation.
        unsafe { self.queue.reclaim_after_reset(token, &[], &mut [dst]) }
    }
}

impl<H: Hal, T: Transport, B: DmaBackend> Drop for VirtIORng<H, T, B> {
    fn drop(&mut self) {
        if !self.is_reset_confirmed {
            self.transport.queue_unset(QUEUE_IDX);
        }
    }
}
