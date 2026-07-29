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
//! Objects are bucketed by size into 9 bins (8..2048 bytes).  The cache
//! works with any [`ByteAllocator`] backend (slab, buddy, TLSF) — it only
//! calls [`ByteAllocator::allocate`] / [`ByteAllocator::deallocate`] through
//! the trait, so all three compile-time allocator choices benefit automatically.
//!
//! # Safety
//!
//! All public functions must be called with local IRQs disabled to prevent
//! reentrancy from interrupt handlers on the same CPU.

use core::alloc::Layout;

use alloc_engine::{AllocResult, ByteAllocator};

// ---------------------------------------------------------------------------
// Bucket definitions
// ---------------------------------------------------------------------------

/// Number of size buckets.
const BUCKET_COUNT: usize = 9;

/// Maximum object size handled by the per-CPU cache.
const MAX_BUCKET_SIZE: usize = 2048;

/// Upper-bound sizes for each bucket, in ascending order.
const BUCKET_SIZES: [usize; BUCKET_COUNT] = [8, 16, 32, 64, 128, 256, 512, 1024, 2048];

/// Max cached entries per bucket (small objects → larger cache).
const BUCKET_COUNTS: [usize; BUCKET_COUNT] = [32, 32, 24, 16, 12, 8, 6, 4, 2];

/// Map a layout to a bucket index. Returns `None` for sizes or alignments
/// exceeding [`MAX_BUCKET_SIZE`].
fn bucket_index(layout: Layout) -> Option<usize> {
    let required = layout.size().max(layout.align());
    if required > MAX_BUCKET_SIZE {
        return None;
    }
    BUCKET_SIZES.iter().position(|&s| required <= s)
}

// ---------------------------------------------------------------------------
// PerCpuBytes — per-CPU cache for one size bucket
// ---------------------------------------------------------------------------

