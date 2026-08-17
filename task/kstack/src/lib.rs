// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel stack resource management.
//!
//! `ktask` calls [`alloc`] / [`free`] here; the actual backing allocator is
//! registered at boot by the memory subsystem (`memspace`). A per-CPU cache
//! of standard-size stacks absorbs the churn of task create/destroy, avoiding
//! repeated vmalloc/vfree + TLB shootdown.
//!
//! This crate deliberately sits between `ktask` and `memspace` so that the
//! task layer never depends on the address-space layer (and vice versa):
//! `ktask` depends on `kstack`, `memspace` registers the backend through
//! [`register_ops`], and neither depends on the other.

#![no_std]
#![warn(missing_docs)]

extern crate alloc;

use alloc::vec::Vec;

use kerrno::KResult;
use kspin::NoPreemptIrqSave;
use lazyinit::LazyInit;
use memaddr::VirtAddr;

/// Handle to an allocated kernel stack. Owns the physical-page handles so
/// `free` can dealloc without walking the page table.
#[derive(Debug)]
pub struct KernelStackMapping {
    /// Base of the whole allocation, including the guard region.
    pub va: VirtAddr,
    /// Total size of the allocation, including the guard region.
    pub size: usize,
    /// Size of the unmapped guard region at the base.
    pub guard_size: usize,
    /// Backend-owned page handles to release on free.
    pub pages: Vec<usize>,
}

impl KernelStackMapping {
    /// One-past-the-end address; the stack grows down from here.
    pub fn top(&self) -> VirtAddr {
        self.va + self.size
    }

    /// Start of the usable (mapped) stack region.
    pub fn usable_start(&self) -> VirtAddr {
        self.va + self.guard_size
    }

    /// Size of the usable (mapped) stack region.
    pub fn usable_size(&self) -> usize {
        self.size - self.guard_size
    }
}

// ── Backing allocator ─────────────────────────────────────────────────

struct StackOps {
    alloc: fn(usize) -> KResult<KernelStackMapping>,
    free: fn(KernelStackMapping),
}

static OPS: LazyInit<StackOps> = LazyInit::new();

/// Registers the backing allocator. Called once at boot by `memspace`.
pub fn register_ops(alloc: fn(usize) -> KResult<KernelStackMapping>, free: fn(KernelStackMapping)) {
    OPS.init_once(StackOps { alloc, free });
}

fn ops() -> &'static StackOps {
    OPS.get()
        .expect("kstack: register_ops() must be called before alloc/free")
}

// ── Per-CPU cache ─────────────────────────────────────────────────────
//
// Only stacks of `KERNEL_STACK_SIZE` are cached — they dominate the
// allocation churn (every user-task create/destroy). Small kernel
// threads (e.g. migration) use custom sizes and bypass the cache
// so a small slot can never be returned for a large request.

const CACHE_SLOTS: usize = 2;

#[percpu::def_percpu]
static CACHE: StackCache = StackCache::new();

struct StackCache {
    slots: [Option<KernelStackMapping>; CACHE_SLOTS],
}

impl StackCache {
    const fn new() -> Self {
        Self {
            slots: [const { None }; CACHE_SLOTS],
        }
    }

    fn take(&mut self) -> Option<KernelStackMapping> {
        self.slots.iter_mut().find_map(|s| s.take())
    }

    fn push(&mut self, m: KernelStackMapping) -> Option<KernelStackMapping> {
        for slot in &mut self.slots {
            if slot.is_none() {
                *slot = Some(m);
                return None;
            }
        }
        Some(m)
    }
}

fn with_cache<R>(f: impl FnOnce(&mut StackCache) -> R) -> R {
    // SAFETY: every caller holds `NoPreemptIrqSave`, so the calling CPU's
    // instance is not aliased by preemption or by an IRQ handler that
    // re-enters `alloc`/`free` on the same CPU.
    f(unsafe { &mut *CACHE.current_ref_mut_raw() })
}

/// Only the standard user-task stack size is cache-worthy.
fn cacheable(size: usize) -> bool {
    size == kbuild_config::KERNEL_STACK_SIZE
}

// ── Public API ────────────────────────────────────────────────────────

/// Zeroes the usable range of a stack taken from the per-CPU cache.
///
/// Scrubbing happens on allocation instead of on free: free can run from an
/// IRQ handler, where a full-stack memset would stretch interrupt latency,
/// while allocation normally runs in task context. It runs after the cache
/// guard is dropped — the mapping is already exclusively owned — so the
/// memset does not extend the IRQ-off section.
fn scrub(m: &KernelStackMapping) {
    let start = m.usable_start().as_usize();
    let len = m.usable_size();
    // SAFETY: the usable range is mapped and owned by this stack, and it is
    // exclusively handed to the caller after the scrub.
    unsafe { core::ptr::write_bytes(start as *mut u8, 0, len) };
}

/// Allocates a kernel stack of the requested size.
///
/// The `NoPreemptIrqSave` guard covers only the per-CPU cache access. The
/// backend runs in the caller's context with IRQs as they arrived: the
/// vmalloc backend takes page-table locks and, on free, may run a synchronous
/// TLB shootdown that spin-waits for remote CPUs — that wait deadlocks if
/// entered with IRQs disabled, so it must never run inside the IRQ-off
/// cache section.
pub fn alloc(size: usize) -> KResult<KernelStackMapping> {
    if cacheable(size) {
        let cached = {
            let _guard = NoPreemptIrqSave::new();
            with_cache(|c| c.take())
        };
        if let Some(m) = cached {
            scrub(&m);
            return Ok(m);
        }
    }
    (ops().alloc)(size)
}

/// Frees a kernel stack, caching standard-size stacks for reuse.
///
/// See [`alloc`] for the locking contract: cache push/pop runs under
/// `NoPreemptIrqSave`, the backend free does not. An IRQ-context caller
/// reaches the backend directly (hardirq/softirq stay nested inside the
/// outer IRQ-off state); the vmalloc backend detects exception context or an
/// early-boot shootdown gate and defers the teardown instead of shooting down
/// TLBs in-place.
pub fn free(mapping: KernelStackMapping) {
    if cacheable(mapping.usable_size()) {
        let overflow = {
            let _guard = NoPreemptIrqSave::new();
            with_cache(|c| c.push(mapping))
        };
        if let Some(m) = overflow {
            (ops().free)(m);
        }
        return;
    }
    (ops().free)(mapping);
}
