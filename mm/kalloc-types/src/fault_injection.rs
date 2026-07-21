// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::AllocError;

const DISABLED: usize = usize::MAX;

static ALLOCATIONS_BEFORE_FAILURE: AtomicUsize = AtomicUsize::new(DISABLED);

pub struct AllocationFailureGuard {
    previous: usize,
}

impl Drop for AllocationFailureGuard {
    fn drop(&mut self) {
        ALLOCATIONS_BEFORE_FAILURE.store(self.previous, Ordering::SeqCst);
    }
}

pub fn fail_allocations_after(successful_allocations: usize) -> AllocationFailureGuard {
    let previous = ALLOCATIONS_BEFORE_FAILURE.swap(successful_allocations, Ordering::SeqCst);
    AllocationFailureGuard { previous }
}

pub fn fail_next_allocation() -> AllocationFailureGuard {
    fail_allocations_after(0)
}

pub fn maybe_fail_allocation() -> Result<(), AllocError> {
    loop {
        let remaining = ALLOCATIONS_BEFORE_FAILURE.load(Ordering::SeqCst);
        if remaining == DISABLED {
            return Ok(());
        }
        if remaining == 0 {
            if ALLOCATIONS_BEFORE_FAILURE
                .compare_exchange(0, DISABLED, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Err(AllocError::NoMemory);
            }
            continue;
        }
        if ALLOCATIONS_BEFORE_FAILURE
            .compare_exchange(remaining, remaining - 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Ok(());
        }
    }
}
