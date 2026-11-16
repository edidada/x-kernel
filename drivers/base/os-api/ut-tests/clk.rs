// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Clock capability contract regressions: the guard forwards operations to
//! the erased consumer, clones are independent consumers, and the op resolves
//! through the host adapter.

use alloc::sync::Arc;

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use unittest::{assert, assert_eq, def_test};

use crate::{Clk, ClkConsumer, ClockOp, ResError, ResResult};

/// Atomic-backed mock state: `ClkConsumer` is `Send + Sync`, so the mock
/// cannot rely on single-threaded access.
#[derive(Default)]
struct MockState {
    prepare_enable: AtomicU32,
    disable_unprepare: AtomicU32,
    enabled: AtomicBool,
    rate: AtomicU64,
}

struct MockConsumer {
    state: MockState,
    fail_enable: bool,
}

impl MockConsumer {
    fn new(fail_enable: bool) -> Self {
        Self {
            state: MockState {
                prepare_enable: AtomicU32::new(0),
                disable_unprepare: AtomicU32::new(0),
                enabled: AtomicBool::new(false),
                rate: AtomicU64::new(0),
            },
            fail_enable,
        }
    }
}

impl ClkConsumer for MockConsumer {
    fn prepare_enable(&self) -> ResResult {
        if self.fail_enable {
            return Err(ResError::BadState);
        }
        self.state.prepare_enable.fetch_add(1, Ordering::AcqRel);
        self.state.enabled.store(true, Ordering::Release);
        Ok(())
    }

    fn disable_unprepare(&self) {
        self.state.disable_unprepare.fetch_add(1, Ordering::AcqRel);
        self.state.enabled.store(false, Ordering::Release);
    }

    fn get_rate(&self) -> u64 {
        self.state.rate.load(Ordering::Acquire)
    }

    fn round_rate(&self, hz: u64) -> ResResult<u64> {
        Ok(hz)
    }

    fn set_rate(&self, hz: u64) -> ResResult<u64> {
        self.state.rate.store(hz, Ordering::Release);
        Ok(hz)
    }

    fn is_enabled(&self) -> bool {
        self.state.enabled.load(Ordering::Acquire)
    }
}

struct MockOp {
    consumer: Arc<MockConsumer>,
    missing: bool,
}

impl ClockOp for MockOp {
    fn clk_get(&self, provider_id: u32, index: u32) -> ResResult<Clk> {
        if self.missing || (provider_id, index) == (9, 9) {
            return Err(ResError::NotFound);
        }
        Ok(Clk::new(self.consumer.clone()))
    }
}

#[def_test]
fn guard_forwards_operations_to_the_consumer() {
    let consumer = Arc::new(MockConsumer::new(false));
    let clk = Clk::new(consumer.clone());

    assert!(!clk.is_enabled());
    clk.prepare_enable().expect("enable succeeds");
    assert!(clk.is_enabled());

    clk.set_rate(24_000_000).expect("set_rate succeeds");
    assert_eq!(clk.get_rate(), 24_000_000);
    assert_eq!(
        clk.round_rate(12_000_000).expect("round succeeds"),
        12_000_000
    );

    clk.disable_unprepare();
    assert!(!clk.is_enabled());

    assert_eq!(consumer.state.prepare_enable.load(Ordering::Acquire), 1);
    assert_eq!(
        consumer.state.disable_unprepare.load(Ordering::Acquire),
        1
    );
    assert_eq!(consumer.state.rate.load(Ordering::Acquire), 24_000_000);
}

#[def_test]
fn enable_failure_is_reported_without_flipping_state() {
    let consumer = Arc::new(MockConsumer::new(true));
    let clk = Clk::new(consumer.clone());

    let error = clk.prepare_enable().expect_err("enable fails");
    assert!(matches!(error, ResError::BadState));
    assert!(!clk.is_enabled());
    assert_eq!(consumer.state.prepare_enable.load(Ordering::Acquire), 0);
}

#[def_test]
fn clones_are_independent_consumers() {
    let consumer = Arc::new(MockConsumer::new(false));
    let clk = Clk::new(consumer.clone());
    let clone = clk.clone();

    clone.prepare_enable().expect("clone enable succeeds");
    assert!(clk.is_enabled(), "both guards observe the same clock");

    clk.disable_unprepare();
    assert!(!clone.is_enabled());
    assert_eq!(consumer.state.prepare_enable.load(Ordering::Acquire), 1);
}

#[def_test]
fn op_resolves_and_reports_missing_providers() {
    let consumer = Arc::new(MockConsumer::new(false));
    let op = MockOp {
        consumer: consumer.clone(),
        missing: false,
    };

    let clk = op.clk_get(1, 0).expect("known provider resolves");
    clk.prepare_enable().expect("enable succeeds");
    assert!(clk.is_enabled());

    let missing = op.clk_get(9, 9).expect_err("unknown pair fails");
    assert!(matches!(missing, ResError::NotFound));
}
