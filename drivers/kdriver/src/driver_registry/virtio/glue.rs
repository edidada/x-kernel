// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO device probing and HAL integration.
use alloc::{boxed::Box, sync::Arc};
use core::ptr::NonNull;

use cfg_if::cfg_if;
use device_res::{
    DmaAllocation, DmaDirection, DmaMapping, DmaSpec, MmioRegion, dma_provider, mmio_provider,
    try_dma_provider,
};
use driver_base::{Device, DriverResult};
#[cfg(target_arch = "x86_64")]
use kaddr_layout::{KIMAGE_VADDR, KIMAGE_VSIZE, LINEAR_MAP_VADDR, LINEAR_MAP_VSIZE};
#[cfg(target_arch = "x86_64")]
use khal::mem::{VirtAddr, v2p};
use virtio::{BufferDirection, PhysAddr, VirtIoHal};

cfg_if! {
    if #[cfg(feature = "virtio-net")] {
        pub struct VirtIoNet;

        impl VirtIoNet {
            pub fn try_new<T: virtio::Transport + 'static>(
                transport: T,
                irq: Option<usize>,
            ) -> DriverResult<kclass::prelude::NetDeviceImpl> {
                Ok(Box::new(virtio::VirtIoNetDev::<VirtIoHalImpl, T, 64>::try_new(
                    transport,
                    irq,
                )?))
            }
        }
    }
}

cfg_if! {
    if #[cfg(feature = "virtio-blk")] {
        pub struct VirtIoBlk;

        impl VirtIoBlk {
            pub fn try_new<T: virtio::Transport + 'static>(
                transport: T,
                _irq: Option<usize>,
            ) -> DriverResult<kclass::prelude::BlockDeviceImpl> {
                let device = virtio::VirtIoBlkDev::<VirtIoHalImpl, T>::try_new(transport)?;
                let name = device.name().into();
                let first_minor = device.index() << virtio::VIRTIO_BLK_PART_BITS;
                let disk = block::Gendisk::new(
                    name,
                    virtio::VIRTIO_BLK_MAJOR,
                    first_minor,
                    1 << virtio::VIRTIO_BLK_PART_BITS,
                    Box::new(device),
                )?;
                Ok(Arc::new(disk))
            }
        }
    }
}

cfg_if! {
    if #[cfg(feature = "virtio-gpu")] {
        pub struct VirtIoGpu;

        impl VirtIoGpu {
            pub fn try_new<T: virtio::Transport + 'static>(
                transport: T,
                _irq: Option<usize>,
            ) -> DriverResult<kclass::prelude::DisplayDeviceImpl> {
                Ok(Box::new(virtio::VirtIoGpuDev::<VirtIoHalImpl, T>::try_new(
                    transport,
                )?))
            }
        }
    }
}

cfg_if! {
    if #[cfg(feature = "virtio-input")] {
        pub struct VirtIoInput;

        impl VirtIoInput {
            pub fn try_new<T: virtio::Transport + 'static>(
                transport: T,
                _irq: Option<usize>,
            ) -> DriverResult<kclass::prelude::InputDeviceImpl> {
                Ok(Box::new(virtio::VirtIoInputDev::<VirtIoHalImpl, T>::try_new(
                    transport,
                )?))
            }
        }
    }
}

cfg_if! {
    if #[cfg(feature = "virtio-socket")] {
        pub struct VirtIoSocket;

        impl VirtIoSocket {
            pub fn try_new<T: virtio::Transport + 'static>(
                transport: T,
                irq: Option<usize>,
            ) -> DriverResult<kclass::prelude::VsockDeviceImpl> {
                Ok(Box::new(virtio::VirtIoSocketDev::<VirtIoHalImpl, T>::try_new(
                    transport, irq,
                )?))
            }
        }
    }
}

cfg_if! {
    if #[cfg(feature = "virtio-9p")] {
        pub struct VirtIo9p;

        impl VirtIo9p {
            pub fn try_new<T: virtio::Transport + 'static>(
                transport: T,
                _irq: Option<usize>,
            ) -> DriverResult<kclass::prelude::Virtio9pDeviceImpl> {
                Ok(Box::new(virtio::VirtIo9pDev::<VirtIoHalImpl, T>::try_new(
                    transport,
                )?))
            }
        }
    }
}

