// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-CPU byte cache (PCB) for fast lock-free allocation.
//!
//! This module implements per-CPU caches for byte-sized allocation,
//! analogous to [`super::pcp`] (Per-CPU Page sets) for page allocation.
//! It reduces contention on the global [`ByteAllocator`](alloc_engine::ByteAllocator)
//! lock by caching freed objects per-CPU and bulk-filling / bulk-draining
//! when the cache is empty or full.
//!
//! Objects are bucketed by [`SizeClass`](alloc_engine::SizeClass), reusing
//! the same size-class model used by the slab allocator.  Each class gets
//! its own per-CPU LIFO cache.
//!
//! # Safety
//!
//! All public functions must be called with local IRQs disabled to prevent
//! reentrancy from interrupt handlers on the same CPU.

use core::alloc::Layout;

use alloc_engine::{AllocResult, ByteAllocator, SizeClass};

/// Return the canonical [`Layout`] for a size class.
///
/// Every object in a given class is allocated and freed with this layout,
/// regardless of the caller's original layout.  This guarantees a single
/// (size, align) pair per bucket, so that allocators which rely on the
/// layout for block-header lookup (TLSF) or alignment invariants remain
/// correct even when different callers route to the same class.
fn canonical_layout(class: SizeClass) -> Layout {
    let size = class.size();
    // SAFETY: size is a power of two in [8, 2048]; alignment equal to
    // size always succeeds.
    unsafe { Layout::from_size_align_unchecked(size, size) }
}

// ---------------------------------------------------------------------------
// PerCpuBytes — per-CPU cache for one size class
// ---------------------------------------------------------------------------

/// Per-CPU cache for a single size class.
///
/// Stores freed object addresses in a simple LIFO array.
/// Uses the same pattern as [`super::pcp::PerCpuPages`].
#[derive(Clone, Copy)]
struct PerCpuBytes<const COUNT: usize> {
    addrs: [usize; COUNT],
    size: u8,
}

impl<const COUNT: usize> PerCpuBytes<COUNT> {
    const fn new() -> Self {
        Self {
            addrs: [0; COUNT],
            size: 0,
        }
    }

    /// Pop a cached address. Returns `None` if empty.
    fn try_pop(&mut self) -> Option<usize> {
        if self.size == 0 {
            return None;
        }
        self.size -= 1;
        Some(self.addrs[self.size as usize])
    }

    /// Push a freed address. Returns `Err(addr)` if full.
    fn try_push(&mut self, addr: usize) -> Result<(), usize> {
        if self.size as usize >= COUNT {
            return Err(addr);
        }
        self.addrs[self.size as usize] = addr;
        self.size += 1;
        Ok(())
    }

    /// Allocate one object, possibly refilling from the global byte allocator.
    ///
    /// Double-checks the cache before refilling: an interrupt may have
    /// filled the cache between the outer `try_alloc` and this call.
    fn alloc(&mut self, balloc: &mut impl ByteAllocator, layout: Layout) -> AllocResult<usize> {
        if let Some(addr) = self.try_pop() {
            return Ok(addr);
        }
        self.refill(balloc, layout)
    }

    /// Free one object, possibly draining to the global byte allocator when full.
    fn dealloc(&mut self, balloc: &mut impl ByteAllocator, addr: usize, layout: Layout) {
        if self.try_push(addr).is_err() {
            self.drain(balloc, addr, layout);
        }
    }

    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    /// Bulk-fill the cache from the global allocator.
    ///
    /// Allocates `COUNT * 3 / 4` objects, pushes all but the first
    /// into the cache, and returns the first to the caller.
    fn refill(&mut self, balloc: &mut impl ByteAllocator, layout: Layout) -> AllocResult<usize> {
        let nr_to_alloc = COUNT * 3 / 4;

        // Allocate the first object — this is the one we return.
        let first = balloc.allocate(layout).inspect_err(|_| {
            log::debug!(
                "pcb refill(COUNT={}): first alloc failed (heap may expand)",
                COUNT
            );
        })?;

        // Push the tail objects into the cache.
        // (The cache was empty on entry, so these pushes always succeed.)
        for _ in 1..nr_to_alloc {
            match balloc.allocate(layout) {
                Ok(obj) => {
                    let _ = self.try_push(obj.as_ptr() as usize);
                }
                Err(_) => {
                    // Partial fill is acceptable — stop early.
                    break;
                }
            }
        }

        Ok(first.as_ptr() as usize)
    }

