// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The dma-heap exporter: cacheable buffers allocated from `kdma`.
//!
//! `/dev/dma_heap/*` is the allocation half of dma-buf. A client opens a heap
//! node, issues [`DMA_HEAP_IOCTL_ALLOC`](crate::uapi::DMA_HEAP_IOCTL_ALLOC),
//! and receives a descriptor whose object is a [`HeapBuffer`]. From then on
//! everything happens through the descriptor — `mmap`, `lseek`,
//! `DMA_BUF_IOCTL_SYNC` — exactly as for any other exporter.
//!
//! # Cacheability
//!
//! The buffer is shared with userspace as ordinary memory, so both mappings
//! are cacheable. A non-coherent device therefore needs explicit maintenance:
//! [`DmaBuf::begin_cpu_access`] invalidates before the CPU reads what a device
//! wrote, and [`DmaBuf::end_cpu_access`] cleans after the CPU wrote and before
//! a device reads. The descriptor decides when to call them from the
//! `DMA_BUF_IOCTL_SYNC` flags; this module is the policy.
//!
//! # Length and initial contents
//!
//! A request is rounded up to a whole page before it becomes the buffer's
//! size, matching `dma_heap_buffer_alloc`. `lseek(SEEK_END)`, `mmap` and
//! `SYNC` then all speak about one length, so a sub-page request cannot expose
//! a mapped tail that `SYNC` never reaches.
//!
//! The full page-rounded allocation is zeroed and cleaned to the Point of
//! Coherency before the object is published. Userspace must not observe a
//! previous owner's data, and a later `SYNC START | READ` invalidation
//! refetches the zeros instead of reviving stale cache lines.
//!
//! # No per-heap policy
//!
//! `system` and `cma` are names for the same allocator: this kernel has one
//! pool of physically contiguous DMA memory, so a separate CMA policy would
//! be a name without a difference. The heap name is kept only because Linux
//! reports it as the export's `exp_name`.

use alloc::sync::Arc;
use core::{
    alloc::Layout,
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering},
};

use kdma::{DmaBusAddress, DmaInfo};
use kerrno::{KError, KResult};
use memaddr::{PAGE_SIZE_4K, PhysAddr, VirtAddr};

use crate::object::{CpuAccess, DmaBuf, ExportId, MappedWindow};

/// Exports handed out by the heap, in creation order.
///
/// Zero is reserved so an [`ExportId`] can never be mistaken for "no export";
/// Linux uses the `struct dma_buf` address for the same identity.
static NEXT_EXPORT_ID: AtomicU64 = AtomicU64::new(1);

/// One physically contiguous, cacheable DMA buffer owned by a heap.
///
/// The object is reachable only through an `Arc`, and every descriptor and
/// user mapping of the buffer holds one. Dropping the last reference returns
/// the pages exactly once.
pub struct HeapBuffer {
    export_id: ExportId,
    /// The heap that allocated the buffer, reported as `dma_buf->exp_name`.
    name: &'static str,
    /// CPU address of the linear mapping.
    ///
    /// Stored as an integer rather than `NonNull` so the object stays
    /// `Send + Sync`, which the [`DmaBuf`] contract requires.
    kva: usize,
    /// Bus address the allocation was made with.
    bus_addr: DmaBusAddress,
    /// Physical base, which is the window a user mapping covers.
    paddr: PhysAddr,
    /// Page-aligned length; the one length every interface reports.
    size: usize,
    /// The exact layout the allocation was made with, so the free matches.
    layout: Layout,
}

/// Rounds a heap request up to a whole page.
///
/// This mirrors `PAGE_ALIGN` in Linux's `dma_heap_buffer_alloc`; the rounded
/// length is the buffer's size. Returns `None` when the round-up overflows.
fn page_aligned_len(len: usize) -> Option<usize> {
    len.checked_add(PAGE_SIZE_4K - 1)
        .map(|len| len & !(PAGE_SIZE_4K - 1))
}

/// Allocates a cacheable heap buffer of at least `len` bytes from `name`.
///
/// This is the entry point `/dev/dma_heap/*` uses; it returns the object that
/// implements [`DmaBuf`] for the descriptor a caller installs.
///
/// # Errors
///
/// Same as [`HeapBuffer::alloc`].
pub fn alloc(name: &'static str, len: usize) -> KResult<Arc<HeapBuffer>> {
    HeapBuffer::alloc(name, len)
}