cfg_if! {
    if #[cfg(feature = "virtio-rng")] {
        pub struct VirtIoRng;

        impl VirtIoRng {
            pub fn try_new<T: virtio::Transport + 'static>(
                transport: T,
                _irq: Option<usize>,
            ) -> DriverResult<kclass::prelude::CharDeviceImpl> {
                Ok(Box::new(virtio::VirtIoRngDev::<VirtIoHalImpl, T>::try_new(
                    transport,
                )?))
            }
        }
    }
}

use memaddr::PAGE_SIZE_4K;
pub struct VirtIoHalImpl;

// SAFETY: `VirtIoHal` requires that DMA buffers handed to the device are
// valid for the device's view of memory and stay alive until `dma_dealloc`
// is called, that MMIO physical-to-virtual translations produce valid kernel
// mappings of the MMIO window, and that `share` / `unshare` keep CPU and
// device views of a buffer coherent.
//
// This implementation upholds those invariants as follows:
// * `dma_alloc` / `dma_dealloc` use `alloc_coherent` / `free_coherent`, which
//   return a paired (`cpu_addr`, `bus_addr`) for the same region and free it
//   in one call;
// * `mmio_phys_to_virt` uses `map_mmio`, which installs (or reuses) a kernel
//   mapping covering the requested MMIO range before returning a virtual
//   pointer;
// * when `VIRTIO_F_ACCESS_PLATFORM` is active, `share` / `unshare` use
//   `map_streaming` / `unmap_streaming` for cache maintenance and platform DMA
//   address translation;
// * without that feature, x86_64 buffers are passed directly only after their
//   physical pages are verified contiguous. Non-contiguous buffers continue
//   through the provider's bounce mapping, and x86 cache coherency keeps the
//   direct path's CPU and device views consistent for the queue-owned lifetime.
const fn hal_direction(direction: BufferDirection) -> DmaDirection {
    match direction {
        BufferDirection::DriverToDevice => DmaDirection::DriverToDevice,
        BufferDirection::DeviceToDriver => DmaDirection::DeviceToDriver,
        BufferDirection::Both => DmaDirection::Bidirectional,
    }
}

#[cfg(target_arch = "x86_64")]
fn try_identity_dma_address(buffer: NonNull<[u8]>) -> Option<PhysAddr> {
    let address = buffer.as_ptr() as *mut u8 as usize;
    let length = buffer.len();
    if length == 0 {
        return None;
    }
    let end = address.checked_add(length)?;
    let is_in_window = |start: usize, size: usize| {
        if address < start {
            return Some(false);
        }
        let offset = address - start;
        Some(offset <= size && length <= size - offset)
    };
    let is_in_kimage = is_in_window(KIMAGE_VADDR, KIMAGE_VSIZE)?;
    let is_in_linear_map = is_in_window(LINEAR_MAP_VADDR, LINEAR_MAP_VSIZE)?;
    if !is_in_kimage && !is_in_linear_map {
        return None;
    }
    let physical = v2p(VirtAddr::from_usize(address)).as_usize();
    let mut next_page = address.checked_add(PAGE_SIZE_4K - (address & (PAGE_SIZE_4K - 1)))?;
    while next_page < end {
        let expected = physical.checked_add(next_page - address)?;
        let actual = v2p(VirtAddr::from_usize(next_page)).as_usize();
        if actual != expected {
            return None;
        }
        next_page = next_page.checked_add(PAGE_SIZE_4K)?;
    }
    Some(physical as PhysAddr)
}

// SAFETY: DMA and MMIO allocation lifetimes are delegated to the installed
// provider. Streaming mappings use the provider whenever platform DMA access
// was negotiated. The x86_64 identity branch is restricted to normal kernel
// allocations covered by `v2p`; the VirtIO caller retains each allocation
// until the matching `unshare`, and x86 hardware supplies coherent DMA.
unsafe impl VirtIoHal for VirtIoHalImpl {
    fn dma_alloc(
        pages: usize,
        _direction: BufferDirection,
        _access_platform: bool,
    ) -> (PhysAddr, NonNull<u8>) {
        let size = pages * PAGE_SIZE_4K;
        let spec = DmaSpec::new(size, PAGE_SIZE_4K);
        match dma_provider().and_then(|p| p.alloc_coherent(spec)) {
            Ok(alloc) => {
                let cpu_ptr = alloc.cpu_addr as *mut u8;
                // SAFETY: `alloc.cpu_addr` was just returned by the provider and
                // points to `size` bytes of valid, exclusively-owned DMA memory.
                // Zeroing prevents the device from reading stale kernel data.
                unsafe { core::ptr::write_bytes(cpu_ptr, 0, size) };
                // Ownership of the allocation is handed to the virtio transport
                // and returned via `dma_dealloc`. We deliberately do NOT wrap it
                // in `DmaCoherent` (whose Drop would free it on return).
                let paddr = alloc.bus_addr as PhysAddr;
                let ptr = NonNull::new(cpu_ptr).expect("dma allocation stored a null CPU address");
                (paddr, ptr)
            }
            Err(err) => {
                log::error!("dma_alloc failed: pages={}, error={:?}", pages, err);
                (0, NonNull::dangling())
            }
        }
    }