    /// Bulk-drain entries from the cache back to the global allocator.
    ///
    /// Frees the given object plus up to `COUNT * 3 / 4` additional
    /// objects popped from the cache.
    fn drain(&mut self, balloc: &mut impl ByteAllocator, free_addr: usize, layout: Layout) {
        let nr_to_drain = COUNT * 3 / 4;

        // Free the passed-in object.
        // SAFETY: `free_addr` came from a prior successful allocation
        // with the same layout from this allocator.
        unsafe {
            balloc.deallocate(
                core::ptr::NonNull::new_unchecked(free_addr as *mut u8),
                layout,
            );
        }

        // Pop and free additional entries from the cache.
        for _ in 0..nr_to_drain {
            if let Some(addr) = self.try_pop() {
                // SAFETY: each cached addr came from a prior allocation
                // with the same layout.
                unsafe {
                    balloc.deallocate(core::ptr::NonNull::new_unchecked(addr as *mut u8), layout);
                }
            } else {
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PerCpuByteSet — aggregate for all nine size classes
// ---------------------------------------------------------------------------

/// Per-CPU byte cache: collects per-class caches for all 9 [`SizeClass`] values.
#[derive(Clone, Copy)]
struct PerCpuByteSet {
    class_8: PerCpuBytes<48>,
    class_16: PerCpuBytes<48>,
    class_32: PerCpuBytes<40>,
    class_64: PerCpuBytes<32>,
    class_128: PerCpuBytes<24>,
    class_256: PerCpuBytes<16>,
    class_512: PerCpuBytes<10>,
    class_1024: PerCpuBytes<8>,
    class_2048: PerCpuBytes<4>,
}

impl PerCpuByteSet {
    const fn new() -> Self {
        Self {
            class_8: PerCpuBytes::new(),
            class_16: PerCpuBytes::new(),
            class_32: PerCpuBytes::new(),
            class_64: PerCpuBytes::new(),
            class_128: PerCpuBytes::new(),
            class_256: PerCpuBytes::new(),
            class_512: PerCpuBytes::new(),
            class_1024: PerCpuBytes::new(),
            class_2048: PerCpuBytes::new(),
        }
    }
}

/// Dispatch a [`SizeClass`] to the matching named cache field.
macro_rules! with_class {
    ($cache:expr, $class:expr, | $c:ident | $body:expr) => {
        match $class {
            SizeClass::Bytes8 => {
                let $c = &mut $cache.class_8;
                $body
            }
            SizeClass::Bytes16 => {
                let $c = &mut $cache.class_16;
                $body
            }
            SizeClass::Bytes32 => {
                let $c = &mut $cache.class_32;
                $body
            }
            SizeClass::Bytes64 => {
                let $c = &mut $cache.class_64;
                $body
            }
            SizeClass::Bytes128 => {
                let $c = &mut $cache.class_128;
                $body
            }
            SizeClass::Bytes256 => {
                let $c = &mut $cache.class_256;
                $body
            }
            SizeClass::Bytes512 => {
                let $c = &mut $cache.class_512;
                $body
            }
            SizeClass::Bytes1024 => {
                let $c = &mut $cache.class_1024;
                $body
            }
            SizeClass::Bytes2048 => {
                let $c = &mut $cache.class_2048;
                $body
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Per-CPU storage
// ---------------------------------------------------------------------------

#[percpu::def_percpu]
static PCB_CACHE: PerCpuByteSet = PerCpuByteSet::new();

// ---------------------------------------------------------------------------
// Public module API
// ---------------------------------------------------------------------------

/// Try to pop an object for `layout` from the per-CPU cache.
///
/// Returns `None` if the corresponding cache is empty, or if the layout
/// exceeds the maximum size class.
///
/// # Safety
///
/// Must be called with local IRQs disabled to prevent interrupt handlers
/// on the same CPU from re-entering the per-CPU cache and creating aliased
/// `&mut` references.
#[inline]
pub(crate) unsafe fn try_alloc(layout: Layout) -> Option<usize> {
    let class = SizeClass::from_layout(layout)?;
    PCB_CACHE.with_current(|cache| with_class!(cache, class, |c| c.try_pop()))
}

/// Try to push a freed object into the per-CPU cache.
///
/// Returns `true` if the object was cached, `false` if the cache is full
/// and the caller should drain.
///
/// # Safety
///
/// Must be called with local IRQs disabled to prevent interrupt handlers
/// on the same CPU from re-entering the per-CPU cache and creating aliased
/// `&mut` references.
#[inline]
pub(crate) unsafe fn try_free(layout: Layout, addr: usize) -> bool {
    let Some(class) = SizeClass::from_layout(layout) else {
        return false;
    };
    PCB_CACHE.with_current(|cache| with_class!(cache, class, |c| c.try_push(addr).is_ok()))
}

/// Bulk-fill the cache for `layout`'s size class from the global byte allocator.
///
/// Called when [`try_alloc`] returned `None`.  Uses the class's canonical
/// layout so that all objects in a bucket share the same (size, align).
///
/// # Safety
///
/// Must be called while holding the global balloc lock (IRQs disabled).
pub(crate) unsafe fn fill_cache(
    layout: Layout,
    balloc: &mut impl ByteAllocator,
) -> AllocResult<usize> {
    let class = SizeClass::from_layout(layout).ok_or(alloc_engine::AllocError::NoMemory)?;
    let canonical = canonical_layout(class);
    PCB_CACHE.with_current(|cache| with_class!(cache, class, |c| c.alloc(balloc, canonical)))
}

/// Bulk-drain the cache for `layout`'s size class to the global byte allocator.
///
/// Called when [`try_free`] returned `false`.  Uses the class's canonical
/// layout so deallocation matches the layout that was used during allocation.
///
/// # Safety
///
/// Must be called while holding the global balloc lock (IRQs disabled).
pub(crate) unsafe fn drain_cache(layout: Layout, balloc: &mut impl ByteAllocator, addr: usize) {
    let Some(class) = SizeClass::from_layout(layout) else {
        // Layout exceeds size-class limits; fall back to direct deallocation
        // using the caller's original layout (no cache involvement).
        // SAFETY: addr came from a prior allocation with this layout.
        unsafe {
            balloc.deallocate(core::ptr::NonNull::new_unchecked(addr as *mut u8), layout);
        }
        return;
    };
    let canonical = canonical_layout(class);
    PCB_CACHE
        .with_current(|cache| with_class!(cache, class, |c| c.dealloc(balloc, addr, canonical)))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(unittest)]
#[allow(missing_docs)]
mod tests {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_cache_push_pop() {
        let mut cache = PerCpuBytes::<6>::new();
        assert!(cache.try_push(0x1000).is_ok());
        assert_eq!(cache.try_pop(), Some(0x1000));
        assert_eq!(cache.try_pop(), None);
    }

    #[def_test]
    fn test_cache_push_full_returns_err() {
        let mut cache = PerCpuBytes::<4>::new();
        for i in 0..4 {
            assert!(cache.try_push(0x1000 * (i + 1)).is_ok());
        }
        assert_eq!(cache.try_push(0xFFFF_0000), Err(0xFFFF_0000));
    }

    #[def_test]
    fn test_cache_lifo_order() {
        let mut cache = PerCpuBytes::<12>::new();
        cache.try_push(0x1000).unwrap();
        cache.try_push(0x2000).unwrap();
        cache.try_push(0x3000).unwrap();
        assert_eq!(cache.try_pop(), Some(0x3000));
        assert_eq!(cache.try_pop(), Some(0x2000));
        assert_eq!(cache.try_pop(), Some(0x1000));
    }

    #[def_test]
    fn test_size_class_routing() {
        let l = Layout::from_size_align(64, 8).unwrap();
        assert_eq!(SizeClass::from_layout(l), Some(SizeClass::Bytes64));
        let l = Layout::from_size_align(4096, 8).unwrap();
        assert_eq!(SizeClass::from_layout(l), None);
    }

    #[def_test]
    fn test_canonical_layout() {
        for &class in &SizeClass::ALL {
            let layout = canonical_layout(class);
            assert_eq!(layout.size(), class.size());
            assert_eq!(layout.align(), class.size());
        }
    }
}
