// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Virtual address space management utilities.
#![no_std]

#[macro_use]
extern crate log;

extern crate alloc;

#[cfg(target_arch = "aarch64")]
mod aarch64_asid;
mod aspace;
pub mod backend;
mod cpu_residency;
mod fault;
mod iomap;
mod kernel_stack;
mod lifetime;
mod vma;
mod vmalloc;

use kaddr_layout::{KERNEL_ASPACE_BASE, KERNEL_ASPACE_SIZE};
use kerrno::LinuxResult;
use khal::{
    mem::{MemFlags, MemoryRegion, memory_regions, p2v},
    paging::MappingFlags,
};
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;
use memaddr::{MemoryAddr, PhysAddr, va};
pub use vmobj::{ObjectInvalidateRequest, VmObjectId};

#[cfg(target_arch = "aarch64")]
pub use self::aarch64_asid::Aarch64UserAsidContext;
pub use self::{
    aspace::{AddrPolicy, AddrSpace, FutexBacking, InvalidateHandle, MmSpace, MremapSource},
    cpu_residency::{MmCpuResidency, MmCpuResidencyRef},
    fault::{FaultContext, FaultInput, FaultOutcome, PageFaultOutcome},
    iomap::{
        DeviceRegion, DeviceRegionIter, IoMapError, device_regions, iomap_device, iounmap,
        register_device_region, register_fixed_device_region,
    },
    lifetime::MmObserver,
    vma::{
        FileMappingInfo, ForkCloneTarget, MsyncPolicy, MsyncRuntimeResult, VmArea, VmAreaSet,
        VmBackingInfo, VmBackingKind, VmInheritance, VmMayPerm, VmPerm, VmRuntimeOps, VmRuntimeRef,
    },
};

/// Process-runtime-only address-space lifetime protocol.
#[doc(hidden)]
pub mod process_lifetime {
    pub use crate::lifetime::{MmPin, MmUserHandle};
}

/// Global kernel address space.
///
/// Protected by `SpinNoPreempt` (not `SpinNoIrq`): this lock must **never**
/// be acquired from IRQ or exception context.  Doing so would deadlock the
/// current CPU if it already holds the lock.  All current callers are in
/// task context — see `kernel_layout()` for the full list.
static KERNEL_ASPACE: LazyInit<SpinNoPreempt<MmSpace>> = LazyInit::new();

fn mem_to_mapping_flags(f: MemFlags) -> MappingFlags {
    let mut flags = MappingFlags::empty();

    let mappings = [
        (MemFlags::READ, MappingFlags::READ),
        (MemFlags::WRITE, MappingFlags::WRITE),
        (MemFlags::EXECUTE, MappingFlags::EXECUTE),
        (MemFlags::DEVICE, MappingFlags::DEVICE),
        (MemFlags::UNCACHED, MappingFlags::UNCACHED),
    ];

    for (mem_flag, map_flag) in mappings.iter() {
        if f.contains(*mem_flag) {
            flags |= *map_flag;
        }
    }

    flags
}

fn map_memory_region(vmspace: &mut MmSpace, region: MemoryRegion) -> LinuxResult<()> {
    let start = region.paddr.align_down_4k();
    let end = (region.paddr + region.size).align_up_4k();
    let target_va = match region.vaddr {
        Some(v) => v.align_down_4k(),
        None => p2v(start),
    };
    vmspace.map_linear(
        target_va,
        start,
        end - start,
        mem_to_mapping_flags(region.flags),
    )?;
    Ok(())
}

/// Creates a new address space for kernel itself.
pub fn new_kernel_layout() -> LinuxResult<MmSpace> {
    let mut vmspace =
        MmSpace::new_empty_kernel(va!(KERNEL_ASPACE_BASE as _), KERNEL_ASPACE_SIZE as _)?;
    for region in memory_regions() {
        map_memory_region(&mut vmspace, region)?;
    }
    for region in device_regions() {
        let mut mapped = MemoryRegion::new_mmio(region.paddr.as_usize(), region.size, region.name);
        mapped.vaddr = region.vaddr;
        map_memory_region(&mut vmspace, mapped)?;
    }
    Ok(vmspace)
}

/// Returns the globally unique kernel address space.
///
/// # Locking contract
///
/// The lock is `SpinNoPreempt` (IRQs enabled).  **Must never be acquired
/// from IRQ or exception context** — if the current CPU already holds the
/// lock, a nested acquisition will deadlock.
///
/// All current callers are in task context:
///
/// | Caller | Context |
/// |--------|---------|
/// | `vmalloc` alloc/free | task create / destroy / GC |
/// | `iomap_device` / `iounmap` | device init / teardown |
/// | `copy_mappings_from` | fork |
/// | `DmaPageTableIf::protect` | DMA alloc / dealloc |
/// | `write_kernel_text` | static-key patching |
pub fn kernel_layout() -> &'static SpinNoPreempt<MmSpace> {
    &KERNEL_ASPACE
}

/// Returns the root physical address of the kernel page table.
pub fn kernel_page_table_root() -> PhysAddr {
    KERNEL_ASPACE.lock().page_table_root()
}

/// Initializes virtual memory management.
///
/// It mainly sets up the kernel virtual memory address space and recreate a
/// fine-grained kernel page table.
pub fn init_memory_management() {
    info!("Initialize virtual memory management...");
    assert!(
        kalloc::is_page_allocator_ready(),
        "kernel page allocator must be initialized before init_memory_management()"
    );
    let kernel_layout = new_kernel_layout().expect("failed to initialize kernel address space");
    debug!("kernel address space init OK: {:#x?}", kernel_layout);
    KERNEL_ASPACE.init_once(SpinNoPreempt::new(kernel_layout));
    // SAFETY: `kernel_page_table_root()` returns the initialized kernel root
    // page table that must become active before continuing boot.
    unsafe { karch::write_kernel_page_table(kernel_page_table_root().into()) };
    // flush all TLB
    karch::flush_tlb(None);
    // Register kernel stack allocator so ktask can allocate stacks without
    // depending on memspace directly (avoids circular dep with ksync/ktask).
    kernel_stack::register();
}

/// Initializes kernel paging for secondary CPUs.
pub fn init_memory_management_secondary() {
    assert!(
        KERNEL_ASPACE.get().is_some(),
        "kernel address space must be initialized before secondary MMU activation"
    );
    // SAFETY: secondary CPUs switch to the already initialized kernel root page table.
    unsafe { karch::write_kernel_page_table(kernel_page_table_root().into()) };
    // flush all TLB
    karch::flush_tlb(None);
}

#[cfg(unittest)]
mod tests_memspace {
    use khal::{mem::MemFlags, paging::MappingFlags};
    use unittest::def_test;

    use super::mem_to_mapping_flags;

    #[def_test]
    fn test_mem_to_mapping_flags_basic() {
        let flags = MemFlags::READ | MemFlags::WRITE;
        let mapped = mem_to_mapping_flags(flags);
        assert!(mapped.contains(MappingFlags::READ));
        assert!(mapped.contains(MappingFlags::WRITE));
    }

    #[def_test]
    fn test_mem_to_mapping_flags_device_uncached() {
        let flags = MemFlags::DEVICE | MemFlags::UNCACHED;
        let mapped = mem_to_mapping_flags(flags);
        assert!(mapped.contains(MappingFlags::DEVICE));
        assert!(mapped.contains(MappingFlags::UNCACHED));
    }

    #[def_test]
    fn test_mem_to_mapping_flags_empty() {
        let mapped = mem_to_mapping_flags(MemFlags::empty());
        assert!(mapped.is_empty());
    }
}
