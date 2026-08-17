// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel stack allocation — picks the allocator based on `CONFIG_VMAP_STACK`.
//!
//! | Config        | Backend              | Guard page |
//! |---------------|----------------------|------------|
//! | VMAP_STACK=n  | global heap          | none       |
//! | VMAP_STACK=y  | [`crate::vmalloc`]   | 1-page VA hole |

use alloc::vec::Vec;

use kerrno::KResult;
use kstack::KernelStackMapping;

/// Register the strategy chosen by `CONFIG_VMAP_STACK`.
pub fn register() {
    #[cfg(feature = "vmap_stack")]
    {
        kstack::register_ops(alloc_vm, free_vm);
    }
    #[cfg(not(feature = "vmap_stack"))]
    {
        kstack::register_ops(alloc_heap, free_heap);
    }
}

// ── heap ─────────────────────────────────────────────────────────────
#[allow(dead_code)]
fn alloc_heap(size: usize) -> KResult<KernelStackMapping> {
    let layout =
        core::alloc::Layout::from_size_align(size, 16).map_err(|_| kerrno::KError::NoMemory)?;
    // SAFETY: `layout` is validated above; allocation is exclusively owned.
    let ptr = unsafe { alloc::alloc::alloc(layout) };
    if ptr.is_null() {
        return Err(kerrno::KError::NoMemory);
    }
    Ok(KernelStackMapping {
        va: memaddr::VirtAddr::from(ptr as usize),
        size,
        guard_size: 0,
        pages: Vec::new(),
    })
}

#[allow(dead_code)]
fn free_heap(mapping: KernelStackMapping) {
    let layout = core::alloc::Layout::from_size_align(mapping.size, 16)
        .expect("Layout unchanged from alloc");
    // SAFETY: `ptr` and `layout` are the exact pair from `alloc_heap`.
    unsafe { alloc::alloc::dealloc(mapping.va.as_usize() as *mut u8, layout) }
}

// ── vmalloc (1 guard page) ───────────────────────────────────────────

#[allow(dead_code)]
fn alloc_vm(size: usize) -> KResult<KernelStackMapping> {
    // `size` is the requested usable stack; we add one guard page on top.
    let total = size
        .checked_add(memaddr::PAGE_SIZE_4K)
        .ok_or(kerrno::KError::InvalidInput)?;
    let (region, pages) = crate::vmalloc::alloc(total, /* guard_pages */ 1)?;
    Ok(KernelStackMapping {
        va: region.va,
        size: total,
        guard_size: memaddr::PAGE_SIZE_4K,
        pages,
    })
}

#[allow(dead_code)]
fn free_vm(mapping: KernelStackMapping) {
    let guard_pages = mapping.guard_size / memaddr::PAGE_SIZE_4K;
    crate::vmalloc::free(
        crate::vmalloc::VmRegion {
            va: mapping.va,
            size: mapping.size,
        },
        guard_pages,
        mapping.pages,
    );
}
