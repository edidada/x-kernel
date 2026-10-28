// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Physical page inventory and PCP routing, independent of the kernel heap.

use core::sync::atomic::{AtomicBool, Ordering};

use alloc_engine::{AllocError, AllocResult, BaseAllocator, BuddyPageAllocator, PageAllocator};
use kspin::SpinNoIrq;
use memaddr::{PAGE_SIZE_4K, PhysAddr};

use crate::{
    AllocLocalOwner, UsageKind, kernel_virt_to_phys, pages_to_bytes, pcp, record_usage_alloc,
    record_usage_dealloc,
};

/// Owns physical page inventory and routes requests through PCP and buddy.
///
/// Allocation and release exchange physical addresses, not content pointers.
/// The caller owns each raw allocation until matching release. Access to its
/// contents requires a separate mapping. This service does not own the heap.
pub struct PhysicalPageAllocator {
    palloc: SpinNoIrq<BuddyPageAllocator<PAGE_SIZE_4K>>,
    ready: AtomicBool,
}

impl Default for PhysicalPageAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl PhysicalPageAllocator {
    /// Creates an empty page inventory. Kernel users share page_allocator().
    pub const fn new() -> Self {
        Self {
            palloc: SpinNoIrq::new(BuddyPageAllocator::new()),
            ready: AtomicBool::new(false),
        }
    }

    /// Registers exclusively owned RAM with a permanent writable direct mapping.
    ///
    /// Boot serializes registration before other users start. Keep these mappings
    /// valid for the inventory lifetime and exclude all other owners. The first
    /// region makes pages ready without bootstrapping or depending on the heap.
    ///
    /// # Errors
    /// Returns the backend error for invalid or overlapping memory regions.
    pub fn add_memory(&self, paddr: PhysAddr, size: usize) -> AllocResult {
        self.palloc
            .lock()
            .add_region(kaddr_layout::p2v(paddr.as_usize()), size)?;
        self.ready.store(true, Ordering::Release);
        Ok(())
    }

    /// Reports whether usable page inventory has been registered.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Allocates physically contiguous pages and charges their original category.
    ///
    /// Returns an owned physical base address without creating a content mapping.
    /// Release must use the same base, page count and category. The size and
    /// alignment behavior follows the existing buddy/PCP allocation protocol.
    ///
    /// # Errors
    /// Returns the inventory allocation error, including NoMemory on exhaustion.
    pub fn alloc_pages(
        &self,
        num_pages: usize,
        align_pow2: usize,
        kind: UsageKind,
    ) -> AllocResult<PhysAddr> {
        let pa = self.alloc_from_inventory(num_pages, align_pow2)?;
        record_usage_alloc(kind, pages_to_bytes(num_pages));
        Ok(pa)
    }

    /// Takes exclusive backing without usage accounting for kernel heap growth.
    /// Buddy and PCP retain their direct-map address storage behind this boundary.
    pub(super) fn alloc_from_inventory(
        &self,
        num_pages: usize,
        align_pow2: usize,
    ) -> AllocResult<PhysAddr> {
        debug_assert!(
            self.ready.load(Ordering::Acquire),
            "page allocator is not initialized"
        );

        // Fast path: 1–4 pages with standard alignment -> PCP cache.
        if (1..=pcp::PCP_MAX_PAGES).contains(&num_pages) && align_pow2 <= PAGE_SIZE_4K {
            let mut local_owner = AllocLocalOwner::acquire();
            if let Some(addr) = pcp::try_alloc(&mut local_owner, num_pages) {
                drop(local_owner);
                return Ok(PhysAddr::from(kernel_virt_to_phys(addr)));
            }
            drop(local_owner);

            // Cache miss: batch-fill from the global allocator.
            //
            // Acquire the owner before `palloc`: it stores the pre-lock IRQ
            // state, so nesting it inside the `SpinNoIrq` guard would restore
            // "IRQs disabled" when it drops after `palloc` already re-enabled
            // them, leaking an IRQ-off window back to the caller.
            let mut local_owner = AllocLocalOwner::acquire();
            let mut palloc = self.palloc.lock();
            let result = pcp::fill_cache(&mut local_owner, num_pages, &mut palloc);
            drop(palloc);
            return result.map(|va| PhysAddr::from(kernel_virt_to_phys(va)));
        }

        // Slow path: multi-page or special alignment.
        let addr = self.palloc.lock().allocate_pages(num_pages, align_pow2)?;
        Ok(PhysAddr::from(kernel_virt_to_phys(addr)))
    }