impl HeapBuffer {
    /// Allocates a cacheable heap buffer of at least `len` bytes.
    ///
    /// `name` is the heap that owns the allocation (`"system"`, `"cma"`, …);
    /// it is reported as the export's name and never affects the memory.
    ///
    /// # Errors
    ///
    /// [`KError::InvalidInput`] if `len` is zero, cannot be rounded to a page,
    /// or cannot form a layout; [`KError::NoMemory`] if no contiguous run is
    /// available.
    pub fn alloc(name: &'static str, len: usize) -> KResult<Arc<Self>> {
        if len == 0 {
            return Err(KError::InvalidInput);
        }
        let size = page_aligned_len(len).ok_or(KError::InvalidInput)?;
        let layout =
            Layout::from_size_align(size, PAGE_SIZE_4K).map_err(|_| KError::InvalidInput)?;
        // SAFETY: `layout` is a valid non-zero layout. Ownership of the
        // returned allocation is established here: the `Arc<HeapBuffer>`
        // below is its sole holder, and `Drop` releases it exactly once with
        // the same stored layout. The cacheable contract is carried by the
        // type: `begin/end_cpu_access` are the supported clean/invalidate
        // paths, and the window is only handed out through `DmaBuf`, whose
        // callers keep the export alive while a device addresses it. No
        // reference to the allocation or its address escapes before the
        // wrapper exists.
        let info =
            unsafe { kdma::allocate_dma_memory_cacheable(layout) }.map_err(|_| KError::NoMemory)?;
        // SAFETY: `info.cpu_addr` is the first byte of the live `size`-byte
        // allocation returned above, which this object owns exclusively; no
        // reference to it has escaped yet.
        unsafe { core::ptr::write_bytes(info.cpu_addr.as_ptr(), 0, size) };
        // The zeroing stores left dirty cache lines. A later invalidation
        // (SYNC START | READ) discards clean lines; if the zeros never
        // reached the Point of Coherency, that invalidation would revive
        // whatever the previous owner left there. Clean them now, before
        // publication.
        karch::dma_clean_range(VirtAddr::from(info.cpu_addr.as_ptr() as usize), size);
        Ok(Arc::new(Self {
            export_id: ExportId::new(NEXT_EXPORT_ID.fetch_add(1, Ordering::Relaxed)),
            name,
            kva: info.cpu_addr.as_ptr() as usize,
            bus_addr: info.bus_addr,
            paddr: kdma::b2p(info.bus_addr),
            size,
            layout,
        }))
    }

    /// Makes the CPU and a non-coherent device agree before the CPU reads.
    fn invalidate(&self) {
        karch::dma_invalidate_range(VirtAddr::from(self.kva), self.size);
    }

    /// Publishes the CPU's writes before a device reads.
    fn clean(&self) {
        karch::dma_clean_range(VirtAddr::from(self.kva), self.size);
    }
}

impl DmaBuf for HeapBuffer {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn export_id(&self) -> ExportId {
        self.export_id
    }

    fn exporter_name(&self) -> &str {
        self.name
    }

    fn size_bytes(&self) -> u64 {
        self.size as u64
    }

    fn window(&self) -> MappedWindow {
        MappedWindow::new(self.paddr, self.size)
    }

    fn begin_cpu_access(&self, access: CpuAccess) -> KResult<()> {
        // A read consumes what a device wrote. A start of write access has
        // nothing to make coherent yet; Linux's direction switch treats it as
        // a no-op too.
        if access.reads_memory() {
            self.invalidate();
        }
        Ok(())
    }

    fn end_cpu_access(&self, access: CpuAccess) -> KResult<()> {
        // A write must reach memory before a device reads it. An end of read
        // access publishes nothing.
        if access.writes_memory() {
            self.clean();
        }
        Ok(())
    }
}

impl Drop for HeapBuffer {
    fn drop(&mut self) {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: a HeapBuffer is built from a \
                      successful allocation, so its kva is non-null"
        )]
        let cpu_addr = NonNull::new(self.kva as *mut u8)
            .expect("a HeapBuffer is built from a successful allocation, so its kva is non-null");
        let info = DmaInfo {
            cpu_addr,
            bus_addr: self.bus_addr,
        };
        // SAFETY: `info` and `layout` describe the allocation made in `alloc`
        // for this object, and `Drop` runs exactly once.
        unsafe { kdma::deallocate_dma_memory(info, self.layout) };
    }
}

unittest::path_mod_test!(tests, "../ut-tests/heap.rs");