    #[allow(unused_variables)]
    unsafe fn dma_dealloc(
        paddr: PhysAddr,
        vaddr: NonNull<u8>,
        pages: usize,
        _access_platform: bool,
    ) -> i32 {
        let spec = DmaSpec::new(pages * PAGE_SIZE_4K, PAGE_SIZE_4K);
        if let Some(p) = try_dma_provider() {
            p.free_coherent(DmaAllocation {
                cpu_addr: vaddr.as_ptr() as usize,
                bus_addr: paddr,
                spec,
            });
        }
        0
    }

    #[inline]
    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, size: usize) -> NonNull<u8> {
        let region = MmioRegion {
            base: paddr as usize,
            size,
        };
        match mmio_provider().and_then(|p| p.map_mmio(region, "virtio-hal")) {
            Ok(m) => NonNull::new(m.vaddr as *mut u8)
                .expect("virtio mmio mapping stored a null virtual address"),
            Err(err) => {
                log::error!(
                    "virtio mmio_phys_to_virt failed: paddr={:#x}, size={:#x}, err={:?}",
                    paddr,
                    size,
                    err
                );
                panic!("virtio mmio_phys_to_virt failed")
            }
        }
    }

    #[allow(unused_variables)]
    #[inline]
    unsafe fn share(
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        access_platform: bool,
    ) -> PhysAddr {
        #[cfg(target_arch = "x86_64")]
        if !access_platform && let Some(paddr) = try_identity_dma_address(buffer) {
            return paddr;
        }

        match dma_provider().and_then(|p| p.map_streaming(buffer, hal_direction(direction))) {
            Ok(m) => m.bus_addr as PhysAddr,
            Err(err) => {
                log::error!("virtio share failed: err={:?}", err);
                0
            }
        }
    }

    #[inline]
    #[allow(unused_variables)]
    unsafe fn unshare(
        paddr: PhysAddr,
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        access_platform: bool,
    ) {
        #[cfg(target_arch = "x86_64")]
        if !access_platform && try_identity_dma_address(buffer).is_some() {
            return;
        }

        let dir = hal_direction(direction);
        let Some(p) = try_dma_provider() else {
            return;
        };
        // SAFETY: `buffer` is valid (caller contract); `paddr` / `buffer` come
        // from a prior `share` call, so reconstructing the mapping reverses it.
        let slice = unsafe { buffer.as_ref() };
        let cpu_addr = NonNull::from(slice).cast::<u8>();
        p.unmap_streaming(DmaMapping {
            cpu_addr: cpu_addr.as_ptr() as usize,
            bus_addr: paddr,
            len: slice.len(),
            direction: dir,
        });
    }
}

#[cfg(all(unittest, target_arch = "x86_64"))]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    static TEST_BUFFER: [u8; 8] = [0; 8];

    #[def_test]
    fn test_share_without_access_platform_uses_physical_address() {
        let buffer = NonNull::from(&TEST_BUFFER[..]);
        let expected = try_identity_dma_address(buffer).unwrap();

        // SAFETY: `TEST_BUFFER` is a live, non-empty kernel-image allocation
        // and the test performs no concurrent access during this call.
        let actual =
            unsafe { VirtIoHalImpl::share(buffer, BufferDirection::DriverToDevice, false) };

        assert_eq!(actual, expected);
        // SAFETY: `actual` and `buffer` are the values from the matching
        // `share` call above, and the test performs no concurrent access.
        unsafe { VirtIoHalImpl::unshare(actual, buffer, BufferDirection::DriverToDevice, false) };
    }
}
