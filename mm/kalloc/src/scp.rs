// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-CPU slab object cache (SCP) for fast lock-free slab allocation.
//!
//! This module implements per-CPU caches for each slab size class,
//! analogous to the per-CPU page set ([`super::pcp`]) for page allocation.
//! It reduces contention on the global [`SlabByteAllocator`](alloc_engine::SlabByteAllocator)
//! lock by caching freed objects per-CPU and bulk-filling / bulk-draining
//! when the cache is empty or full.
//!
//! # Safety
//!
//! All public functions must be called with local IRQs disabled to prevent
//! reentrancy from interrupt handlers on the same CPU.

use core::alloc::Layout;

use alloc_engine::{AllocResult, ByteAllocator, SizeClass, SlabByteAllocator};

// ---------------------------------------------------------------------------
// Cache sizes (number of entries per size-class cache)
// ---------------------------------------------------------------------------

/// 8-byte objects — highest allocation frequency.
const C8: usize = 32;
/// 16-byte objects.
const C16: usize = 32;
/// 32-byte objects.
const C32: usize = 24;
/// 64-byte objects.
const C64: usize = 16;
/// 128-byte objects.
const C128: usize = 12;
/// 256-byte objects.
const C256: usize = 8;
/// 512-byte objects.
const C512: usize = 6;
/// 1024-byte objects.
const C1024: usize = 4;
/// 2048-byte objects — lowest frequency.
const C2048: usize = 2;

// ---------------------------------------------------------------------------
// SlabObjectCache — per-CPU cache for one size class
// ---------------------------------------------------------------------------

/// Per-CPU cache for a single slab size class.
///
/// Stores freed object addresses in a simple LIFO array.
/// Uses the same pattern as [`super::pcp::PerCpuPages`].
#[derive(Clone, Copy)]
struct SlabObjectCache<const COUNT: usize> {
    addrs: [usize; COUNT],
    size: u8,
}

impl<const COUNT: usize> SlabObjectCache<COUNT> {
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

    /// Allocate one object, possibly refilling from the global slab allocator.
    ///
    /// Double-checks the cache before refilling: an interrupt may have
    /// consumed the last entry between the outer `try_alloc` and this
    /// `fill_cache` call.
    fn alloc(&mut self, balloc: &mut SlabByteAllocator, layout: Layout) -> AllocResult<usize> {
        if let Some(addr) = self.try_pop() {
            return Ok(addr);
        }
        self.refill(balloc, layout)
    }