    /// Allocates contiguous DMA pages.
    ///
    /// DMA allocations must come from physical memory below 4 GiB.
    /// The DMA32 constraint is checked on physical addresses; internal buddy
    /// addresses remain in the kernel direct map.
    pub fn alloc_dma_pages(
        &self,
        num_pages: usize,
        align_pow2: usize,
        kind: UsageKind,
    ) -> AllocResult<PhysAddr> {
        debug_assert!(
            self.ready.load(Ordering::Acquire),
            "page allocator is not initialized"
        );
        const DMA32_LIMIT: usize = 0x1_0000_0000;

        let addr = self.palloc.lock().allocate_pages(num_pages, align_pow2)?;

        let phys = kernel_virt_to_phys(addr);
        let size = pages_to_bytes(num_pages);
        if phys + size > DMA32_LIMIT {
            self.palloc.lock().deallocate_pages(addr, num_pages);
            return Err(AllocError::NoMemory);
        }

        record_usage_alloc(kind, size);
        Ok(PhysAddr::from(phys))
    }

    /// Allocates contiguous pages starting from the given address.
    ///
    /// It allocates `num_pages` pages from the page allocator starting from the
    /// given address.
    ///
    /// `align_pow2` must be a power of 2, and the returned region bound will be
    /// aligned to it.
    pub fn alloc_pages_at(
        &self,
        paddr: PhysAddr,
        num_pages: usize,
        align_pow2: usize,
        kind: UsageKind,
    ) -> AllocResult<PhysAddr> {
        self.palloc.lock().allocate_pages_at(
            kaddr_layout::p2v(paddr.as_usize()),
            num_pages,
            align_pow2,
        )?;
        record_usage_alloc(kind, pages_to_bytes(num_pages));
        Ok(paddr)
    }

    /// Returns an allocation to the physical-page service.
    ///
    /// The PA, page count and category must match one live allocation from this
    /// service, with all software and hardware use completed. This operation
    /// does not establish ownership from a bare address or detect every double free.
    ///
    /// 1–4 page deallocations are cached per-CPU to reduce global lock
    /// contention.
    ///
    /// [`alloc_pages`]: Self::alloc_pages
    pub fn dealloc_pages(&self, paddr: PhysAddr, num_pages: usize, kind: UsageKind) {
        record_usage_dealloc(kind, pages_to_bytes(num_pages));
        self.release_to_inventory(paddr, num_pages);
    }

    /// Returns exclusively owned backing without object usage accounting.
    pub(super) fn release_to_inventory(&self, paddr: PhysAddr, num_pages: usize) {
        let va = kaddr_layout::p2v(paddr.as_usize());
        // Fast path: 1–4 pages -> try to cache them per-CPU.
        if (1..=pcp::PCP_MAX_PAGES).contains(&num_pages) {
            let mut local_owner = AllocLocalOwner::acquire();
            if pcp::try_free(&mut local_owner, num_pages, va) {
                return;
            }
            drop(local_owner);

            // Cache full: bulk-drain to the global allocator. Acquire the owner
            // before `palloc` for the same IRQ-state nesting reason as
            // `alloc_pages`.
            let mut local_owner = AllocLocalOwner::acquire();
            let mut palloc = self.palloc.lock();
            pcp::drain_cache(&mut local_owner, num_pages, &mut palloc, va);
            return;
        }

        // Slow path: multi-page.
        self.palloc.lock().deallocate_pages(va, num_pages);
    }

    /// Returns DMA backing using its original physical base, count and category.
    pub fn dealloc_dma_pages(&self, paddr: PhysAddr, num_pages: usize, kind: UsageKind) {
        record_usage_dealloc(kind, pages_to_bytes(num_pages));
        self.palloc
            .lock()
            .deallocate_pages(kaddr_layout::p2v(paddr.as_usize()), num_pages);
    }

    /// Returns the total number of allocatable pages managed by the page allocator.
    ///
    /// The total excludes allocator metadata such as the buddy free-head bitmap.
    pub fn total_pages(&self) -> usize {
        self.palloc.lock().total_pages()
    }

    /// Returns the number of allocated pages in the page allocator.
    pub fn used_pages(&self) -> usize {
        self.palloc.lock().used_pages()
    }

    /// Returns the number of available pages in the page allocator.
    pub fn available_pages(&self) -> usize {
        self.palloc.lock().available_pages()
    }
}

static PAGE_ALLOCATOR: PhysicalPageAllocator = PhysicalPageAllocator::new();

/// Returns the shared system page inventory, independently of the object heap.
pub fn page_allocator() -> &'static PhysicalPageAllocator {
    &PAGE_ALLOCATOR
}

/// Reports readiness of the system page inventory.
pub fn is_page_allocator_ready() -> bool {
    PAGE_ALLOCATOR.is_ready()
}

unittest::path_mod_test!(tests, "../ut-tests/pages.rs");
