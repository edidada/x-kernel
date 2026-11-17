// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Virtual address space management utilities.
#![allow(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: case (b): legacy memspace has 12 diagnostic \
              sites, too many to judge individually in this migration; new code remains reviewed"
)]
#![no_std]

#[macro_use]
extern crate log;

extern crate alloc;

#[cfg(target_arch = "aarch64")]
mod aarch64_asid;
mod aspace;
pub mod backend;
mod commit;
mod cpu_residency;
mod dma_mapping;
mod fault;
mod iomap;
mod lifetime;
mod vma;

use core::sync::atomic::{AtomicBool, Ordering};

use kaddr_layout::{KERNEL_ASPACE_BASE, KERNEL_ASPACE_SIZE};
use kerrno::{KError, KResult, LinuxResult};
use khal::{
    mem::{MemFlags, MemoryRegion, memory_regions, p2v},
    paging::MappingFlags,
};
use kspin::{SpinNoIrq, SpinNoIrqGuard};
use ksync::{Mutex, MutexGuard};
use lazyinit::LazyInit;
use memaddr::{MemoryAddr, PhysAddr, va};
pub use vmobj::{ObjectInvalidateRequest, VmObjectId};

#[cfg(target_arch = "aarch64")]
pub use self::aarch64_asid::Aarch64UserAsidContext;
pub use self::{
    aspace::{AddrPolicy, AddrSpace, FutexBacking, InvalidateHandle, MmSpace, MremapSource},
    commit::{check_memory_commitment, committed_pages},
    cpu_residency::{MmCpuResidency, MmCpuResidencyRef},
    fault::{FaultContext, FaultInput, FaultOutcome, PageFaultOutcome},
    iomap::{
        DeviceRegion, DeviceRegionIter, IoMapError, device_regions, iomap_device, iounmap,
        register_device_region, register_fixed_device_region,
    },
    lifetime::MmObserver,
    vma::{
        FileMappingInfo, ForkCloneTarget, MsyncPolicy, MsyncWriteback, VmArea, VmAreaSet,
        VmBackingInfo, VmBackingKind, VmInheritance, VmMayPerm, VmPerm, VmRuntimeOps, VmRuntimeRef,
    },
};

/// Process-runtime-only address-space lifetime protocol.
#[doc(hidden)]
pub mod process_lifetime {
    pub use crate::lifetime::{MmPin, MmUserHandle};
}

static KERNEL_ASPACE: LazyInit<KernelLayout> = LazyInit::new();
static KERNEL_PAGE_TABLE_ROOT: LazyInit<PhysAddr> = LazyInit::new();
static IS_RUNTIME_MAPPING: AtomicBool = AtomicBool::new(false);
static IS_BOOT_MAPPING: AtomicBool = AtomicBool::new(true);

/// Closes local-only mapping completion before any secondary activates the root.
/// New mutations remain unavailable until CPU TLB services are ready. Metadata
/// reads do not depend on this boot-to-runtime handoff.
pub fn seal_boot_mapping() {
    IS_BOOT_MAPPING.store(false, Ordering::Release);
}

/// Ends the single-CPU mapping bootstrap before drivers can publish IRQ work.
///
/// Call once after all CPU interrupt services are ready, with local IRQs enabled.
/// Runtime mapping mutations require task context and enabled local IRQs.
/// Read-only metadata access remains IRQ-safe.
pub fn enable_runtime_mapping() {
    assert!(
        karch::local_irq_enabled(),
        "runtime mapping requires enabled IRQs"
    );
    IS_BOOT_MAPPING.store(false, Ordering::Release);
    IS_RUNTIME_MAPPING.store(true, Ordering::Release);
}

pub(crate) fn check_kernel_mapping_context() -> KResult {
    if kiface_irq_context_api::InterruptContextIf::is_in_interrupt_context() {
        return Err(KError::WouldBlock);
    }
    if IS_RUNTIME_MAPPING.load(Ordering::Acquire) {
        if !kiface_task_api::TaskBlockingContextIf::is_sleepable_context() {
            return Err(KError::WouldBlock);
        }
    } else if !IS_BOOT_MAPPING.load(Ordering::Acquire) {
        return Err(KError::WouldBlock);
    }
    Ok(())
}

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

/// Short IRQ-safe PTE/read serialization and task-owned update completion.
///
/// Readers may enter from ordinary IRQ context. Mutations hold a separate task
/// mutex through invalidation, but release the IRQ-off PTE lock before waiting.
pub struct KernelLayout {
    inner: SpinNoIrq<MmSpace>,
    updates: Mutex<Option<khal::paging::KernelTlbFlush>>,
}

/// Read-only access to kernel mapping metadata, with local IRQs saved.
/// Mutation is deliberately unavailable: kernel updates must complete TLBs
/// outside this guard through `KernelLayout`'s mapping methods.
pub struct KernelLayoutReadGuard<'a>(SpinNoIrqGuard<'a, MmSpace>);

impl core::ops::Deref for KernelLayoutReadGuard<'_> {
    type Target = MmSpace;

    fn deref(&self) -> &MmSpace {
        &self.0
    }
}