    /// Free one object, possibly draining to the global slab allocator when full.
    fn dealloc(&mut self, balloc: &mut SlabByteAllocator, addr: usize, layout: Layout) {
        if self.try_push(addr).is_err() {
            self.drain(balloc, addr, layout);
        }
    }

    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    /// Bulk-fill the cache from the global slab allocator.
    ///
    /// Allocates `COUNT * 2 / 3` objects individually, pushes all but
    /// the first into the cache, and returns the first to the caller.
    fn refill(&mut self, balloc: &mut SlabByteAllocator, layout: Layout) -> AllocResult<usize> {
        let nr_to_alloc = COUNT * 2 / 3;

        // Allocate the first object — this is the one we return.
        let first = balloc.allocate(layout).inspect_err(|_| {
            log::warn!(
                "scp refill(COUNT={}): balloc.allocate first object failed",
                COUNT,
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

    /// Bulk-drain entries from the cache back to the global slab allocator.
    ///
    /// Frees the given object plus up to `COUNT * 2 / 3` additional
    /// objects popped from the cache. Unlike PCP's drain which merges
    /// contiguous ranges, slab drain simply returns each object to the
    /// global slab freelist individually.
    fn drain(&mut self, balloc: &mut SlabByteAllocator, free_addr: usize, layout: Layout) {
        let nr_to_drain = COUNT * 2 / 3;

        // Free the passed-in object.
        // SAFETY: `free_addr` came from a prior successful allocation
        // with the same layout from this slab allocator.
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
                // with the same layout from this slab allocator.
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
// PerCpuSlabCache — aggregate for all nine size classes
// ---------------------------------------------------------------------------

/// Per-CPU slab cache: collects per-size-class caches for all 9 slab sizes.
#[derive(Clone, Copy)]
struct PerCpuSlabCache {
    cache_8: SlabObjectCache<C8>,
    cache_16: SlabObjectCache<C16>,
    cache_32: SlabObjectCache<C32>,
    cache_64: SlabObjectCache<C64>,
    cache_128: SlabObjectCache<C128>,
    cache_256: SlabObjectCache<C256>,
    cache_512: SlabObjectCache<C512>,
    cache_1024: SlabObjectCache<C1024>,
    cache_2048: SlabObjectCache<C2048>,
}

impl PerCpuSlabCache {
    const fn new() -> Self {
        Self {
            cache_8: SlabObjectCache::new(),
            cache_16: SlabObjectCache::new(),
            cache_32: SlabObjectCache::new(),
            cache_64: SlabObjectCache::new(),
            cache_128: SlabObjectCache::new(),
            cache_256: SlabObjectCache::new(),
            cache_512: SlabObjectCache::new(),
            cache_1024: SlabObjectCache::new(),
            cache_2048: SlabObjectCache::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Per-CPU storage
// ---------------------------------------------------------------------------

#[percpu::def_percpu]
static SCP_CACHE: PerCpuSlabCache = PerCpuSlabCache::new();

// ---------------------------------------------------------------------------
// Public module API
// ---------------------------------------------------------------------------

/// Try to pop an object for `layout` from the per-CPU cache.
///
/// Returns `None` if the corresponding cache is empty, or if the layout
/// exceeds slab limits (size or alignment > 2048).
///
/// # Safety
///
/// Must be called with local IRQs disabled.
#[inline]
pub(crate) fn try_alloc(layout: Layout) -> Option<usize> {
    let class = SizeClass::from_layout(layout)?;
    SCP_CACHE.with_current(|cache| match class {
        SizeClass::Bytes8 => cache.cache_8.try_pop(),
        SizeClass::Bytes16 => cache.cache_16.try_pop(),
        SizeClass::Bytes32 => cache.cache_32.try_pop(),
        SizeClass::Bytes64 => cache.cache_64.try_pop(),
        SizeClass::Bytes128 => cache.cache_128.try_pop(),
        SizeClass::Bytes256 => cache.cache_256.try_pop(),
        SizeClass::Bytes512 => cache.cache_512.try_pop(),
        SizeClass::Bytes1024 => cache.cache_1024.try_pop(),
        SizeClass::Bytes2048 => cache.cache_2048.try_pop(),
    })
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
    let Some(class) = SizeClass::from_layout(layout) else {
        return false;
    };
    SCP_CACHE.with_current(|cache| match class {
        SizeClass::Bytes8 => cache.cache_8.try_push(addr).is_ok(),
        SizeClass::Bytes16 => cache.cache_16.try_push(addr).is_ok(),
        SizeClass::Bytes32 => cache.cache_32.try_push(addr).is_ok(),
        SizeClass::Bytes64 => cache.cache_64.try_push(addr).is_ok(),
        SizeClass::Bytes128 => cache.cache_128.try_push(addr).is_ok(),
        SizeClass::Bytes256 => cache.cache_256.try_push(addr).is_ok(),
        SizeClass::Bytes512 => cache.cache_512.try_push(addr).is_ok(),
        SizeClass::Bytes1024 => cache.cache_1024.try_push(addr).is_ok(),
        SizeClass::Bytes2048 => cache.cache_2048.try_push(addr).is_ok(),
    })
}

/// Bulk-fill the cache for `layout`'s size class from the global slab allocator.
///
/// Called when [`try_alloc`] returned `None`.  Allocates a batch of objects
/// from the global slab, fills the cache, and returns one object.
///
/// Must be called while holding the global balloc lock (IRQs disabled).
pub(crate) fn fill_cache(layout: Layout, balloc: &mut SlabByteAllocator) -> AllocResult<usize> {
    let class = SizeClass::from_layout(layout).ok_or(alloc_engine::AllocError::NoMemory)?;
    SCP_CACHE.with_current(|cache| match class {
        SizeClass::Bytes8 => cache.cache_8.alloc(balloc, layout),
        SizeClass::Bytes16 => cache.cache_16.alloc(balloc, layout),
        SizeClass::Bytes32 => cache.cache_32.alloc(balloc, layout),
        SizeClass::Bytes64 => cache.cache_64.alloc(balloc, layout),
        SizeClass::Bytes128 => cache.cache_128.alloc(balloc, layout),
        SizeClass::Bytes256 => cache.cache_256.alloc(balloc, layout),
        SizeClass::Bytes512 => cache.cache_512.alloc(balloc, layout),
        SizeClass::Bytes1024 => cache.cache_1024.alloc(balloc, layout),
        SizeClass::Bytes2048 => cache.cache_2048.alloc(balloc, layout),
    })
}

/// Bulk-drain the cache for `layout`'s size class to the global slab allocator.
///
/// Called when [`try_free`] returned `false`.  Pops up to `COUNT * 2 / 3`
/// additional objects from the cache and returns them all to the global slab.
///
/// Must be called while holding the global balloc lock (IRQs disabled).
pub(crate) fn drain_cache(layout: Layout, balloc: &mut SlabByteAllocator, addr: usize) {
    let Some(class) = SizeClass::from_layout(layout) else {
        // Layout exceeds slab limits; fall back to direct deallocation.
        // SAFETY: addr came from a prior allocation with this layout.
        unsafe {
            balloc.deallocate(core::ptr::NonNull::new_unchecked(addr as *mut u8), layout);
        }
        return;
    };
    SCP_CACHE.with_current(|cache| match class {
        SizeClass::Bytes8 => cache.cache_8.dealloc(balloc, addr, layout),
        SizeClass::Bytes16 => cache.cache_16.dealloc(balloc, addr, layout),
        SizeClass::Bytes32 => cache.cache_32.dealloc(balloc, addr, layout),
        SizeClass::Bytes64 => cache.cache_64.dealloc(balloc, addr, layout),
        SizeClass::Bytes128 => cache.cache_128.dealloc(balloc, addr, layout),
        SizeClass::Bytes256 => cache.cache_256.dealloc(balloc, addr, layout),
        SizeClass::Bytes512 => cache.cache_512.dealloc(balloc, addr, layout),
        SizeClass::Bytes1024 => cache.cache_1024.dealloc(balloc, addr, layout),
        SizeClass::Bytes2048 => cache.cache_2048.dealloc(balloc, addr, layout),
    });
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
        let mut cache = SlabObjectCache::<12>::new();
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.try_pop(), None);
    }

    #[def_test]
    fn test_cache_push_pop() {
        let mut cache = SlabObjectCache::<6>::new();
        assert!(cache.try_push(0x1000).is_ok());
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.try_pop(), Some(0x1000));
        assert_eq!(cache.len(), 0);
    }

    #[def_test]
    fn test_cache_pop_empty_returns_none() {
        let mut cache = SlabObjectCache::<12>::new();
        assert_eq!(cache.try_pop(), None);
    }

    #[def_test]
    fn test_cache_push_full_returns_err() {
        let mut cache = SlabObjectCache::<4>::new();
        for i in 0..4 {
            assert!(cache.try_push(0x1000 * (i + 1)).is_ok());
        }
        assert_eq!(cache.len(), 4);
        assert_eq!(cache.try_push(0xFFFF_0000), Err(0xFFFF_0000));
    }

    #[def_test]
    fn test_cache_lifo_order() {
        let mut cache = SlabObjectCache::<12>::new();
        cache.try_push(0x1000).unwrap();
        cache.try_push(0x2000).unwrap();
        cache.try_push(0x3000).unwrap();
        assert_eq!(cache.try_pop(), Some(0x3000));
        assert_eq!(cache.try_pop(), Some(0x2000));
        assert_eq!(cache.try_pop(), Some(0x1000));
    }

    #[def_test]
    fn test_constants_reasonable() {
        assert!(C8 >= 4);
        assert!(C16 >= 4);
        assert!(C32 >= 2);
        assert!(C64 >= 2);
        assert!(C128 >= 2);
        assert!(C256 >= 2);
        assert!(C512 >= 2);
        assert!(C1024 >= 1);
        assert!(C2048 >= 1);
    }
}
