// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Adds transmit offload feature negotiation to the upstream raw net device.

use alloc::sync::Arc;
use core::{
    fmt::Debug,
    ops::BitAnd,
    sync::atomic::{AtomicU64, Ordering},
};

use bitflags::Flags;
use virtio_drivers::{
    PhysAddr, Result,
    transport::{DeviceStatus, DeviceType, InterruptStatus, Transport},
};
use zerocopy::{FromBytes, Immutable, IntoBytes};

pub(super) const VIRTIO_NET_F_CSUM: u64 = 1 << 0;
pub(super) const VIRTIO_NET_F_HOST_TSO4: u64 = 1 << 11;
pub(super) const VIRTIO_NET_F_HOST_ECN: u64 = 1 << 13;

pub(super) struct TsoTransport<T> {
    inner: T,
    negotiated_features: Arc<AtomicU64>,
}

impl<T> TsoTransport<T> {
    pub(super) fn new(inner: T, negotiated_features: Arc<AtomicU64>) -> Self {
        Self {
            inner,
            negotiated_features,
        }
    }
}

impl<T: Transport> Transport for TsoTransport<T> {
    fn device_type(&self) -> DeviceType {
        self.inner.device_type()
    }

    fn read_device_features(&mut self) -> u64 {
        self.inner.read_device_features()
    }

    fn write_driver_features(&mut self, driver_features: u64) {
        self.inner.write_driver_features(driver_features);
    }

    fn max_queue_size(&mut self, queue: u16) -> u32 {
        self.inner.max_queue_size(queue)
    }

    fn notify(&mut self, queue: u16) {
        self.inner.notify(queue);
    }

    fn get_status(&self) -> DeviceStatus {
        self.inner.get_status()
    }

    fn set_status(&mut self, status: DeviceStatus) {
        self.inner.set_status(status);
    }

    fn set_guest_page_size(&mut self, guest_page_size: u32) {
        self.inner.set_guest_page_size(guest_page_size);
    }

    fn requires_legacy_layout(&self) -> bool {
        self.inner.requires_legacy_layout()
    }

    fn queue_set(
        &mut self,
        queue: u16,
        size: u32,
        descriptors: PhysAddr,
        driver_area: PhysAddr,
        device_area: PhysAddr,
    ) {
        self.inner
            .queue_set(queue, size, descriptors, driver_area, device_area);
    }

    fn queue_unset(&mut self, queue: u16) {
        self.inner.queue_unset(queue);
    }

    fn queue_used(&mut self, queue: u16) -> bool {
        self.inner.queue_used(queue)
    }

    fn ack_interrupt(&mut self) -> InterruptStatus {
        self.inner.ack_interrupt()
    }

    fn begin_init<F: Flags<Bits = u64> + BitAnd<Output = F> + Debug>(
        &mut self,
        supported_features: F,
    ) -> F {
        let supported = F::from_bits_retain(
            supported_features.bits()
                | VIRTIO_NET_F_CSUM
                | VIRTIO_NET_F_HOST_TSO4
                | VIRTIO_NET_F_HOST_ECN,
        );
        let negotiated = self.inner.begin_init(supported);
        self.negotiated_features
            .store(negotiated.bits(), Ordering::Relaxed);
        negotiated
    }

    fn read_config_generation(&self) -> u32 {
        self.inner.read_config_generation()
    }

    fn read_config_space<V: FromBytes + IntoBytes>(&self, offset: usize) -> Result<V> {
        self.inner.read_config_space(offset)
    }

    fn write_config_space<V: IntoBytes + Immutable>(
        &mut self,
        offset: usize,
        value: V,
    ) -> Result<()> {
        self.inner.write_config_space(offset, value)
    }
}
