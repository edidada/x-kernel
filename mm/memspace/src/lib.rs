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
mod kernel_aspace;
mod kernel_stack;
mod kernel_va;
mod lifetime;
mod paging;
mod vma;
mod vmalloc;

use kaddr_layout::{KERNEL_ASPACE_BASE, KERNEL_ASPACE_SIZE};
use kerrno::LinuxResult;
use khal::{
    mem::{MemFlags, MemoryRegion, memory_regions, p2v},
    paging::MappingFlags,
};
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use memaddr::{MemoryAddr, PhysAddr, va};
pub use vmobj::{ObjectInvalidateRequest, VmObjectId};

#[cfg(target_arch = "aarch64")]
pub use self::aarch64_asid::Aarch64UserAsidContext;
use self::kernel_va::{KERNEL_VA_ALLOCATOR, iomap_window, vmalloc_window};
pub use self::{
    aspace::{
        AddrPolicy, AddrSpace, FutexBacking, InvalidateHandle, MmSpace, MremapSource, UserMmSpace,
    },
    cpu_residency::{MmCpuResidency, MmCpuResidencyRef},
    fault::{FaultContext, FaultInput, FaultOutcome, PageFaultOutcome},
    iomap::{
        DeviceRegion, DeviceRegionIter, IoMapError, device_regions, iomap_device, iounmap,
        register_device_region, register_fixed_device_region,
    },
    kernel_aspace::KernelAddressSpace,
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
/// Protected by `SpinNoIrq`: while the lock is held the current CPU's IRQs
/// are disabled, so IRQ-handler paths never re-enter the lock on the same
/// CPU. Dynamic VA reservations use `KERNEL_VA_ALLOCATOR` before this lock.
static KERNEL_ASPACE: LazyInit<SpinNoIrq<KernelAddressSpace>> = LazyInit::new();

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

fn map_memory_region(vmspace: &mut KernelAddressSpace, region: MemoryRegion) -> LinuxResult<()> {
    let start = region.paddr.align_down_4k();
    let end = (region.paddr + region.size).align_up_4k();
    let target_va = match region.vaddr {
        Some(v) => v.align_down_4k(),
        None => p2v(start),
    };
    let mapped_size = end - start;
    let reservation = reserve_fixed_dynamic_mapping(target_va, mapped_size)?;
    if let Err(error) = vmspace.map_linear(
        target_va,
        start,
        mapped_size,
        mem_to_mapping_flags(region.flags),
    ) {
        release_dynamic_mapping(reservation);
        return Err(error.into());
    }
    Ok(())
}

fn reserve_fixed_dynamic_mapping(
    start: memaddr::VirtAddr,
    size: usize,
) -> LinuxResult<Option<kernel_va::KernelVaReservation>> {
    let range = memaddr::VirtAddrRange::try_from_start_size(start, size)
        .ok_or(kerrno::KError::InvalidInput)?;
    let iomap = iomap_window();
    let vmalloc = vmalloc_window();
    let window = if iomap.contains_range(range) {
        Some(iomap)
    } else if vmalloc.contains_range(range) {
        Some(vmalloc)
    } else if range.overlaps(iomap) || range.overlaps(vmalloc) {
        return Err(kerrno::KError::InvalidInput.into());
    } else {
        None
    };
    let Some(window) = window else {
        return Ok(None);
    };
    Ok(Some(
        KERNEL_VA_ALLOCATOR
            .lock()
            .reserve_fixed_in(window, start, size)?,
    ))
}

fn release_dynamic_mapping(reservation: Option<kernel_va::KernelVaReservation>) {
    if let Some(reservation) = reservation {
        KERNEL_VA_ALLOCATOR
            .lock()
            .release(reservation)
            .expect("fixed kernel VA reservation must be released exactly once");
    }
}

/// Creates a new address space for kernel itself.
pub fn new_kernel_layout() -> LinuxResult<KernelAddressSpace> {
    let mut vmspace = KernelAddressSpace::new_empty_kernel(
        va!(KERNEL_ASPACE_BASE as _),
        KERNEL_ASPACE_SIZE as _,
    )?;
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
/// The lock is `SpinNoIrq` (IRQs disabled while held). It is a low-level
/// kernel-space lock: user address spaces have their own per-`UserMmSpace`
/// locks. Dynamic VA allocators reserve an interval before taking this lock;
/// no path acquires `KERNEL_ASPACE` before the shared VA allocator.
pub fn kernel_layout() -> &'static SpinNoIrq<KernelAddressSpace> {
    &KERNEL_ASPACE
}

/// Returns the root physical address of the kernel page table.
pub fn kernel_page_table_root() -> PhysAddr {
    KERNEL_ASPACE.lock().page_table_root()
}

/// Flushes a translation from every CPU using the shared kernel page table.
///
/// Kernel address-space mutation methods commit with `finish_no_flush()` so
/// callers can release `KERNEL_ASPACE` before issuing the synchronous remote
/// shootdown.  The local flush is always performed; the IPI backend suppresses
/// remote delivery until secondary CPUs are ready during early boot.
pub fn flush_kernel_tlb(vaddr: Option<memaddr::VirtAddr>) {
    karch::flush_tlb(vaddr);
    kipi::tlb::flush_all_cpus(vaddr);
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
    KERNEL_ASPACE.init_once(SpinNoIrq::new(kernel_layout));
    // SAFETY: `kernel_page_table_root()` returns the initialized kernel root
    // page table that must become active before continuing boot.
    unsafe { karch::write_kernel_page_table(kernel_page_table_root().into()) };
    // flush all TLB
    karch::flush_tlb(None);
    // Register the kernel stack backend into `kstack`. `ktask` consumes
    // stacks through `kstack` without depending on memspace directly,
    // avoiding a circular dep with the task layer (memspace -> ksync -> ktask).
    vmalloc::init();
    kernel_stack::register();
}

/// Schedules vmalloc frees that were held during early boot.
///
/// The caller must invoke this after `kipi::tlb::mark_all_cpus_started()` so
/// the deferred teardown can complete a remote TLB shootdown before releasing
/// the VA interval and its physical pages.
pub fn complete_deferred_vmalloc_frees() {
    vmalloc::drain_deferred_frees();
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
