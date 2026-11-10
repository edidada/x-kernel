// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The MMIO family: the mapping handle (`devm_ioremap` ownership).

use core::{
    fmt, ptr,
    sync::atomic::{Ordering, fence},
};

use super::ReleaseHook;
use crate::resource::MmioRegion;

/// A mapped MMIO region with checked volatile accessors.
///
/// Auto-unmapped on drop. The accessors carry release/acquire fences and
/// bounds checks, matching readl/writel semantics.
pub struct IoMap {
    vaddr: usize,
    region: MmioRegion,
    release: Option<ReleaseHook>,
}

// A mapping is a shared kernel VA window: reads/writes through it are plain
// volatile accesses, so a second handle imposes no aliasing beyond what the
// device registers already imply.
// SAFETY: same reasoning — the handle is just a (VA, region) pair over an
// already-shared device window; sending/sharing it adds no aliasing hazard.
unsafe impl Send for IoMap {}
// SAFETY: see `Send` above.
unsafe impl Sync for IoMap {}

impl fmt::Debug for IoMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IoMap")
            .field("region", &self.region)
            .finish_non_exhaustive()
    }
}

impl IoMap {
    /// Construct an IoMap over an ALREADY-MAPPED region.
    ///
    /// # Safety
    ///
    /// `vaddr` must point at a valid mapping of at least `region.size`
    /// bytes for the lifetime of this handle, and the caller must own the
    /// mapping (pairing the eventual release). A safe constructor here
    /// would let any `usize` reach the volatile accessors — that is a
    /// soundness hole, not an inconvenience.
    pub unsafe fn new(vaddr: usize, region: MmioRegion, release: Option<ReleaseHook>) -> Self {
        Self {
            vaddr,
            region,
            release,
        }
    }

    pub const fn region(&self) -> MmioRegion {
        self.region
    }

    /// A second handle to the same mapping, WITHOUT the release hook.
    ///
    /// The clone borrows the original's lifetime: it must not outlive the
    /// `IoMap` it came from. Used to hand one consumer-visible view of a
    /// provider-owned window to a published lookup object (e.g. a PHY whose
    /// clock output is a GRF bit inside the provider's mapped region).
    pub fn clone_shared(&self) -> Self {
        Self {
            vaddr: self.vaddr,
            region: self.region,
            release: None,
        }
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.vaddr as *mut u8
    }

    #[inline]
    fn access_ptr(&self, offset: usize, size: usize) -> *mut u8 {
        // Alignment is part of the safe contract: `read_volatile::<u32>`
        // requires 4-byte alignment and "registers are naturally aligned"
        // comments cannot constrain a safe `usize` offset. Misalignment
        // panics here instead of being UB at the volatile op.
        #[expect(
            clippy::panic,
            reason = "CG-021-no-panic-on-reachable-failure: a misaligned MMIO offset is a driver \
                      programming error; a loud immediate failure beats UB at the volatile op"
        )]
        {
            let align = size.max(1);
            if !(self.vaddr + offset).is_multiple_of(align) {
                panic!(
                    "MMIO access misaligned: vaddr {:#x} + offset {:#x} not {}-byte aligned",
                    self.vaddr, offset, align
                );
            }
        }
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: offsets come from region-bounded \
                      register accessors; overflow means a malformed accessor, not runtime data"
        )]
        let end = offset
            .checked_add(size)
            .expect("MMIO access offset overflow");
        assert!(end <= self.region.size, "MMIO access out of bounds");
        // SAFETY: the mapping covers `region.size` bytes and the checked
        // offset is within it.
        unsafe { self.as_ptr().add(offset) }
    }

    #[inline]
    pub fn read8(&self, offset: usize) -> u8 {
        // SAFETY: `access_ptr` bounds-checks the offset against the mapped
        // region and the pointer is aligned for u8 by construction.
        let value = unsafe { ptr::read_volatile(self.access_ptr(offset, 1)) };
        fence(Ordering::Acquire);
        value
    }

    #[inline]
    pub fn read16(&self, offset: usize) -> u16 {
        // SAFETY: bounds-checked by `access_ptr`; MMIO u16 registers are
        // naturally aligned, so the cast keeps alignment.
        let value = unsafe { ptr::read_volatile(self.access_ptr(offset, 2) as *const u16) };
        fence(Ordering::Acquire);
        value
    }

    #[inline]
    pub fn read32(&self, offset: usize) -> u32 {
        // SAFETY: bounds-checked by `access_ptr`; MMIO u32 registers are
        // naturally aligned, so the cast keeps alignment.
        let value = unsafe { ptr::read_volatile(self.access_ptr(offset, 4) as *const u32) };
        fence(Ordering::Acquire);
        value
    }

    #[inline]
    pub fn read64(&self, offset: usize) -> u64 {
        // SAFETY: bounds-checked by `access_ptr`; MMIO u64 registers are
        // naturally aligned, so the cast keeps alignment.
        let value = unsafe { ptr::read_volatile(self.access_ptr(offset, 8) as *const u64) };
        fence(Ordering::Acquire);
        value
    }

    #[inline]
    pub fn write8(&self, offset: usize, value: u8) {
        fence(Ordering::Release);
        // SAFETY: bounds-checked by `access_ptr`; u8 needs no alignment.
        unsafe { ptr::write_volatile(self.access_ptr(offset, 1), value) };
    }

    #[inline]
    pub fn write16(&self, offset: usize, value: u16) {
        fence(Ordering::Release);
        // SAFETY: bounds-checked by `access_ptr`; naturally aligned for u16.
        unsafe { ptr::write_volatile(self.access_ptr(offset, 2) as *mut u16, value) };
    }

    #[inline]
    pub fn write32(&self, offset: usize, value: u32) {
        fence(Ordering::Release);
        // SAFETY: bounds-checked by `access_ptr`; naturally aligned for u32.
        unsafe { ptr::write_volatile(self.access_ptr(offset, 4) as *mut u32, value) };
    }

    #[inline]
    pub fn write64(&self, offset: usize, value: u64) {
        fence(Ordering::Release);
        // SAFETY: bounds-checked by `access_ptr`; naturally aligned for u64.
        unsafe { ptr::write_volatile(self.access_ptr(offset, 8) as *mut u64, value) };
    }
}

impl Drop for IoMap {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}
