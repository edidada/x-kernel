// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Demand faults must reach a sleepable backing provider from user accesses.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use khal::paging::{PageSize, PageTableMut};
use ksync::Mutex;
use ktask::current;
use memaddr::{PAGE_SIZE_4K, VirtAddrRange};
use memspace::{
    FaultContext, ForkCloneTarget, InvalidateHandle, MmSpace, VmBackingInfo, VmRuntimeOps,
    VmRuntimeRef,
    backend::{FaultCompletion, FaultCompletionResult, PrivateBackend},
};
use unittest::def_test;

use super::*;

enum FaultBehavior {
    Normal,
    StagedWrite,
    Stalled,
    Retry,
    NoProgress,
}

struct SleepableBacking {
    pages: PrivateBackend,
    faults: AtomicUsize,
    behavior: FaultBehavior,
}

impl VmRuntimeOps for SleepableBacking {
    fn backing_info(&self) -> VmBackingInfo {
        VmRuntimeOps::backing_info(&self.pages)
    }

    fn map(&self, range: VirtAddrRange, flags: MappingFlags, table: &mut PageTableMut) -> KResult {
        VmRuntimeOps::map(&self.pages, range, flags, table)
    }

    fn unmap(&self, range: VirtAddrRange, table: &mut PageTableMut) -> KResult {
        VmRuntimeOps::unmap(&self.pages, range, table)
    }

    fn on_protect(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        table: &mut PageTableMut,
    ) -> KResult<MappingFlags> {
        VmRuntimeOps::on_protect(&self.pages, range, flags, table)
    }

    fn handle_fault(
        &self,
        ctx: FaultContext,
        flags: MappingFlags,
        table: &mut PageTableMut,
    ) -> FaultCompletionResult {
        // Exercise the same context requirement as IRQ-driven block I/O, without
        // depending on disk timing. A remembered wake permits a deterministic park.
        let mut wait = ktask::future::PreparedTaskWait::prepare()?;
        if khal::context::in_exception_context() {
            return Err(KError::BadState);
        }
        wait.waker().wake_by_ref();
        wait.park();
        self.faults.fetch_add(1, Ordering::Relaxed);
        match self.behavior {
            FaultBehavior::StagedWrite
                if ctx.access_flags().contains(MappingFlags::WRITE)
                    && matches!(
                        table.query(ctx.address()),
                        Err(khal::paging::PagingError::NotMapped)
                    ) =>
            {
                // Model shared-file materialization separately from write permission.
                VmRuntimeOps::handle_fault(
                    &self.pages,
                    FaultContext::new(ctx.address(), MappingFlags::READ),
                    flags - MappingFlags::WRITE,
                    table,
                )
            }
            FaultBehavior::Stalled => Ok(FaultCompletion::from_populate((1, None))),
            FaultBehavior::Retry => Ok(FaultCompletion::cow_conflict_retry()),
            FaultBehavior::NoProgress => Ok(FaultCompletion::from_populate((0, None))),
            _ => VmRuntimeOps::handle_fault(&self.pages, ctx, flags, table),
        }
    }

    fn relocate_for_mremap(
        &self,
        source_range: VirtAddrRange,
        start: VirtAddr,
        new_len: usize,
        id: u64,
        space: &Arc<Mutex<MmSpace>>,
        invalidate: Option<InvalidateHandle>,
    ) -> KResult<Arc<dyn VmRuntimeOps>> {
        VmRuntimeOps::relocate_for_mremap(
            &self.pages,
            source_range,
            start,
            new_len,
            id,
            space,
            invalidate,
        )
    }

    fn clone_for_fork(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        old: &mut PageTableMut,
        new: &mut PageTableMut,
        target: ForkCloneTarget<'_>,
    ) -> KResult<Arc<dyn VmRuntimeOps>> {
        VmRuntimeOps::clone_for_fork(&self.pages, range, flags, old, new, target)
    }
}

