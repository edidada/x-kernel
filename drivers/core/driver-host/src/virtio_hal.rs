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

use driver_core::{
    MmioRegion,
    provider::{DmaAllocation, DmaMapping, DmaOp, DmaSpec, MmioOp},
};
use memaddr::PAGE_SIZE_4K;
use virtio_devices::{BufferDirection, PhysAddr, VirtIoHal};

use crate::resource_provider;

pub struct Hal;

// SAFETY: the HAL routes every operation through the global x-kernel
// resource provider with the lifetimes the vendored trait documents
// (DMA and MMIO live until the matching dealloc/unmap).
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
                // SAFETY: the provider returned `size` bytes of coherent
                // memory exclusively owned by this allocation.
                unsafe { core::ptr::write_bytes(cpu_ptr, 0, size) };
                #[expect(
                    clippy::expect_used,
                    reason = "CG-021-no-panic-on-reachable-failure: the provider handed out \\
                              an exclusive allocation — its cpu_addr cannot be null"
                )]
                {
                    (
                        alloc.bus_addr as PhysAddr,
                        NonNull::new(cpu_ptr).expect("non-null dma"),
                    )
                }
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
                // SAFETY: the provider mapped the region; the lease's
                // release hook pairs the provider's unmap on drop.
                let lease = unsafe {
                    driver_core::IoMap::new(
                        m.vaddr,
                        region,
                        Some(alloc::sync::Arc::new(move || provider.unmap_mmio(m))),
                    )
                };
                let vaddr = lease.as_ptr() as usize;
                // The two `expect`s and the closing one assert the same
                // invariant: `lease` was just wrapped in `Some` and taken
                // at most once by the collector closure.
                #[expect(
                    clippy::expect_used,
                    reason = "CG-021-no-panic-on-reachable-failure: the lease was wrapped in \\
                              Some immediately above; the collector takes it at most once"
                )]
                {
                    let mut lease = Some(lease);
                    if !mmio_probe_collector(|leases| {
                        leases.push(lease.take().expect("lease present"))
                    }) {
                        FALLBACK_MMIO_LEASES
                            .lock()
                            .push(lease.expect("lease present"));
                    }
                }
                #[expect(
                    clippy::expect_used,
                    reason = "CG-021-no-panic-on-reachable-failure: the provider mapped the \\
                              region — its vaddr cannot be null"
                )]
                {
                    NonNull::new(vaddr as *mut u8).expect("non-null mmio")
                }
            }
            Err(err) => {
                #[expect(
                    clippy::panic,
                    reason = "CG-021-no-panic-on-reachable-failure: the vendored Hal trait's \\
                              associated function cannot return an error — a failed map is \\
                              unrecoverable at this boundary (driver paths map through \\
                              DeviceCtx::iomap, which reports ProbeError properly)"
                )]
                {
                    log::error!("virtio mmio map failed paddr={paddr:#x}: {err:?}");
                    panic!("virtio mmio map failed")
                }
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
            BufferDirection::DriverToDevice => driver_core::provider::DmaDirection::DriverToDevice,
            BufferDirection::DeviceToDriver => driver_core::provider::DmaDirection::DeviceToDriver,
            BufferDirection::Both => driver_core::provider::DmaDirection::Bidirectional,
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
            BufferDirection::DriverToDevice => driver_core::provider::DmaDirection::DriverToDevice,
            BufferDirection::DeviceToDriver => driver_core::provider::DmaDirection::DeviceToDriver,
            BufferDirection::Both => driver_core::provider::DmaDirection::Bidirectional,
        };
        // SAFETY: the vendored contract hands us a buffer valid for the
        // duration of this call; only a shared read happens here.
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

static PROBE_MMIO_LEASES: kspin::SpinNoPreempt<Option<alloc::vec::Vec<driver_core::IoMap>>> =
    kspin::SpinNoPreempt::new(None);

/// Mappings taken outside any probe scope: kept alive forever (same
/// validity as the old leaked tokens, now visible and countable).
static FALLBACK_MMIO_LEASES: kspin::SpinNoPreempt<alloc::vec::Vec<driver_core::IoMap>> =
    kspin::SpinNoPreempt::new(alloc::vec::Vec::new());

/// Open a probe collection scope, returning a guard.
///
/// # Serialization contract
///
/// PCI transport probes are serialized today (the boot-time probe pump
/// runs single-threaded; the review's concurrent-probe scenario is an
/// architecture change, not a current path). The single-slot collector
/// ENFORCES that contract: a second scope while one is open panics
/// loudly instead of silently mixing leases across probes.
///
/// The guard's Drop closes the scope — a panic between open and take
/// no longer leaves the slot stuck open (the old begin/take pair had no
/// error-path recovery).
pub fn begin_mmio_probe() -> MmioProbeScope<'static> {
    let mut guard = PROBE_MMIO_LEASES.lock();
    assert!(
        guard.is_none(),
        "concurrent mmio probe scopes: transport probes are serialized by contract; nesting is a \
         design change, not a bug workaround"
    );
    *guard = Some(alloc::vec::Vec::new());
    drop(guard);
    MmioProbeScope {
        _priv: core::marker::PhantomData,
    }
}

/// Scope guard: closing on Drop releases the slot. Use
/// [`MmioProbeScope::take_leases`] to move the collected mappings out
/// (the scope is then closed by the same Drop).
pub struct MmioProbeScope<'a> {
    _priv: core::marker::PhantomData<&'a ()>,
}

impl MmioProbeScope<'static> {
    /// Take every lease collected since the scope opened.
    pub fn take_leases(self) -> alloc::vec::Vec<driver_core::IoMap> {
        let leases = PROBE_MMIO_LEASES.lock().take().unwrap_or_default();
        core::mem::forget(self); // closed manually; Drop must not double-take
        leases
    }
}

impl Drop for MmioProbeScope<'_> {
    fn drop(&mut self) {
        // Error path: drop the collected leases (mappings released) and
        // close the slot.
        let _ = PROBE_MMIO_LEASES.lock().take();
    }
}

/// Back-compat shim for callers using the begin/take pair manually.
/// Prefer [`begin_mmio_probe`]'s guard.
pub fn take_mmio_probe_leases() -> alloc::vec::Vec<driver_core::IoMap> {
    PROBE_MMIO_LEASES.lock().take().unwrap_or_default()
}

fn mmio_probe_collector(f: impl FnOnce(&mut alloc::vec::Vec<driver_core::IoMap>)) -> bool {
    let mut guard = PROBE_MMIO_LEASES.lock();
    match guard.as_mut() {
        Some(leases) => {
            f(leases);
            true
        }
        None => false,
    }
}
