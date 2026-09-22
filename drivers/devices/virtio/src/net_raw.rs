// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO-net queues owned by this kernel.
//!
//! `kvirtiodrivers::device::net::VirtIONetRaw::receive_begin` notifies the
//! device after every refilled buffer. The RX refill path here uses the
//! public [`VirtQueue`] instead, and checks notification requests after each
//! publication while deferring the kick until a batch or a drain boundary.

use core::{
    mem::size_of,
    sync::atomic::{Ordering, fence},
};

use log::warn;
use virtio_drivers::{
    Error, Hal, Result,
    device::net::VirtioNetHdr,
    queue::VirtQueue,
    transport::{InterruptStatus, Transport},
};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

const QUEUE_RECEIVE: u16 = 0;
const QUEUE_TRANSMIT: u16 = 1;
const MIN_BUFFER_LEN: usize = 1526;
const RX_REFILL_BATCH: u16 = 16;

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
    struct Features: u64 {
        const MAC = 1 << 5;
        const STATUS = 1 << 16;
        const MRG_RXBUF = 1 << 15;
        const RING_INDIRECT_DESC = 1 << 28;
        const RING_EVENT_IDX = 1 << 29;
        const VERSION_1 = 1 << 32;
        const ACCESS_PLATFORM = 1 << 33;
    }
}

const SUPPORTED_FEATURES: Features = Features::MAC
    .union(Features::STATUS)
    .union(Features::RING_EVENT_IDX)
    .union(Features::RING_INDIRECT_DESC)
    .union(Features::VERSION_1)
    .union(Features::ACCESS_PLATFORM);

#[derive(Copy, Clone, Debug, Eq, FromBytes, Immutable, IntoBytes, KnownLayout, PartialEq)]
#[repr(C)]
struct MacAddress([u8; 6]);

/// Legacy virtio-net header, without `num_buffers`.
#[derive(
    Copy, Clone, Debug, Default, Eq, FromBytes, Immutable, IntoBytes, KnownLayout, PartialEq,
)]
#[repr(C)]
struct VirtioNetHdrLegacy {
    flags: u8,
    gso_type: u8,
    hdr_len: u16,
    gso_size: u16,
    csum_start: u16,
    csum_offset: u16,
}

pub(super) struct RawNet<H: Hal, T: Transport, const QS: usize> {
    transport: T,
    mac: [u8; 6],
    recv_queue: VirtQueue<H, QS>,
    send_queue: VirtQueue<H, QS>,
    legacy_header: bool,
    /// Refills since the last receive-queue flush.
    rx_since_notify: u16,
    /// A notification requested by any publication in the current batch.
    needs_rx_notify: bool,
}

impl<H: Hal, T: Transport, const QS: usize> RawNet<H, T, QS> {
    pub(super) fn new(mut transport: T) -> Result<Self> {
        let negotiated_features = transport.begin_init(SUPPORTED_FEATURES);
        let mac = transport.read_consistent(|| transport.read_config_space::<MacAddress>(0))?;

        let send_queue = VirtQueue::new(
            &mut transport,
            QUEUE_TRANSMIT,
            negotiated_features.contains(Features::RING_INDIRECT_DESC),
            negotiated_features.contains(Features::RING_EVENT_IDX),
            negotiated_features.contains(Features::ACCESS_PLATFORM),
        )?;
        let recv_queue = VirtQueue::new(
            &mut transport,
            QUEUE_RECEIVE,
            negotiated_features.contains(Features::RING_INDIRECT_DESC),
            negotiated_features.contains(Features::RING_EVENT_IDX),
            negotiated_features.contains(Features::ACCESS_PLATFORM),
        )?;

        transport.finish_init();

        Ok(Self {
            transport,
            mac: mac.0,
            recv_queue,
            send_queue,
            legacy_header: !negotiated_features.contains(Features::VERSION_1)
                && !negotiated_features.contains(Features::MRG_RXBUF),
            rx_since_notify: 0,
            needs_rx_notify: false,
        })
    }

    pub(super) fn ack_interrupt(&mut self) -> InterruptStatus {
        self.transport.ack_interrupt()
    }

    pub(super) fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    pub(super) fn can_send(&self) -> bool {
        self.send_queue.available_desc() >= 2
    }

    pub(super) fn fill_buffer_header(&self, buffer: &mut [u8]) -> Result<usize> {
        if self.legacy_header {
            write_header::<VirtioNetHdrLegacy>(buffer)
        } else {
            write_header::<VirtioNetHdr>(buffer)
        }
    }