#[def_test(user)]
fn user_copies_resolve_sleepable_backing_and_preserve_permissions() {
    let curr = current();
    let space = curr.as_thread().process().address_space().unwrap();
    let start = VirtAddr::from_usize(kaddr_layout::USER_HEAP_BASE + 0x1000_0000);
    let backing = Arc::new(SleepableBacking {
        pages: PrivateBackend::new(start, PageSize::Size4K),
        faults: AtomicUsize::new(0),
        behavior: FaultBehavior::Normal,
    });
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    space
        .lock()
        .map(
            start,
            PAGE_SIZE_4K * 3,
            flags,
            false,
            VmRuntimeRef::new_file_private(backing.clone()),
        )
        .unwrap();

    // The first copy crosses two initially absent pages without touching them
    // from userspace first. The third page is first faulted by copy-to-user.
    let address = start.as_usize() + PAGE_SIZE_4K - 8;
    let mut output = [MaybeUninit::<u8>::uninit(); 16];
    osvm::read_vm_bytes(address as *const u8, &mut output).unwrap();
    assert_eq!(backing.faults.load(Ordering::Relaxed), 2);
    for byte in output {
        // SAFETY: successful read_vm_bytes initialized every output byte.
        assert_eq!(unsafe { byte.assume_init() }, 0);
    }
    let write_address = start.as_usize() + PAGE_SIZE_4K * 3 - 8;
    osvm::write_vm_bytes(write_address as *mut u8, &[0x5a; 8]).unwrap();
    assert_eq!(backing.faults.load(Ordering::Relaxed), 3);
    let mut readback = [MaybeUninit::<u8>::uninit(); 8];
    osvm::read_vm_bytes(write_address as *const u8, &mut readback).unwrap();
    for byte in readback {
        // SAFETY: successful read_vm_bytes initialized every output byte.
        assert_eq!(unsafe { byte.assume_init() }, 0x5a);
    }

    space
        .lock()
        .protect(
            start,
            PAGE_SIZE_4K * 3,
            MappingFlags::USER | MappingFlags::READ,
        )
        .unwrap();
    assert_eq!(
        osvm::write_vm_bytes(write_address as *mut u8, &[1]),
        Err(MemError::NoAccess)
    );
    space.lock().unmap(start, PAGE_SIZE_4K * 3).unwrap();
    assert_eq!(
        osvm::read_vm_bytes(address as *const u8, &mut output),
        Err(MemError::NoAccess)
    );
}

#[def_test(user)]
fn user_atomics_resolve_sleepable_backing_and_preserve_permissions() {
    let curr = current();
    let space = curr.as_thread().process().address_space().unwrap();
    let start = VirtAddr::from_usize(kaddr_layout::USER_HEAP_BASE + 0x1000_0000);
    let load_address = start.as_usize();
    let exchange_address = load_address + PAGE_SIZE_4K;
    for behavior in [FaultBehavior::Normal, FaultBehavior::StagedWrite] {
        let expected_faults = if matches!(behavior, FaultBehavior::StagedWrite) {
            3
        } else {
            2
        };
        let backing = Arc::new(SleepableBacking {
            pages: PrivateBackend::new(start, PageSize::Size4K),
            faults: AtomicUsize::new(0),
            behavior,
        });
        space
            .lock()
            .map(
                start,
                PAGE_SIZE_4K * 2,
                MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                false,
                VmRuntimeRef::new_file_shared(backing.clone()),
            )
            .unwrap();

        let nofault_load = atomic_load_u32_nofault(load_address);
        let nofault_exchange = atomic_cmpxchg_u32_nofault(exchange_address, 0, 7);
        let (masked_load, masked_exchange) = {
            let _irq = IrqSave::new();
            (
                atomic_load_u32(load_address),
                atomic_cmpxchg_u32(exchange_address, 0, 7),
            )
        };
        let faults_before = backing.faults.load(Ordering::Relaxed);
        let load = atomic_load_u32(load_address);
        let exchange = atomic_cmpxchg_u32(exchange_address, 0, 7);
        let mismatch = atomic_cmpxchg_u32(exchange_address, 0, 9);
        space
            .lock()
            .protect(
                start,
                PAGE_SIZE_4K * 2,
                MappingFlags::USER | MappingFlags::READ,
            )
            .unwrap();
        let protected_exchange = atomic_cmpxchg_u32(exchange_address, 7, 9);
        let readback = atomic_load_u32(exchange_address);
        space.lock().unmap(start, PAGE_SIZE_4K * 2).unwrap();

        assert_eq!(nofault_load, Err(MemError::NoAccess));
        assert_eq!(nofault_exchange, Err(MemError::NoAccess));
        assert_eq!(masked_load, Err(MemError::NoAccess));
        assert_eq!(masked_exchange, Err(MemError::NoAccess));
        assert_eq!(faults_before, 0, "non-sleepable access must not invoke the backend");
        assert_eq!(load, Ok(0), "load must resolve faults in task context");
        assert_eq!(exchange, Ok((true, 0)), "retry the whole exchange after a fault");
        assert_eq!(mismatch, Ok((false, 7)));
        assert_eq!(protected_exchange, Err(MemError::NoAccess));
        assert_eq!(readback, Ok(7));
        assert_eq!(backing.faults.load(Ordering::Relaxed), expected_faults);
    }
}

