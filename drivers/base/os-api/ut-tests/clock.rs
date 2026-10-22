// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{assert_eq, def_test};

use crate::{ClkHandle, ClkOps, ResError};

struct Clock {
    fails_enable: bool,
    prepares: AtomicUsize,
    unprepares: AtomicUsize,
    disables: AtomicUsize,
}

impl Clock {
    fn new(fails_enable: bool) -> Self {
        Self {
            fails_enable,
            prepares: AtomicUsize::new(0),
            unprepares: AtomicUsize::new(0),
            disables: AtomicUsize::new(0),
        }
    }
}

impl ClkOps for Clock {
    fn name(&self) -> &str {
        "test-clock"
    }

    fn get_rate(&self) -> u64 {
        24_000_000
    }

    fn round_rate(&self, rate: u64) -> Result<u64, ResError> {
        Ok(rate)
    }

    fn set_rate(&self, rate: u64) -> Result<u64, ResError> {
        Ok(rate)
    }

    fn prepare(&self) -> Result<(), ResError> {
        self.prepares.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn unprepare(&self) {
        self.unprepares.fetch_add(1, Ordering::Relaxed);
    }

    fn enable(&self) -> Result<(), ResError> {
        if self.fails_enable {
            Err(ResError::Io)
        } else {
            Ok(())
        }
    }

    fn disable(&self) {
        self.disables.fetch_add(1, Ordering::Relaxed);
    }

    fn is_prepared(&self) -> bool {
        self.prepares.load(Ordering::Relaxed) > self.unprepares.load(Ordering::Relaxed)
    }

    fn is_enabled(&self) -> bool {
        self.is_prepared() && !self.fails_enable
    }
}

#[def_test]
fn failed_enable_unprepares_without_disabling() {
    let clock = Arc::new(Clock::new(true));
    let handle = ClkHandle::new(clock.clone(), None);
    let result = handle.prepare_enable();
    assert_eq!(result.err(), Some(ResError::Io));
    assert_eq!(clock.prepares.load(Ordering::Relaxed), 1);
    assert_eq!(clock.unprepares.load(Ordering::Relaxed), 1);
    assert_eq!(clock.disables.load(Ordering::Relaxed), 0);
}

#[def_test]
fn enabled_token_outlives_handle_and_balances_once() {
    let clock = Arc::new(Clock::new(false));
    let releases = Arc::new(AtomicUsize::new(0));
    let captured = releases.clone();
    let handle = ClkHandle::new(
        clock.clone(),
        Some(Arc::new(move || {
            captured.fetch_add(1, Ordering::Relaxed);
        })),
    );
    let enabled = handle.prepare_enable().unwrap();
    drop(handle);
    assert_eq!(releases.load(Ordering::Relaxed), 1);
    assert_eq!(clock.disables.load(Ordering::Relaxed), 0);
    drop(enabled);
    assert_eq!(clock.disables.load(Ordering::Relaxed), 1);
    assert_eq!(clock.unprepares.load(Ordering::Relaxed), 1);
}
