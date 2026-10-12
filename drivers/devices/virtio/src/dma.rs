// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Adapt device-res DMA capabilities for VirtIO transports.

use alloc::sync::Arc;
use core::ptr::NonNull;

use device_res::{DmaAllocation, DmaDirection, DmaMapping, DmaOp, DmaSpec};
use virtio_drivers::{BufferDirection, DmaProvider, Error, PAGE_SIZE, PhysAddr};

/// Adapt one device-res provider for binding to a VirtIO transport at probe.
pub fn dma_provider(provider: Arc<dyn DmaOp>) -> Arc<dyn DmaProvider> {
    Arc::new(DeviceDma { provider })
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