    /// # Safety
    ///
    /// `tx_buf` must stay unmodified until `transmit_complete` consumes `token`.
    pub(super) unsafe fn transmit_begin(&mut self, tx_buf: &[u8]) -> Result<u16> {
        self.check_tx_buf_len(tx_buf)?;
        // SAFETY: the caller keeps `tx_buf` live until `transmit_complete`.
        let token = unsafe { self.send_queue.add(&[tx_buf], &mut [])? };
        // Publish avail.idx before reading the device's notification request.
        fence(Ordering::SeqCst);
        if self.send_queue.should_notify() {
            self.transport.notify(QUEUE_TRANSMIT);
        }
        Ok(token)
    }

    pub(super) fn poll_transmit(&mut self) -> Option<u16> {
        self.send_queue.peek_used()
    }

    /// # Safety
    ///
    /// `tx_buf` must be the buffer `transmit_begin` submitted for `token`.
    pub(super) unsafe fn transmit_complete(&mut self, token: u16, tx_buf: &[u8]) -> Result<usize> {
        // SAFETY: `tx_buf` matches the descriptor chain identified by `token`.
        let len = unsafe { self.send_queue.pop_used(token, &[tx_buf], &mut [])? };
        Ok(len as usize)
    }

    /// Submits one receive buffer and batches requested kicks across at most
    /// [`RX_REFILL_BATCH`] refills, bounded by the queue capacity.
    ///
    /// # Safety
    ///
    /// `rx_buf` must stay live and must not be accessed until
    /// `receive_complete` consumes the returned token.
    pub(super) unsafe fn receive_begin(&mut self, rx_buf: &mut [u8]) -> Result<u16> {
        if rx_buf.len() < MIN_BUFFER_LEN {
            warn!("Receive buffer len {} is too small", rx_buf.len());
            return Err(Error::InvalidParam);
        }
        // SAFETY: the caller keeps `rx_buf` live until `receive_complete`.
        let token = unsafe { self.recv_queue.add(&[], &mut [rx_buf])? };
        // Virtio 1.2 section 2.7.13.4.1 requires a full barrier between
        // publishing avail.idx and reading flags/avail_event. Check each
        // publication: should_notify() is not an interval test and cannot
        // detect an event crossed by an entire batch spanning u16 wraparound.
        fence(Ordering::SeqCst);
        self.needs_rx_notify |= self.recv_queue.should_notify();
        self.rx_since_notify += 1;
        if usize::from(self.rx_since_notify) >= usize::from(RX_REFILL_BATCH).min(QS) {
            self.flush_receive_notify();
        }
        Ok(token)
    }

    /// Kicks the receive queue if a short refill batch is still unnotified.
    pub(super) fn flush_receive_notify(&mut self) {
        if self.rx_since_notify == 0 {
            return;
        }
        if self.needs_rx_notify {
            self.transport.notify(QUEUE_RECEIVE);
        }
        self.rx_since_notify = 0;
        self.needs_rx_notify = false;
    }

    pub(super) fn poll_receive(&self) -> Option<u16> {
        self.recv_queue.peek_used()
    }

    /// # Safety
    ///
    /// `rx_buf` must be the buffer `receive_begin` submitted for `token`.
    pub(super) unsafe fn receive_complete(
        &mut self,
        token: u16,
        rx_buf: &mut [u8],
    ) -> Result<(usize, usize)> {
        // SAFETY: `rx_buf` matches the descriptor chain identified by `token`.
        let len = unsafe { self.recv_queue.pop_used(token, &[], &mut [rx_buf])? } as usize;
        let hdr_size = if self.legacy_header {
            size_of::<VirtioNetHdrLegacy>()
        } else {
            size_of::<VirtioNetHdr>()
        };
        let packet_len = len.checked_sub(hdr_size).ok_or(Error::IoError)?;
        Ok((hdr_size, packet_len))
    }

    fn check_tx_buf_len(&self, tx_buf: &[u8]) -> Result<()> {
        let hdr_size = if self.legacy_header {
            size_of::<VirtioNetHdrLegacy>()
        } else {
            size_of::<VirtioNetHdr>()
        };
        if tx_buf.len() < hdr_size {
            warn!("Transmit buffer len {} is too small", tx_buf.len());
            Err(Error::InvalidParam)
        } else {
            Ok(())
        }
    }
}

fn write_header<H: Default + IntoBytes + Immutable>(buffer: &mut [u8]) -> Result<usize> {
    let header = H::default();
    let bytes = header.as_bytes();
    if buffer.len() < bytes.len() {
        return Err(Error::InvalidParam);
    }
    buffer[..bytes.len()].copy_from_slice(bytes);
    Ok(bytes.len())
}

#[cfg(unittest)]
#[path = "net_tests.rs"]
mod tests;
