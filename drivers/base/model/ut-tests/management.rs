// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Single-task synchronous test fixture, not a host contention lock.
//!
//! Relaxed ordering suffices for same-task reentry detection. The RAII guard
//! clears ownership on exit; no spin guard spans a lifecycle callback.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::{DeviceManagementError, DeviceManagementLock};

pub(super) struct TestManagementLock {
    is_entered: AtomicBool,
}

impl TestManagementLock {
    pub(super) fn new() -> Self {
        Self {
            is_entered: AtomicBool::new(false),
        }
    }
}

struct EntryGuard<'a>(&'a AtomicBool);

impl Drop for EntryGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

impl DeviceManagementLock for TestManagementLock {
    fn run_locked(&self, operation: &mut dyn FnMut()) -> Result<(), DeviceManagementError> {
        if self.is_entered.swap(true, Ordering::Relaxed) {
            return Err(DeviceManagementError::Reentrant);
        }
        let _guard = EntryGuard(&self.is_entered);
        operation();
        Ok(())
    }
}
