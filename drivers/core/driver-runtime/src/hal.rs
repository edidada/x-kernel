// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The x-kernel HAL for the vendored virtio-drivers transports.
//!
//! Bridges DMA allocation, MMIO mapping, and streaming DMA share/unshare
//! onto the process-wide resource provider. Mappings made here live for
//! the transport's lifetime (the transport owns them); kernel-owned
//! transports remain a later refactor item.

use core::ptr::NonNull;

use device_res::{DmaAllocation, DmaMapping, DmaOp, DmaSpec, MmioOp, MmioRegion};
use memaddr::PAGE_SIZE_4K;
use virtio::{BufferDirection, PhysAddr, VirtIoHal};

use crate::resource_provider;

pub struct Hal;

unsafe impl VirtIoHal for Hal {
    fn dma_alloc(
        pages: usize,
        _direction: BufferDirection,
        _access_platform: bool,
    ) -> (PhysAddr, NonNull<u8>) {
        let size = pages * PAGE_SIZE_4K;
        let spec = DmaSpec::new(size, PAGE_SIZE_4K);
        match resource_provider().alloc_coherent(spec) {
            Ok(alloc) => {
                let cpu_ptr = alloc.cpu_addr as *mut u8;
                unsafe { core::ptr::write_bytes(cpu_ptr, 0, size) };
                (
                    alloc.bus_addr as PhysAddr,
                    NonNull::new(cpu_ptr).expect("non-null dma"),
                )
            }
            Err(err) => {
                log::error!("virtio-blk dma_alloc failed: pages={pages} err={err:?}");
                (0, NonNull::dangling())
            }
        }
    }

    unsafe fn dma_dealloc(
        paddr: PhysAddr,
        vaddr: NonNull<u8>,
        pages: usize,
        _access_platform: bool,
    ) -> i32 {
        let spec = DmaSpec::new(pages * PAGE_SIZE_4K, PAGE_SIZE_4K);
        resource_provider().free_coherent(DmaAllocation {
            cpu_addr: vaddr.as_ptr() as usize,
            bus_addr: paddr,
            spec,
        });
        0
    }

    #[inline]
    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, size: usize) -> NonNull<u8> {
        let region = MmioRegion {
            base: paddr as usize,
            size,
        };
        match resource_provider().map_mmio(region, "virtio-blk") {
            Ok(m) => NonNull::new(m.vaddr as *mut u8).expect("non-null mmio"),
            Err(err) => {
                log::error!("virtio-blk mmio map failed paddr={paddr:#x}: {err:?}");
                panic!("virtio-blk mmio map failed")
            }
        }
    }

    #[inline]
    unsafe fn share(
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        _access_platform: bool,
    ) -> PhysAddr {
        let dir = match direction {
            BufferDirection::DriverToDevice => device_res::DmaDirection::DriverToDevice,
            BufferDirection::DeviceToDriver => device_res::DmaDirection::DeviceToDriver,
            BufferDirection::Both => device_res::DmaDirection::Bidirectional,
        };
        match resource_provider().map_streaming(buffer, dir) {
            Ok(m) => m.bus_addr as PhysAddr,
            Err(err) => {
                log::error!("virtio-blk share failed: {err:?}");
                0
            }
        }
    }

    #[inline]
    unsafe fn unshare(
        paddr: PhysAddr,
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        _access_platform: bool,
    ) {
        if paddr == 0 {
            return;
        }
        let dir = match direction {
            BufferDirection::DriverToDevice => device_res::DmaDirection::DriverToDevice,
            BufferDirection::DeviceToDriver => device_res::DmaDirection::DeviceToDriver,
            BufferDirection::Both => device_res::DmaDirection::Bidirectional,
        };
        let slice = unsafe { buffer.as_ref() };
        let cpu_addr = NonNull::from(slice).cast::<u8>();
        resource_provider().unmap_streaming(DmaMapping {
            cpu_addr: cpu_addr.as_ptr() as usize,
            bus_addr: paddr,
            len: slice.len(),
            direction: dir,
        });
    }
}
