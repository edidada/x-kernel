// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use memaddr::PAGE_SIZE_4K;
use unittest::def_test;

use super::PCP_PAGESET;
use crate::{AllocLocalOwner, UsageKind, global_allocator};

#[def_test]
fn retiring_dma_frame_does_not_hide_it_in_general_pcp() {
    let mut owner = AllocLocalOwner::acquire();
    // SAFETY: owner pins this CPU with IRQs disabled and prevents allocator
    // re-entry. The temporary PCP borrow ends before allocation or retirement.
    let before = unsafe { PCP_PAGESET.get_mut_unchecked(owner.guard()) }.cache1.len();
    let run = global_allocator().alloc_dma_frame_run(1, PAGE_SIZE_4K, UsageKind::Dma).unwrap();
    let last = run.try_clone_frame(0).unwrap();
    drop(run);
    // SAFETY: the same owner still excludes local re-entry and no earlier PCP
    // reference remains live. Dropping a non-final hold must not change inventory.
    assert_eq!(unsafe { PCP_PAGESET.get_mut_unchecked(owner.guard()) }.cache1.len(), before);
    drop(last);
    // SAFETY: owner still pins and excludes local IRQ users; all prior temporary
    // borrows ended. The final DMA hold must return to buddy instead of PCP.
    assert_eq!(unsafe { PCP_PAGESET.get_mut_unchecked(owner.guard()) }.cache1.len(), before);
}