/// Per-CPU cache for a single size bucket.
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

    /// Current number of cached entries.
    #[allow(dead_code)]
    fn len(&self) -> usize {
        self.size as usize
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
    /// Allocates `COUNT * 2 / 3` objects individually, pushes all but
    /// the first into the cache, and returns the first to the caller.
    fn refill(&mut self, balloc: &mut impl ByteAllocator, layout: Layout) -> AllocResult<usize> {
        let nr_to_alloc = COUNT * 2 / 3;

        // Allocate the first object — this is the one we return.
        let first = balloc.allocate(layout).inspect_err(|_| {
            log::warn!("pcb refill(COUNT={}): first alloc failed", COUNT);
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
    /// Frees the given object plus up to `COUNT * 2 / 3` additional
    /// objects popped from the cache.
    fn drain(&mut self, balloc: &mut impl ByteAllocator, free_addr: usize, layout: Layout) {
        let nr_to_drain = COUNT * 2 / 3;

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
// PerCpuByteSet — aggregate for all nine size buckets
// ---------------------------------------------------------------------------

/// Per-CPU byte-object cache: collects per-bucket caches for all 9 sizes.
#[derive(Clone, Copy)]
struct PerCpuByteSet {
    cache_8: PerCpuBytes<32>,
    cache_16: PerCpuBytes<32>,
    cache_32: PerCpuBytes<24>,
    cache_64: PerCpuBytes<16>,
    cache_128: PerCpuBytes<12>,
    cache_256: PerCpuBytes<8>,
    cache_512: PerCpuBytes<6>,
    cache_1024: PerCpuBytes<4>,
    cache_2048: PerCpuBytes<2>,
}

impl PerCpuByteSet {
    const fn new() -> Self {
        Self {
            cache_8: PerCpuBytes::new(),
            cache_16: PerCpuBytes::new(),
            cache_32: PerCpuBytes::new(),
            cache_64: PerCpuBytes::new(),
            cache_128: PerCpuBytes::new(),
            cache_256: PerCpuBytes::new(),
            cache_512: PerCpuBytes::new(),
            cache_1024: PerCpuBytes::new(),
            cache_2048: PerCpuBytes::new(),
        }
    }
}

/// Dispatch a bucket index to the matching named cache field.
///
/// Each arm binds `$c` as `&mut PerCpuBytes<COUNT>`.
macro_rules! with_bucket {
    ($cache:expr, $idx:expr, | $c:ident | $body:expr) => {
        match $idx {
            0 => {
                let $c = &mut $cache.cache_8;
                $body
            }
            1 => {
                let $c = &mut $cache.cache_16;
                $body
            }
            2 => {
                let $c = &mut $cache.cache_32;
                $body
            }
            3 => {
                let $c = &mut $cache.cache_64;
                $body
            }
            4 => {
                let $c = &mut $cache.cache_128;
                $body
            }
            5 => {
                let $c = &mut $cache.cache_256;
                $body
            }
            6 => {
                let $c = &mut $cache.cache_512;
                $body
            }
            7 => {
                let $c = &mut $cache.cache_1024;
                $body
            }
            8 => {
                let $c = &mut $cache.cache_2048;
                $body
            }
            _ => unreachable!("invalid bucket index: {}", $idx),
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
/// exceeds the maximum bucket size.
///
/// # Safety
///
/// Must be called with local IRQs disabled.
#[inline]
pub(crate) fn try_alloc(layout: Layout) -> Option<usize> {
    let idx = bucket_index(layout)?;
    PCB_CACHE.with_current(|cache| with_bucket!(cache, idx, |c| c.try_pop()))
}

/// Try to push a freed object into the per-CPU cache.
///
/// Returns `true` if the object was cached, `false` if the cache is full
/// and the caller should drain.
///
/// # Safety
///
/// Must be called with local IRQs disabled.
#[inline]
pub(crate) fn try_free(layout: Layout, addr: usize) -> bool {
    let Some(idx) = bucket_index(layout) else {
        return false;
    };
    PCB_CACHE.with_current(|cache| with_bucket!(cache, idx, |c| c.try_push(addr).is_ok()))
}

/// Bulk-fill the cache for `layout`'s bucket from the global byte allocator.
///
/// Called when [`try_alloc`] returned `None`.  Allocates a batch of objects
/// from the global allocator, fills the cache, and returns one object.
///
/// Must be called while holding the global balloc lock (IRQs disabled).
pub(crate) fn fill_cache(layout: Layout, balloc: &mut impl ByteAllocator) -> AllocResult<usize> {
    let idx = bucket_index(layout).ok_or(alloc_engine::AllocError::NoMemory)?;
    PCB_CACHE.with_current(|cache| with_bucket!(cache, idx, |c| c.alloc(balloc, layout)))
}

/// Bulk-drain the cache for `layout`'s bucket to the global byte allocator.
///
/// Called when [`try_free`] returned `false`.  Pops up to `COUNT * 2 / 3`
/// additional objects from the cache and returns them all to the global pool.
///
/// Must be called while holding the global balloc lock (IRQs disabled).
pub(crate) fn drain_cache(layout: Layout, balloc: &mut impl ByteAllocator, addr: usize) {
    let Some(idx) = bucket_index(layout) else {
        // Layout exceeds bucket limits; fall back to direct deallocation.
        // SAFETY: addr came from a prior allocation with this layout.
        unsafe {
            balloc.deallocate(core::ptr::NonNull::new_unchecked(addr as *mut u8), layout);
        }
        return;
    };
    PCB_CACHE.with_current(|cache| with_bucket!(cache, idx, |c| c.dealloc(balloc, addr, layout)))
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
    fn test_cache_new_empty() {
        let mut cache = PerCpuBytes::<12>::new();
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.try_pop(), None);
    }

    #[def_test]
    fn test_cache_push_pop() {
        let mut cache = PerCpuBytes::<6>::new();
        assert!(cache.try_push(0x1000).is_ok());
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.try_pop(), Some(0x1000));
        assert_eq!(cache.len(), 0);
    }

    #[def_test]
    fn test_cache_pop_empty_returns_none() {
        let mut cache = PerCpuBytes::<12>::new();
        assert_eq!(cache.try_pop(), None);
    }

    #[def_test]
    fn test_cache_push_full_returns_err() {
        let mut cache = PerCpuBytes::<4>::new();
        for i in 0..4 {
            assert!(cache.try_push(0x1000 * (i + 1)).is_ok());
        }
        assert_eq!(cache.len(), 4);
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
    fn test_bucket_index() {
        use core::alloc::Layout;
        let l = Layout::from_size_align(64, 8).unwrap();
        assert_eq!(bucket_index(l), Some(3)); // BUCKET_SIZES[3] = 64
        let l = Layout::from_size_align(4096, 8).unwrap();
        assert_eq!(bucket_index(l), None); // exceeds MAX_BUCKET_SIZE
    }

    #[def_test]
    fn test_constants() {
        assert_eq!(BUCKET_COUNT, 9);
        assert_eq!(BUCKET_SIZES.len(), BUCKET_COUNT);
        assert_eq!(BUCKET_COUNTS.len(), BUCKET_COUNT);
        for &s in &BUCKET_SIZES {
            assert!(s <= MAX_BUCKET_SIZE);
        }
        for &c in &BUCKET_COUNTS {
            assert!(c >= 1);
        }
    }
}
