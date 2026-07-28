// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel stack allocation — dispatch layer.
//!
//! `ktask` calls [`alloc`] / [`free`] here.  The actual backing allocator is
//! registered at boot by the memory-management subsystem based on
//! `CONFIG_VMAP_STACK`.

use alloc::vec::Vec;

use kerrno::KResult;
use lazyinit::LazyInit;
use memaddr::VirtAddr;

/// Handle to an allocated kernel stack.  Owns the physical-page handles
/// so `free` can dealloc without walking the page table.
#[derive(Debug)]
pub struct KernelStackMapping {
    pub va: VirtAddr,
    pub size: usize,
    pub guard_size: usize,
    /// Direct-map VAs of allocated physical pages (empty for heap path).
    pub pages: Vec<usize>,
}

impl KernelStackMapping {
    pub fn top(&self) -> VirtAddr {
        self.va + self.size
    }

    pub fn usable_start(&self) -> VirtAddr {
        self.va + self.guard_size
    }

    pub fn usable_size(&self) -> usize {
        self.size - self.guard_size
    }
}

struct StackOps {
    alloc: unsafe fn(usize) -> KResult<KernelStackMapping>,
    free: unsafe fn(KernelStackMapping),
}

static OPS: LazyInit<StackOps> = LazyInit::new();

pub fn register_ops(
    alloc: unsafe fn(usize) -> KResult<KernelStackMapping>,
    free: unsafe fn(KernelStackMapping),
) {
    OPS.init_once(StackOps { alloc, free });
}

fn ops() -> &'static StackOps {
    OPS.get()
        .expect("khal::stack: register_ops() must be called before alloc/free")
}

pub fn alloc(size: usize) -> KResult<KernelStackMapping> {
    // SAFETY: ops registered at boot before any task stack is allocated.
    unsafe { (ops().alloc)(size) }
}

pub fn free(mapping: KernelStackMapping) {
    // SAFETY: mapping was returned by `alloc`; ops registered at boot.
    unsafe { (ops().free)(mapping) }
}
