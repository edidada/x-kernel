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

use device_res::{DmaAllocation, DmaMapping, DmaOp, DmaSpec, Io, MmioOp, MmioRegion};
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
        let provider = resource_provider();
        match provider.map_mmio(region, "virtio") {
            Ok(m) => {
                // Hold the mapping token for exactly as long as the object
                // graph that keeps using this pointer: inside a probe scope
                // the lease travels into the transport wrapper (dropping it
                // releases the mapping when the transport dies); outside a
                // probe scope nobody can return it, so keep it in the
                // fallback list — the mapping stays valid, matching the old
                // drop-the-token behavior minus the silent ref leak.
                let vaddr = m.vaddr;
                let mut lease = Some(Io::from_mapping(provider, m));
                if !mmio_probe_collector(|leases| leases.push(lease.take().expect("lease present")))
                {
                    FALLBACK_MMIO_LEASES
                        .lock()
                        .push(lease.expect("lease present"));
                }
                NonNull::new(vaddr as *mut u8).expect("non-null mmio")
            }
            Err(err) => {
                log::error!("virtio mmio map failed paddr={paddr:#x}: {err:?}");
                panic!("virtio mmio map failed")
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

// ---------------------------------------------------------------------------
// Probe-scoped MMIO lease collection
//
// The vendored virtio-drivers HAL trait returns a raw pointer from
// mmio_phys_to_virt; the mapping token would be dropped at the trait
// boundary and the memspace reference leaked. During a transport probe the
// runtime opens a collector scope: every mapping the vendored constructor
// takes is gathered here and attached to the transport wrapper, so the
// mapping lives exactly as long as the transport (and every class clone
// still holding the device) — no leak, no premature unmap.
// ---------------------------------------------------------------------------

static PROBE_MMIO_LEASES: kspin::SpinNoPreempt<Option<alloc::vec::Vec<Io>>> =
    kspin::SpinNoPreempt::new(None);

/// Mappings taken outside any probe scope: kept alive forever (same
/// validity as the old leaked tokens, now visible and countable).
static FALLBACK_MMIO_LEASES: kspin::SpinNoPreempt<alloc::vec::Vec<Io>> =
    kspin::SpinNoPreempt::new(alloc::vec::Vec::new());

/// Open a probe collection scope. Panics if one is already open (probes
/// are not nested today; a nesting need is a design conversation).
pub fn begin_mmio_probe() {
    let mut guard = PROBE_MMIO_LEASES.lock();
    assert!(guard.is_none(), "nested mmio probe scopes");
    *guard = Some(alloc::vec::Vec::new());
}

/// Close the scope and take every lease collected since
/// [`begin_mmio_probe`]. Always call on the error path too — the leases
/// drop here and the mappings are released.
pub fn take_mmio_probe_leases() -> alloc::vec::Vec<Io> {
    PROBE_MMIO_LEASES.lock().take().unwrap_or_default()
}

fn mmio_probe_collector(f: impl FnOnce(&mut alloc::vec::Vec<Io>)) -> bool {
    let mut guard = PROBE_MMIO_LEASES.lock();
    match guard.as_mut() {
        Some(leases) => {
            f(leases);
            true
        }
        None => false,
    }
}
