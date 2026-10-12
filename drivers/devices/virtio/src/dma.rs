// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device-bound DMA injection through an otherwise unchanged transport.

use alloc::sync::Arc;
use core::ptr::NonNull;

use device_res::{DmaAllocation, DmaDirection, DmaMapping, DmaOp, DmaSpec};
use virtio_drivers::{
    BufferDirection, DmaProvider, Error, PAGE_SIZE, PhysAddr,
    transport::{DeviceStatus, DeviceType, InterruptStatus, Transport},
};
use zerocopy::{FromBytes, Immutable, IntoBytes};

/// A VirtIO transport retaining the DMA capability of its hardware device.
pub struct DmaTransport<T> {
    inner: T,
    dma: Arc<DeviceDma>,
}

impl<T> DmaTransport<T> {
    /// Associate a transport with the provider selected by device probe.
    pub fn new(inner: T, provider: Arc<dyn DmaOp>) -> Self {
        Self {
            inner,
            dma: Arc::new(DeviceDma { provider }),
        }
    }
}

struct DeviceDma {
    provider: Arc<dyn DmaOp>,
}

impl core::fmt::Debug for DeviceDma {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("DeviceDma")
    }
}

fn direction(value: BufferDirection) -> DmaDirection {
    match value {
        BufferDirection::DriverToDevice => DmaDirection::DriverToDevice,
        BufferDirection::DeviceToDriver => DmaDirection::DeviceToDriver,
        BufferDirection::Both => DmaDirection::Bidirectional,
    }
}

// SAFETY: allocations and mappings are delegated to the same retained device
// provider. Coherent pages are zeroed before publication, and queue ownership
// keeps streaming buffers live and exclusive until completion or rollback.
unsafe impl DmaProvider for DeviceDma {
    fn allocate(
        &self,
        pages: usize,
        _direction: BufferDirection,
        _access_platform: bool,
    ) -> virtio_drivers::Result<(PhysAddr, NonNull<u8>)> {
        let len = pages.checked_mul(PAGE_SIZE).ok_or(Error::DmaError)?;
        let allocation = self
            .provider
            .alloc_coherent(DmaSpec::new(len, PAGE_SIZE))
            .map_err(|_| Error::DmaError)?;
        let ptr = NonNull::new(allocation.cpu_addr as *mut u8).ok_or(Error::DmaError)?;
        // SAFETY: provider returned an exclusive allocation of len bytes and hardware has not seen it.
        unsafe { ptr.as_ptr().write_bytes(0, len) };
        Ok((allocation.bus_addr, ptr))
    }

    unsafe fn deallocate(
        &self,
        addr: PhysAddr,
        ptr: NonNull<u8>,
        pages: usize,
        _access_platform: bool,
    ) {
        // SAFETY: queue destruction supplies its original allocation after stopping DMA.
        unsafe {
            self.provider.free_coherent(DmaAllocation {
                cpu_addr: ptr.as_ptr() as usize,
                bus_addr: addr,
                spec: DmaSpec::new(pages * PAGE_SIZE, PAGE_SIZE),
            })
        };
    }

    unsafe fn share(
        &self,
        buffer: NonNull<[u8]>,
        dir: BufferDirection,
        _access_platform: bool,
    ) -> virtio_drivers::Result<PhysAddr> {
        // SAFETY: the queue retains the initialized buffer exclusively until completion or rollback.
        unsafe { self.provider.map_streaming(buffer, direction(dir)) }
            .map(|mapping| mapping.bus_addr)
            .map_err(|_| Error::DmaError)
    }

    unsafe fn unshare(
        &self,
        addr: PhysAddr,
        buffer: NonNull<[u8]>,
        dir: BufferDirection,
        _access_platform: bool,
    ) {
        // SAFETY: the queue supplies original mapping arguments after completion or before publication.
        unsafe {
            self.provider.unmap_streaming(DmaMapping {
                cpu_addr: buffer.cast::<u8>().as_ptr() as usize,
                bus_addr: addr,
                len: buffer.len(),
                direction: direction(dir),
            })
        };
    }
}

impl<T: Transport> Transport for DmaTransport<T> {
    fn dma_provider(&self) -> Option<Arc<dyn DmaProvider>> {
        Some(self.dma.clone())
    }

    fn device_type(&self) -> DeviceType {
        self.inner.device_type()
    }

    fn read_device_features(&mut self) -> u64 {
        self.inner.read_device_features()
    }

    fn write_driver_features(&mut self, features: u64) {
        self.inner.write_driver_features(features);
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

    fn set_guest_page_size(&mut self, size: u32) {
        self.inner.set_guest_page_size(size);
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

    fn read_config_generation(&self) -> u32 {
        self.inner.read_config_generation()
    }

    fn read_config_space<V: FromBytes + IntoBytes>(
        &self,
        offset: usize,
    ) -> virtio_drivers::Result<V> {
        self.inner.read_config_space(offset)
    }

    fn write_config_space<V: IntoBytes + Immutable>(
        &mut self,
        offset: usize,
        value: V,
    ) -> virtio_drivers::Result<()> {
        self.inner.write_config_space(offset, value)
    }
}
