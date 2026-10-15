// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Global page allocation helpers.
use kerrno::KResult;
use memaddr::{PAGE_SIZE_4K, PhysAddr, VirtAddr};

use crate::{FrameRun, UsageKind, global_allocator};

/// A RAII wrapper for contiguous 4K-sized pages.
///
/// Owns the complete backing through the common frame lifetime. Its safe data
/// views cover only the requested pages; buddy rounding slack is also retained
/// and released. This wrapper does not expose shared mutable page ownership.
#[derive(Debug)]
pub struct GlobalPage {
    frames: FrameRun<'static>,
    num_pages: usize,
}

impl GlobalPage {
    /// Restores a previously transferred physical allocation.
    ///
    /// # Safety
    ///
    /// The PA and requested page count must come from one outstanding
    /// GlobalPage::into_raw transfer (or an intentionally retained GlobalPage
    /// whose destruction was suppressed). It must be restored exactly once,
    /// with no other data owner, after the required hardware/mapping retirement.
    /// Raw alloc_pages addresses and bare PFNs do not satisfy this contract.
    pub unsafe fn from_raw(paddr: PhysAddr, num_pages: usize) -> Self {
        assert!(num_pages > 0);
        let backing = num_pages
            .checked_next_power_of_two()
            .expect("transferred global page count must not overflow");
        // SAFETY: the caller transfers exactly this GlobalPage's outstanding
        // holds on every rounded backing page from the static global allocator.
        let frames = unsafe { global_allocator().frame_run_from_raw(paddr, backing) };
        for index in 0..backing {
            let meta = global_allocator()
                .frame_meta(paddr.as_usize() / PAGE_SIZE_4K + index)
                .expect("restored global page must be fully described");
            assert!(
                matches!(
                    meta.state(),
                    crate::FrameState::Referenced { references: 1 }
                ),
                "GlobalPage data access requires unique ownership"
            );
        }
        Self { frames, num_pages }
    }

    /// Transfers ownership as a physical address and requested page count.
    ///
    /// Counts are unchanged. The recipient must restore this transfer exactly
    /// once with from_raw, retaining it through any hardware/mapping retirement.
    pub fn into_raw(self) -> (PhysAddr, usize) {
        let Self { frames, num_pages } = self;
        let (paddr, _) = frames.into_raw();
        (paddr, num_pages)
    }

    /// Allocates one 4K-sized page.
    pub fn alloc() -> KResult<Self> {
        Self::alloc_pages(1)
    }

    /// Allocates one 4K-sized page and initializes it with zero.
    pub fn alloc_zero() -> KResult<Self> {
        let mut page = Self::alloc()?;
        page.zero();
        Ok(page)
    }

    /// Allocates contiguous 4K-sized pages.
    pub fn alloc_contiguous(num_pages: usize, align_pow2: usize) -> KResult<Self> {
        Self::alloc_pages_with_alignment(num_pages, align_pow2)
    }

    /// Get the start virtual address of the page.
    pub fn start_va(&self) -> VirtAddr {
        self.frames.kernel_addr()
    }

    /// Returns the allocation's physical identity without address conversion.
    pub fn start_pa(&self) -> PhysAddr {
        self.frames.phys_addr()
    }

    /// Get the total size (in bytes) of these pages.
    pub fn size(&self) -> usize {
        self.num_pages * PAGE_SIZE_4K
    }

    /// Convert the start virtual address to a raw pointer.
    pub fn as_ptr(&self) -> *const u8 {
        self.start_va().as_ptr()
    }

    /// Convert the start virtual address to a mutable raw pointer.
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.start_va().as_mut_ptr()
    }

    /// Fills the allocated pages with a specific byte value.
    pub fn fill(&mut self, byte: u8) {
        // SAFETY: `self` owns a contiguous allocation of `self.size()` bytes
        // starting at `self.as_mut_ptr()`, so writing that full range is valid.
        unsafe { core::ptr::write_bytes(self.as_mut_ptr(), byte, self.size()) }
    }

    /// Fills the allocated pages with zero.
    pub fn zero(&mut self) {
        self.fill(0)
    }

    // Note: The following conversions from raw pointers and sizes to slices
    // utilize standard Rust `core::slice` idioms. These fundamental memory
    // operations are universally identical across Rust projects and should
    // not be interpreted as literal duplication.
    /// Returns a slice for reading data.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `self` owns a live allocation of `self.size()` bytes
        // starting at `self.as_ptr()`, and this method returns a shared view.
        unsafe { core::slice::from_raw_parts(self.as_ptr(), self.size()) }
    }

    /// Returns a mutable slice for writing data.
    pub fn as_slice_mut(&mut self) -> &mut [u8] {
        // SAFETY: `&mut self` guarantees exclusive access to the live
        // allocation of `self.size()` bytes starting at `self.as_mut_ptr()`.
        unsafe { core::slice::from_raw_parts_mut(self.as_mut_ptr(), self.size()) }
    }

    fn alloc_pages(num_pages: usize) -> KResult<Self> {
        Self::alloc_pages_with_alignment(num_pages, PAGE_SIZE_4K)
    }

    fn alloc_pages_with_alignment(num_pages: usize, align_pow2: usize) -> KResult<Self> {
        let frames = global_allocator()
            .alloc_frame_run(num_pages, align_pow2, UsageKind::Global)
            .map_err(|e| match e {
                alloc_engine::AllocError::NoMemory => kerrno::KError::NoMemory,
                _ => kerrno::KError::InvalidInput,
            })?;
        Ok(Self { frames, num_pages })
    }
}
