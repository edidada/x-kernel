// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Split private anonymous first-touch preparation from its locked commit.

use kerrno::KResult;
use khal::paging::{MappingFlags, PagingError};
use ksync::Mutex;
use memaddr::{MemoryAddr, VirtAddrRange};

use super::MmSpace;
use crate::{
    FaultInput, PageFaultOutcome, VmBackingInfo, VmBackingKind, VmRuntimeRef,
    backend::{
        BackendOps, FaultCompletion,
        private::{PreparedPrivatePage, PrivateBackend},
    },
};

struct FirstTouchFault {
    input: FaultInput,
    range: VirtAddrRange,
    flags: MappingFlags,
    max_flags: MappingFlags,
    backing: VmBackingInfo,
    page_offset: u64,
    backend: PrivateBackend,
}

impl FirstTouchFault {
    fn begin(space: &mut MmSpace, input: FaultInput) -> Result<Self, PageFaultOutcome> {
        if !space.range.contains(input.address()) {
            return Err(PageFaultOutcome::Unmapped);
        }
        let Some(vma) = space.find_vma(input.address()) else {
            return Err(PageFaultOutcome::Unmapped);
        };
        if !vma.allows_fault(input.access_flags()) {
            return Err(PageFaultOutcome::AccessDenied);
        }
        if !matches!(vma.backing().kind(), VmBackingKind::AnonymousPrivate { .. }) {
            let vma = vma.clone();
            return Err(vma.handle_fault(space, input.into_context()));
        }
        let Some(runtime) = space.runtime_for_vma(vma) else {
            return Err(PageFaultOutcome::Unmapped);
        };
        let VmRuntimeRef::AnonPrivate(backend) = runtime else {
            // Adapters may report anonymous backing while implementing the
            // fault policy through VmRuntimeOps. Preserve their dispatch.
            let vma = vma.clone();
            return Err(vma.handle_fault(space, input.into_context()));
        };
        let address = input.address().align_down(BackendOps::page_size(&backend));
        let observed = space.pgtbl.query(address);
        if matches!(observed, Err(PagingError::NotMapped)) && backend.has_empty_slot(address) {
            return Ok(Self {
                input,
                range: vma.range(),
                flags: vma.flags(),
                max_flags: vma.max_flags(),
                backing: vma.backing(),
                page_offset: vma.page_offset(),
                backend,
            });
        }
        // Keep the existing fault on its original locked path without walking
        // the VMA tree or page table a second time, especially for COW faults.
        let flags = vma.flags();
        let result = backend.handle_fault_observed(
            vma.fault_context(input.into_context()),
            flags,
            &mut space.pgtbl.modify(),
            observed,
        );
        Err(space.finish_backend_fault(input.address(), flags, result))
    }

    fn matches_current_mapping(&self, space: &MmSpace) -> bool {
        let Some(current) = space.find_vma(self.input.address()) else {
            return false;
        };
        if current.range() != self.range
            || current.flags() != self.flags
            || current.max_flags() != self.max_flags
            || current.backing() != self.backing
            || current.page_offset() != self.page_offset
        {
            return false;
        }
        let Some(runtime) = current.runtime() else {
            return false;
        };
        if runtime.backing_info() != current.backing() {
            return false;
        }
        let VmRuntimeRef::AnonPrivate(backend) = runtime else {
            return false;
        };
        self.backend.is_same_mapping(backend)
    }

    fn commit(self, space: &mut MmSpace, page: KResult<PreparedPrivatePage>) -> PageFaultOutcome {
        space.drain_pending_invalidations();
        let address = self
            .input
            .address()
            .align_down(BackendOps::page_size(&self.backend));
        // The mutex covers validation and publication together. A retained VMA
        // or backend owner only establishes lifetime, not current mapping rights.
        if !self.matches_current_mapping(space)
            || !matches!(space.pgtbl.query(address), Err(PagingError::NotMapped))
            || !self.backend.has_empty_slot(address)
        {
            return PageFaultOutcome::Retry;
        }
        let flags = self.flags;
        let result = page.and_then(|page| {
            self.backend
                .commit_first_touch(address, flags, &mut space.pgtbl.modify(), page)?;
            Ok(FaultCompletion::from_populate((1, None)))
        });
        space.finish_backend_fault(self.input.address(), flags, result)
    }
}

impl MmSpace {
    /// Resolves a task-context fault while allowing independent anonymous
    /// first-touch allocations and zero filling outside the address-space mutex.
    ///
    /// The caller must retain its active address-space user and must not already
    /// hold `address_space`. VMA identity, permissions, the PTE and the anonymous
    /// object slot are revalidated under the same mutex used to commit the page.
    /// Changed state returns a retry outcome and discards unpublished storage.
    /// Other fault kinds use the existing locked fault path. This method may
    /// allocate and block; it is not an interrupt or RCU read-side operation.
    pub fn resolve_fault(address_space: &Mutex<Self>, input: FaultInput) -> PageFaultOutcome {
        Self::resolve_fault_with(address_space, input, |backend| {
            PreparedPrivatePage::new(BackendOps::page_size(backend))
        })
    }

    fn resolve_fault_with(
        address_space: &Mutex<Self>,
        input: FaultInput,
        prepare: impl FnOnce(&PrivateBackend) -> KResult<PreparedPrivatePage>,
    ) -> PageFaultOutcome {
        let fault = {
            let mut space = address_space.lock();
            space.drain_pending_invalidations();
            match FirstTouchFault::begin(&mut space, input) {
                Ok(fault) => fault,
                Err(outcome) => return outcome,
            }
        };
        let page = prepare(&fault.backend);
        fault.commit(&mut address_space.lock(), page)
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/fault_first_touch.rs");