#[def_test(user)]
fn user_copy_handles_staged_write_faults_across_pages() {
    let curr = current();
    let space = curr.as_thread().process().address_space().unwrap();
    let start = VirtAddr::from_usize(kaddr_layout::USER_HEAP_BASE + 0x1000_0000);
    let backing = Arc::new(SleepableBacking {
        pages: PrivateBackend::new(start, PageSize::Size4K),
        faults: AtomicUsize::new(0),
        behavior: FaultBehavior::StagedWrite,
    });
    // Total faults exceed one page's budget, so completed pages must reset it.
    let page_count = MAX_USER_PAGE_FAULT_ATTEMPTS / 2 + 1;
    let mapping_len = PAGE_SIZE_4K * page_count;
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    space
        .lock()
        .map(
            start,
            mapping_len,
            flags,
            false,
            VmRuntimeRef::new_file_shared(backing.clone()),
        )
        .unwrap();
    let address = start.as_usize() + PAGE_SIZE_4K - 8;
    let input = alloc::vec![0x5a; mapping_len - PAGE_SIZE_4K + 8];
    let result = osvm::write_vm_bytes(address as *mut u8, &input);
    let mut output = alloc::vec![MaybeUninit::<u8>::uninit(); input.len()];
    let read_result = osvm::read_vm_bytes(address as *const u8, &mut output);
    space.lock().unmap(start, mapping_len).unwrap();
    assert_eq!(
        result,
        Ok(()),
        "copy must survive materialization then write-permission faults"
    );
    assert_eq!(read_result, Ok(()));
    assert_eq!(backing.faults.load(Ordering::Relaxed), page_count * 2);
    for byte in output {
        // SAFETY: successful read_vm_bytes initialized every output byte.
        assert_eq!(unsafe { byte.assume_init() }, 0x5a);
    }
}

#[def_test(user)]
fn user_copy_bounds_faults_without_progress() {
    let curr = current();
    let space = curr.as_thread().process().address_space().unwrap();
    let start = VirtAddr::from_usize(kaddr_layout::USER_HEAP_BASE + 0x1000_0000);
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    for behavior in [
        FaultBehavior::Stalled,
        FaultBehavior::Retry,
        FaultBehavior::NoProgress,
    ] {
        let expected_attempts = if matches!(behavior, FaultBehavior::NoProgress) {
            1
        } else {
            MAX_USER_PAGE_FAULT_ATTEMPTS
        };
        let backing = Arc::new(SleepableBacking {
            pages: PrivateBackend::new(start, PageSize::Size4K),
            faults: AtomicUsize::new(0),
            behavior,
        });
        space
            .lock()
            .map(
                start,
                PAGE_SIZE_4K,
                flags,
                false,
                VmRuntimeRef::new_file_shared(backing.clone()),
            )
            .unwrap();
        let result = osvm::write_vm_bytes(start.as_usize() as *mut u8, &[1]);
        space.lock().unmap(start, PAGE_SIZE_4K).unwrap();
        assert_eq!(result, Err(MemError::NoAccess));
        assert_eq!(backing.faults.load(Ordering::Relaxed), expected_attempts);
    }
}