pub(crate) struct KernelMappingChange {
    result: KResult,
    flush: khal::paging::KernelTlbFlush,
}

pub(crate) struct KernelMappingUpdate<'a> {
    pending: Option<MutexGuard<'a, Option<khal::paging::KernelTlbFlush>>>,
}

impl KernelMappingUpdate<'_> {
    pub(crate) fn finish(&mut self, mut change: KernelMappingChange) -> KResult {
        if let Some(pending) = self.pending.as_mut() {
            if change.flush.try_finish().is_err() {
                // Keep the existing retirement batch, not a second ownership
                // index. Every later update retries it before changing any PTE.
                **pending = Some(change.flush);
                return Err(KError::Io);
            }
        } else {
            // SAFETY: boot controller mappings precede AP startup. After CPU
            // services are ready, enable_runtime_mapping closes this path.
            unsafe { change.flush.finish_boot_local() };
        }
        change.result
    }
}

impl KernelLayout {
    /// Reads kernel mapping state under short IRQ-safe serialization.
    pub fn lock(&self) -> KernelLayoutReadGuard<'_> {
        KernelLayoutReadGuard(self.inner.lock())
    }

    pub(crate) fn begin_update(&self) -> KResult<KernelMappingUpdate<'_>> {
        check_kernel_mapping_context()?;
        let pending = if IS_RUNTIME_MAPPING.load(Ordering::Acquire) {
            let mut pending = self.updates.lock();
            if let Some(flush) = pending.as_mut() {
                flush.try_finish().map_err(|_| KError::Io)?;
                *pending = None;
            }
            Some(pending)
        } else {
            None
        };
        Ok(KernelMappingUpdate { pending })
    }

    /// Changes raw linear kernel attributes, completing TLBs after unlocking.
    /// Requires boot context or a sleepable task. The backing owner retains
    /// the complete range through this synchronous transition.
    ///
    /// # Errors
    /// Returns `WouldBlock` outside the permitted context, validation or
    /// permission errors for an incompatible range, and `Io` if invalidation
    /// is unconfirmed. An error may follow partial PTE changes; retain backing.
    pub fn protect(&self, start: memaddr::VirtAddr, size: usize, flags: MappingFlags) -> KResult {
        let mut update = self.begin_update()?;
        let change = self
            .inner
            .lock()
            .prepare_kernel_linear_protect(start, size, flags)?;
        update.finish(change)
    }

    /// Installs a raw linear mapping and confirms publication outside the PTE lock.
    /// Requires boot context or a sleepable task. The backing owner must retain
    /// the range until a successful matching unmap.
    ///
    /// # Errors
    /// Returns context/range errors, `AlreadyExists` for overlap, `NoMemory`
    /// for page-table allocation failure, or `Io` for unconfirmed invalidation.
    /// Partial installation keeps the VA reserved; retain its backing on error.
    pub fn map_linear(
        &self,
        start: memaddr::VirtAddr,
        physical: PhysAddr,
        size: usize,
        flags: MappingFlags,
    ) -> KResult {
        let mut update = self.begin_update()?;
        let change = self
            .inner
            .lock()
            .prepare_kernel_linear_map(start, physical, size, flags)?;
        update.finish(change)
    }

    /// Removes a raw linear mapping before allowing its VA/backing to be reused.
    /// Requires boot context or a sleepable task.
    ///
    /// # Errors
    /// Returns context/range errors or `Io` when revocation is unconfirmed.
    /// Failed removal keeps its VA reserved and does not authorize backing reuse.
    pub fn unmap(&self, start: memaddr::VirtAddr, size: usize) -> KResult {
        let mut update = self.begin_update()?;
        self.unmap_in_update(&mut update, start, size)
    }

    pub(crate) fn unmap_in_update(
        &self,
        update: &mut KernelMappingUpdate<'_>,
        start: memaddr::VirtAddr,
        size: usize,
    ) -> KResult {
        let change = self.inner.lock().prepare_kernel_linear_unmap(start, size)?;
        update.finish(change)?;
        self.inner.lock().retire_kernel_linear_vma(start, size);
        Ok(())
    }
}

/// Returns the globally unique kernel address space.
pub fn kernel_layout() -> &'static KernelLayout {
    &KERNEL_ASPACE
}

/// Returns the immutable kernel page-table root without locking.
///
/// Valid after primary memory initialization, including on AP startup and in
/// IRQ-off contexts. Runtime mutations do not replace the kernel root.
pub fn kernel_page_table_root() -> PhysAddr {
    *KERNEL_PAGE_TABLE_ROOT
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
    KERNEL_PAGE_TABLE_ROOT.init_once(kernel_layout.page_table_root());
    KERNEL_ASPACE.init_once(KernelLayout {
        inner: SpinNoIrq::new(kernel_layout),
        updates: Mutex::new(None),
    });
    // SAFETY: `kernel_page_table_root()` returns the initialized kernel root
    // page table that must become active before continuing boot.
    unsafe { karch::write_kernel_page_table(kernel_page_table_root().into()) };
    // flush all TLB
    karch::flush_tlb(None);
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

unittest::path_mod_test!(tests_memspace, "../ut-tests/lib.rs");
