// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO network driver adapter.
use alloc::{collections::BTreeSet, sync::Arc, vec::Vec};
use core::{
    num::NonZeroU16,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use device_res::{Irq, IrqEvent, IrqEventSource, IrqOp, IrqResource, IrqTrigger};
use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use driver_net::{
    MacAddress, NetBuf, NetBufBox, NetBufHandle, NetBufPool, NetDevice, NetRxScheduler,
};
use kspin::{SpinNoIrq, SpinNoPreempt};
use virtio_drivers::{Hal, device::net::VirtIONetRaw as InnerDev, transport::Transport};

use crate::as_driver_error;

const NET_BUF_LEN: usize = 1526;
const TSO_TX_BUF_LEN: usize = 16 * 1024;
const VIRTIO_NET_HDR_MAX_LEN: usize = 12;

mod tso_transport;
use tso_transport::{
    TsoTransport, VIRTIO_NET_F_CSUM, VIRTIO_NET_F_HOST_ECN, VIRTIO_NET_F_HOST_TSO4,
};

static NEXT_IRQ_HANDLE_ID: AtomicUsize = AtomicUsize::new(0);

trait VirtIoNetIrqAck: Send + Sync {
    fn ack_interrupt(&self) -> bool;
    fn handle_id(&self) -> usize;
    fn irq(&self) -> usize;
    fn schedule_rx(&self);
}

struct VirtIoNetIrqHandle<H: Hal, T: Transport, const QS: usize> {
    inner: Arc<SpinNoIrq<InnerDev<H, TsoTransport<T>, QS>>>,
    rx_scheduler: Arc<SpinNoIrq<Option<Arc<dyn NetRxScheduler>>>>,
    id: usize,
    irq: usize,
}

// SAFETY: VirtIoNetIrqHandle only accesses the device through SpinNoIrq,
// which provides mutual exclusion. The inner InnerDev is safe to share
// across threads when protected by a lock.
unsafe impl<H: Hal, T: Transport, const QS: usize> Send for VirtIoNetIrqHandle<H, T, QS> {}
// SAFETY: VirtIoNetIrqHandle only accesses the device through SpinNoIrq,
// which provides mutual exclusion for shared references as well.
unsafe impl<H: Hal, T: Transport, const QS: usize> Sync for VirtIoNetIrqHandle<H, T, QS> {}

impl<H: Hal, T: Transport, const QS: usize> VirtIoNetIrqAck for VirtIoNetIrqHandle<H, T, QS> {
    fn ack_interrupt(&self) -> bool {
        !self.inner.lock().ack_interrupt().is_empty()
    }

    fn handle_id(&self) -> usize {
        self.id
    }

    fn irq(&self) -> usize {
        self.irq
    }

    fn schedule_rx(&self) {
        let scheduler = self.rx_scheduler.lock().clone();
        if let Some(scheduler) = scheduler {
            scheduler.schedule_rx();
        }
    }
}

static NET_IRQ_HANDLES: SpinNoIrq<Vec<Arc<dyn VirtIoNetIrqAck>>> = SpinNoIrq::new(Vec::new());
static REGISTERED_NET_IRQS: SpinNoIrq<BTreeSet<usize>> = SpinNoIrq::new(BTreeSet::new());
/// Keeps the network interrupt registrations alive for the lifetime of the
/// kernel; dropping an [`Irq`] guard would release the handler.
static NET_IRQ_GUARDS: SpinNoIrq<Vec<Irq>> = SpinNoIrq::new(Vec::new());

/// Event source bits reported by the virtio-net IRQ handler. The regular
/// handler only acks; these bits are advisory for future multi-queue routing
/// (the rx-task wake path does not read them).
const RX_SRC: IrqEventSource = 0;
const TX_SRC: IrqEventSource = 1;

fn remove_virtio_net_irq_handle(handle_id: usize) {
    NET_IRQ_HANDLES
        .lock()
        .retain(|handle| handle.handle_id() != handle_id);
}

fn handle_virtio_net_irq(irq: usize) -> IrqEvent {
    let handles = NET_IRQ_HANDLES.lock();
    let mut sources = 0;
    for irq_handle in handles.iter() {
        if irq_handle.irq() != irq {
            continue;
        }
        if irq_handle.ack_interrupt() {
            irq_handle.schedule_rx();
            sources |= (1 << RX_SRC) | (1 << TX_SRC);
        }
    }
    if sources == 0 {
        IrqEvent::NOT_HANDLED
    } else {
        IrqEvent::from_sources(sources)
    }
}

fn register_virtio_net_irq<H: Hal + 'static, T: Transport + 'static, const QS: usize>(
    irq_provider: &'static dyn IrqOp,
    irq: usize,
    inner: &Arc<SpinNoIrq<InnerDev<H, TsoTransport<T>, QS>>>,
    rx_scheduler: &Arc<SpinNoIrq<Option<Arc<dyn NetRxScheduler>>>>,
) -> DriverResult<usize> {
    let handle_id = NEXT_IRQ_HANDLE_ID.fetch_add(1, Ordering::Relaxed);
    NET_IRQ_HANDLES
        .lock()
        .push(Arc::new(VirtIoNetIrqHandle::<H, T, QS> {
            inner: inner.clone(),
            rx_scheduler: rx_scheduler.clone(),
            id: handle_id,
            irq,
        }));

    if REGISTERED_NET_IRQS.lock().insert(irq) {
        let resource = IrqResource::new(irq, IrqTrigger::Unknown(0));
        match Irq::request_with(irq_provider, resource, Arc::new(handle_virtio_net_irq)) {
            Ok(guard) => NET_IRQ_GUARDS.lock().push(guard),
            Err(_) => {
                remove_virtio_net_irq_handle(handle_id);
                REGISTERED_NET_IRQS.lock().remove(&irq);
                return Err(DriverError::ResourceBusy);
            }
        }
    }

    Ok(handle_id)
}

fn unregister_virtio_net_irq(irq: usize, handle_id: usize) {
    remove_virtio_net_irq_handle(handle_id);

    let irq_still_used = NET_IRQ_HANDLES.lock().iter().any(|h| h.irq() == irq);
    if !irq_still_used {
        REGISTERED_NET_IRQS.lock().remove(&irq);
        let guard = {
            let mut guards = NET_IRQ_GUARDS.lock();
            guards
                .iter()
                .position(|guard| guard.number() == irq)
                .map(|index| guards.swap_remove(index))
        };
        drop(guard);
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
/// let (kind, transport, irq) = virtio::probe_pci_device::<HalImpl, _>(...).unwrap();
/// let mut net = VirtIoNetDev::<HalImpl, _, 256>::try_new(transport, Some(irq), Some(irq_provider))?;
/// let tx_buf = net.alloc_tx_buf(1500)?;
/// // ... fill tx_buf and send ...
/// net.send(tx_buf)?;
/// let rx_buf = net.recv()?;
/// ```
pub struct VirtIoNetDev<H: Hal, T: Transport, const QS: usize> {
    bufs: SpinNoPreempt<NetBufState<QS>>,
    // Kept alive so that `NetBufBox`es in `bufs` remain valid.
    #[allow(dead_code)]
    rx_buf_pool: Arc<NetBufPool>,
    #[allow(dead_code)]
    tx_buf_pool: Arc<NetBufPool>,
    inner: Arc<SpinNoIrq<InnerDev<H, TsoTransport<T>, QS>>>,
    mac: MacAddress,
    tso4: bool,
    host_ecn: bool,
    irq: Option<usize>,
    irq_handle_id: Option<usize>,
    rx_scheduler: Arc<SpinNoIrq<Option<Arc<dyn NetRxScheduler>>>>,
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

impl<H: Hal, T: Transport, const QS: usize> Drop for VirtIoNetDev<H, T, QS> {
    fn drop(&mut self) {
        if let (Some(irq), Some(handle_id)) = (self.irq, self.irq_handle_id) {
            unregister_virtio_net_irq(irq, handle_id);
        }
    }
}

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
    pub fn try_new(
        transport: T,
        irq: Option<usize>,
        irq_provider: Option<&'static dyn IrqOp>,
    ) -> DriverResult<Self> {
        // 0. Create a new driver instance.
        const NONE_BUF: Option<NetBufBox> = None;
        let negotiated_features = Arc::new(AtomicU64::new(0));
        let transport = TsoTransport::new(transport, negotiated_features.clone());
        let inner = Arc::new(SpinNoIrq::new(
            InnerDev::new(transport).map_err(as_driver_error)?,
        ));
        let negotiated_features = negotiated_features.load(Ordering::Relaxed);
        let tso4 = negotiated_features & (VIRTIO_NET_F_CSUM | VIRTIO_NET_F_HOST_TSO4)
            == VIRTIO_NET_F_CSUM | VIRTIO_NET_F_HOST_TSO4;
        let host_ecn = negotiated_features & VIRTIO_NET_F_HOST_ECN != 0;
        let mut rx_buffers = [NONE_BUF; QS];
        let tx_buffers = [NONE_BUF; QS];
        let rx_buf_pool = NetBufPool::new(QS, NET_BUF_LEN)?;
        let tx_buf_pool = NetBufPool::new(QS, if tso4 { TSO_TX_BUF_LEN } else { NET_BUF_LEN })?;
        let mut free_tx_bufs = Vec::with_capacity(QS);

        // 1. Fill all rx buffers.
        for (i, rx_buf_place) in rx_buffers.iter_mut().enumerate() {
            let mut rx_buf = rx_buf_pool.alloc_boxed().ok_or(DriverError::NoMemory)?;
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
            let mut tx_buf = tx_buf_pool.alloc_boxed().ok_or(DriverError::NoMemory)?;
            // Fill header
            let hdr_len = inner
                .lock()
                .fill_buffer_header(tx_buf.buffer_mut())
                .or(Err(DriverError::InvalidInput))?;
            tx_buf.set_hdr_len(hdr_len)?;
            free_tx_bufs.push(tx_buf);
        }

        let mut irq_handle_id = None;
        let rx_scheduler = Arc::new(SpinNoIrq::new(None));
        if let Some(irq) = irq {
            let irq_provider = irq_provider.ok_or(DriverError::InvalidInput)?;
            irq_handle_id = Some(register_virtio_net_irq(
                irq_provider,
                irq,
                &inner,
                &rx_scheduler,
            )?);
        }

        // 3. Cache immutable device properties.
        let mac = MacAddress(inner.lock().mac_address());

        // 4. Return the driver instance.
        Ok(Self {
            bufs: SpinNoPreempt::new(NetBufState {
                rx_buffers,
                tx_buffers,
                free_tx_bufs,
            }),
            rx_buf_pool,
            tx_buf_pool,
            inner,
            mac,
            tso4,
            host_ecn,
            irq,
            irq_handle_id,
            rx_scheduler,
        })
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
        self.irq
    }
}

impl<H: Hal, T: Transport, const QS: usize> NetDevice for VirtIoNetDev<H, T, QS> {
    #[inline]
    fn mac(&self) -> MacAddress {
        self.mac
    }

    fn tcpv4_tso_max_frame_len(&self) -> Option<usize> {
        self.tso4.then_some(TSO_TX_BUF_LEN - VIRTIO_NET_HDR_MAX_LEN)
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
        let tso_segment_size = tx_buf.tso_segment_size();
        // SAFETY: `from_handle` converts a handle back to an owned `NetBuf`.
        // The caller guarantees the handle is valid and not used elsewhere.
        let mut tx_buf = unsafe { NetBuf::from_handle(tx_buf) };
        let header_result = self.inner.lock().fill_buffer_header(tx_buf.buffer_mut());
        let hdr_len = match header_result {
            Ok(hdr_len) => hdr_len,
            Err(err) => {
                self.bufs.lock().free_tx_bufs.push(tx_buf);
                return Err(as_driver_error(err));
            }
        };
        debug_assert_eq!(hdr_len, tx_buf.hdr_len());
        if let Some(segment_size) = tso_segment_size {
            if !self.tso4 {
                self.bufs.lock().free_tx_bufs.push(tx_buf);
                return Err(DriverError::Unsupported);
            }
            let frame_len = tx_buf.payload_len();
            let (header, frame) = tx_buf.buffer_mut().split_at_mut(hdr_len);
            if let Err(err) =
                prepare_tso4_header(header, &mut frame[..frame_len], segment_size, self.host_ecn)
            {
                self.bufs.lock().free_tx_bufs.push(tx_buf);
                return Err(err);
            }
        }
        let mut bufs = self.bufs.lock();
        // SAFETY: `transmit_begin` requires exclusive access to the buffer
        // frame for the duration it is in the VirtIO queue. We store the
        // buffer in `tx_buffers[token]` immediately after, ensuring it
        // remains valid until `recycle_tx()` retrieves it.
        let token = unsafe {
            self.inner
                .lock()
                .transmit_begin(tx_buf.frame())
                .map_err(as_driver_error)
        };
        let token = match token {
            Ok(token) => token,
            Err(err) => {
                bufs.free_tx_bufs.push(tx_buf);
                return Err(err);
            }
        };
        bufs.tx_buffers[token as usize] = Some(tx_buf);
        Ok(())
    }

    fn recv(&self) -> DriverResult<NetBufHandle> {
        if self.irq.is_none() {
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

        if let Err(err) = net_buf.set_payload_len(pkt_len) {
            self.bufs.lock().free_tx_bufs.push(net_buf);
            return Err(err);
        }

        Ok(net_buf.into_handle())
    }

    fn set_rx_scheduler(&self, scheduler: Option<Arc<dyn NetRxScheduler>>) -> DriverResult {
        if scheduler.is_some() && self.irq.is_none() {
            return Err(DriverError::Unsupported);
        }
        *self.rx_scheduler.lock() = scheduler;
        Ok(())
    }
}

fn prepare_tso4_header(
    header: &mut [u8],
    frame: &mut [u8],
    segment_size: NonZeroU16,
    host_ecn: bool,
) -> DriverResult {
    const ETH_LEN: usize = 14;
    const TCP_CHECKSUM_OFFSET: usize = 16;
    if header.len() < 10 || frame.len() < ETH_LEN + 20 + 20 {
        return Err(DriverError::InvalidInput);
    }
    if frame[12..14] != [0x08, 0x00] || frame[ETH_LEN] >> 4 != 4 {
        return Err(DriverError::InvalidInput);
    }
    let ip_len = usize::from(frame[ETH_LEN] & 0x0f) * 4;
    let tcp_start = ETH_LEN + ip_len;
    let ip_packet_len = usize::from(u16::from_be_bytes([frame[ETH_LEN + 2], frame[ETH_LEN + 3]]));
    if ip_len < 20
        || ip_packet_len != frame.len() - ETH_LEN
        || frame.len() < tcp_start + 20
        || frame[ETH_LEN + 9] != 6
        || (u16::from_be_bytes([frame[ETH_LEN + 6], frame[ETH_LEN + 7]]) & 0x3fff) != 0
    {
        return Err(DriverError::InvalidInput);
    }
    let tcp_len = usize::from(frame[tcp_start + 12] >> 4) * 4;
    let header_len = tcp_start + tcp_len;
    if tcp_len < 20 || frame.len() <= header_len + usize::from(segment_size.get()) {
        return Err(DriverError::InvalidInput);
    }
    let is_ecn = frame[ETH_LEN + 1] & 0x03 != 0;
    if is_ecn && !host_ecn {
        return Err(DriverError::Unsupported);
    }

    // The device adds each segment's TCP length to this pseudo-header seed.
    let mut sum = 6u32;
    for word in frame[ETH_LEN + 12..ETH_LEN + 20].chunks_exact(2) {
        sum += u32::from(u16::from_be_bytes([word[0], word[1]]));
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    frame[tcp_start + TCP_CHECKSUM_OFFSET..tcp_start + TCP_CHECKSUM_OFFSET + 2]
        .copy_from_slice(&(sum as u16).to_be_bytes());
    header[0] = 1; // VIRTIO_NET_HDR_F_NEEDS_CSUM
    header[1] = if is_ecn { 0x81 } else { 1 }; // TCPv4, optionally ECN
    header[2..4].copy_from_slice(&(header_len as u16).to_le_bytes());
    header[4..6].copy_from_slice(&segment_size.get().to_le_bytes());
    header[6..8].copy_from_slice(&(tcp_start as u16).to_le_bytes());
    header[8..10].copy_from_slice(&(TCP_CHECKSUM_OFFSET as u16).to_le_bytes());
    Ok(())
}

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;

    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::mock_virtio::{MockHal, MockTransport};

    #[def_test]
    fn tso4_header_carries_segment_size_and_partial_checksum() {
        let mut virtio_header = [0u8; 12];
        let mut frame = [0u8; 14 + 20 + 20 + 3000];
        frame[12..14].copy_from_slice(&[0x08, 0x00]);
        frame[14] = 0x45;
        frame[14 + 2..14 + 4].copy_from_slice(&(20 + 20 + 3000u16).to_be_bytes());
        frame[14 + 9] = 6;
        frame[14 + 12..14 + 20].copy_from_slice(&[192, 0, 2, 1, 198, 51, 100, 2]);
        frame[14 + 20 + 12] = 0x50;

        prepare_tso4_header(
            &mut virtio_header,
            &mut frame,
            NonZeroU16::new(1460).unwrap(),
            false,
        )
        .unwrap();

        assert_eq!(&virtio_header[..10], &[1, 1, 54, 0, 180, 5, 34, 0, 16, 0]);
        assert_eq!(&frame[14 + 20 + 16..14 + 20 + 18], &[0xec, 0x3d]);
    }

    #[def_test]
    fn tso4_rejects_ecn_without_negotiation() {
        let mut virtio_header = [0u8; 12];
        let mut frame = [0u8; 14 + 20 + 20 + 3000];
        frame[12..14].copy_from_slice(&[0x08, 0x00]);
        frame[14] = 0x45;
        frame[14 + 2..14 + 4].copy_from_slice(&(20 + 20 + 3000u16).to_be_bytes());
        frame[14 + 1] = 0x02;
        frame[14 + 9] = 6;
        frame[14 + 20 + 12] = 0x50;

        assert_eq!(
            prepare_tso4_header(
                &mut virtio_header,
                &mut frame,
                NonZeroU16::new(1460).unwrap(),
                false,
            ),
            Err(DriverError::Unsupported)
        );
    }

    #[def_test]
    fn tso4_rejects_mismatched_ipv4_length() {
        let mut virtio_header = [0u8; 12];
        let mut frame = [0u8; 14 + 20 + 20 + 3000];
        frame[12..14].copy_from_slice(&[0x08, 0x00]);
        frame[14] = 0x45;
        frame[14 + 2..14 + 4].copy_from_slice(&(20 + 20 + 2999u16).to_be_bytes());
        frame[14 + 9] = 6;
        frame[14 + 20 + 12] = 0x50;

        assert_eq!(
            prepare_tso4_header(
                &mut virtio_header,
                &mut frame,
                NonZeroU16::new(1460).unwrap(),
                false,
            ),
            Err(DriverError::InvalidInput)
        );
    }

    #[def_test]
    fn tso4_capability_requires_checksum_and_segmentation_features() {
        for features in [
            0,
            VIRTIO_NET_F_CSUM,
            VIRTIO_NET_F_HOST_TSO4,
            VIRTIO_NET_F_CSUM | VIRTIO_NET_F_HOST_TSO4,
        ] {
            let mut transport = MockTransport::new();
            transport.device_type = virtio_drivers::transport::DeviceType::Network;
            transport.features = features;
            let dev =
                VirtIoNetDev::<MockHal, MockTransport, 32>::try_new(transport, None, None).unwrap();

            assert_eq!(
                dev.tcpv4_tso_max_frame_len(),
                (features == VIRTIO_NET_F_CSUM | VIRTIO_NET_F_HOST_TSO4)
                    .then_some(TSO_TX_BUF_LEN - VIRTIO_NET_HDR_MAX_LEN)
            );
        }
    }

    struct MockIrqHandle {
        id: usize,
        irq_num: usize,
        handled: bool,
        scheduler: Option<Arc<dyn NetRxScheduler>>,
    }

    struct MockRxScheduler {
        schedules: AtomicUsize,
    }

    impl NetRxScheduler for MockRxScheduler {
        fn schedule_rx(&self) {
            self.schedules.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl VirtIoNetIrqAck for MockIrqHandle {
        fn ack_interrupt(&self) -> bool {
            self.handled
        }

        fn handle_id(&self) -> usize {
            self.id
        }

        fn irq(&self) -> usize {
            self.irq_num
        }

        fn schedule_rx(&self) {
            if let Some(scheduler) = &self.scheduler {
                scheduler.schedule_rx();
            }
        }
    }

    fn reset_irq_state() {
        NET_IRQ_HANDLES.lock().clear();
        REGISTERED_NET_IRQS.lock().clear();
    }

    fn mock_handle(id: usize, irq_num: usize) -> Arc<MockIrqHandle> {
        Arc::new(MockIrqHandle {
            id,
            irq_num,
            handled: false,
            scheduler: None,
        })
    }

    fn handled_mock_handle(
        id: usize,
        irq_num: usize,
        scheduler: Option<Arc<dyn NetRxScheduler>>,
    ) -> Arc<MockIrqHandle> {
        Arc::new(MockIrqHandle {
            id,
            irq_num,
            handled: true,
            scheduler,
        })
    }

    #[def_test(serial)]
    fn test_unregister_removes_correct_handle() {
        reset_irq_state();

        NET_IRQ_HANDLES.lock().push(mock_handle(10, 32));
        NET_IRQ_HANDLES.lock().push(mock_handle(11, 33));
        NET_IRQ_HANDLES.lock().push(mock_handle(12, 34));
        REGISTERED_NET_IRQS.lock().insert(32);
        REGISTERED_NET_IRQS.lock().insert(33);
        REGISTERED_NET_IRQS.lock().insert(34);

        unregister_virtio_net_irq(33, 11);

        {
            let handles = NET_IRQ_HANDLES.lock();
            assert_eq!(handles.len(), 2);
            assert_eq!(handles[0].handle_id(), 10);
            assert_eq!(handles[1].handle_id(), 12);
        }
        assert!(!REGISTERED_NET_IRQS.lock().contains(&33));
        assert!(REGISTERED_NET_IRQS.lock().contains(&32));
        assert!(REGISTERED_NET_IRQS.lock().contains(&34));

        reset_irq_state();
    }

    #[def_test(serial)]
    fn test_shared_irq_preserved_when_handles_remain() {
        reset_irq_state();

        NET_IRQ_HANDLES.lock().push(mock_handle(20, 40));
        NET_IRQ_HANDLES.lock().push(mock_handle(21, 40));
        REGISTERED_NET_IRQS.lock().insert(40);

        unregister_virtio_net_irq(40, 20);

        {
            let handles = NET_IRQ_HANDLES.lock();
            assert_eq!(handles.len(), 1);
            assert_eq!(handles[0].handle_id(), 21);
        }
        assert!(REGISTERED_NET_IRQS.lock().contains(&40));

        reset_irq_state();
    }

    #[def_test(serial)]
    fn test_shared_irq_unregistered_when_all_handles_removed() {
        reset_irq_state();

        NET_IRQ_HANDLES.lock().push(mock_handle(30, 50));
        NET_IRQ_HANDLES.lock().push(mock_handle(31, 50));
        REGISTERED_NET_IRQS.lock().insert(50);

        unregister_virtio_net_irq(50, 30);
        assert!(REGISTERED_NET_IRQS.lock().contains(&50));

        unregister_virtio_net_irq(50, 31);
        assert!(!REGISTERED_NET_IRQS.lock().contains(&50));
        assert!(NET_IRQ_HANDLES.lock().is_empty());

        reset_irq_state();
    }

    #[def_test(serial)]
    fn test_unregister_nonexistent_handle_is_noop() {
        reset_irq_state();

        NET_IRQ_HANDLES.lock().push(mock_handle(40, 60));
        REGISTERED_NET_IRQS.lock().insert(60);

        unregister_virtio_net_irq(60, 9999);

        assert_eq!(NET_IRQ_HANDLES.lock().len(), 1);
        assert!(REGISTERED_NET_IRQS.lock().contains(&60));

        reset_irq_state();
    }

    #[def_test(serial)]
    fn test_failed_try_new_does_not_leak_irq_handle() {
        reset_irq_state();

        let mut transport = MockTransport::new();
        transport.device_type = virtio_drivers::transport::DeviceType::Network;
        let _dev = VirtIoNetDev::<MockHal, MockTransport, 32>::try_new(transport, None, None);

        assert!(NET_IRQ_HANDLES.lock().is_empty());

        reset_irq_state();
    }

    #[def_test(serial)]
    fn test_rx_scheduler_requires_registered_irq_but_allows_detach() {
        reset_irq_state();

        let mut transport = MockTransport::new();
        transport.device_type = virtio_drivers::transport::DeviceType::Network;
        let dev =
            VirtIoNetDev::<MockHal, MockTransport, 32>::try_new(transport, None, None).unwrap();
        let scheduler = Arc::new(MockRxScheduler {
            schedules: AtomicUsize::new(0),
        });

        assert_eq!(
            dev.set_rx_scheduler(Some(scheduler)),
            Err(DriverError::Unsupported)
        );
        assert_eq!(dev.set_rx_scheduler(None), Ok(()));

        reset_irq_state();
    }

    #[def_test]
    fn test_recv_acknowledges_interrupt_in_poll_mode() {
        let mut transport = MockTransport::new();
        transport.device_type = virtio_drivers::transport::DeviceType::Network;
        let interrupt_ack_count = transport.interrupt_ack_count.clone();
        let dev =
            VirtIoNetDev::<MockHal, MockTransport, 32>::try_new(transport, None, None).unwrap();

        assert!(matches!(dev.recv(), Err(DriverError::WouldBlock)));
        assert_eq!(interrupt_ack_count.load(Ordering::Relaxed), 1);
    }

    #[def_test(serial)]
    fn test_recv_leaves_interrupt_ack_to_registered_handler() {
        reset_irq_state();

        let mut transport = MockTransport::new();
        transport.device_type = virtio_drivers::transport::DeviceType::Network;
        let interrupt_ack_count = transport.interrupt_ack_count.clone();
        let mut dev =
            VirtIoNetDev::<MockHal, MockTransport, 32>::try_new(transport, None, None).unwrap();
        let irq = 73;
        let handle_id = 53;
        NET_IRQ_HANDLES
            .lock()
            .push(Arc::new(VirtIoNetIrqHandle::<MockHal, MockTransport, 32> {
                inner: dev.inner.clone(),
                rx_scheduler: dev.rx_scheduler.clone(),
                id: handle_id,
                irq,
            }));
        REGISTERED_NET_IRQS.lock().insert(irq);
        dev.irq = Some(irq);
        dev.irq_handle_id = Some(handle_id);

        assert!(matches!(dev.recv(), Err(DriverError::WouldBlock)));
        assert_eq!(interrupt_ack_count.load(Ordering::Relaxed), 0);

        let _ = handle_virtio_net_irq(irq);
        assert_eq!(interrupt_ack_count.load(Ordering::Relaxed), 1);

        drop(dev);
        reset_irq_state();
    }

    #[def_test(serial)]
    fn test_irq_handler_filters_irq_and_schedules_rx() {
        reset_irq_state();

        let scheduler = Arc::new(MockRxScheduler {
            schedules: AtomicUsize::new(0),
        });
        NET_IRQ_HANDLES
            .lock()
            .push(handled_mock_handle(50, 70, None));
        NET_IRQ_HANDLES
            .lock()
            .push(handled_mock_handle(51, 71, Some(scheduler.clone())));

        let event = handle_virtio_net_irq(71);

        assert!(event.has_source(RX_SRC));
        assert!(event.has_source(TX_SRC));
        assert_eq!(scheduler.schedules.load(Ordering::Relaxed), 1);

        reset_irq_state();
    }

    #[def_test(serial)]
    fn test_irq_handler_treats_matched_empty_ack_as_miss() {
        reset_irq_state();

        let scheduler = Arc::new(MockRxScheduler {
            schedules: AtomicUsize::new(0),
        });
        NET_IRQ_HANDLES.lock().push(Arc::new(MockIrqHandle {
            id: 52,
            irq_num: 72,
            handled: false,
            scheduler: Some(scheduler.clone()),
        }));

        let event = handle_virtio_net_irq(72);

        assert!(!event.handled());
        assert!(!event.has_source(RX_SRC));
        assert_eq!(scheduler.schedules.load(Ordering::Relaxed), 0);

        reset_irq_state();
    }

    #[def_test(serial)]
    fn test_remove_irq_handle_uses_id_not_stack_order() {
        reset_irq_state();

        NET_IRQ_HANDLES.lock().push(mock_handle(60, 80));
        NET_IRQ_HANDLES.lock().push(mock_handle(61, 81));
        NET_IRQ_HANDLES.lock().push(mock_handle(62, 82));

        remove_virtio_net_irq_handle(61);

        let handles = NET_IRQ_HANDLES.lock();
        assert_eq!(handles.len(), 2);
        assert_eq!(handles[0].handle_id(), 60);
        assert_eq!(handles[1].handle_id(), 62);
        drop(handles);

        reset_irq_state();
    }

    #[def_test]
    fn test_virtio_net_concurrency_traits() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<VirtIoNetDev<MockHal, MockTransport, 32>>();
    }
}
