// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host concurrency tests importing the production report protocol directly.

use std::{
    sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

#[path = "../../../src/rendezvous/state.rs"]
mod state;
use state::{Participation, ReportState, WaitBudget, cpu_bit};

#[test]
fn concurrent_failures_elect_exactly_one_reporter() {
    for _ in 0..64 {
        let report = Arc::new(ReportState::new());
        let barrier = Arc::new(Barrier::new(8));
        let winners = thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|cpu| {
                    let report = report.clone();
                    let barrier = barrier.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        let won = report.try_begin(cpu, 0xff);
                        if won {
                            assert!(report.is_owner(cpu));
                            assert_eq!(report.targets(), 0xff);
                        }
                        usize::from(won)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .sum::<usize>()
        });
        assert_eq!(winners, 1);
        assert!(report.is_active());
    }
}

#[test]
fn sparse_targets_are_frozen_and_always_include_the_reporter() {
    let report = ReportState::new();
    assert!(!report.is_active());
    assert!(!report.all_ready());
    assert!(report.claim(3).is_none());
    assert!(report.try_begin(3, (1 << 1) | (1 << 7)));
    assert_eq!(report.targets(), (1 << 1) | (1 << 3) | (1 << 7));
    assert!(!report.try_begin(2, usize::MAX));
    assert!(report.claim(2).is_none());
    for cpu in [1, 3, 7] {
        assert!(matches!(report.claim(cpu), Some(Participation::Capture)));
        report.mark_ready(cpu);
    }
    assert!(report.all_ready());
    assert_eq!(report.close(), report.targets());
}

#[test]
fn nested_nmi_cannot_publish_or_park_an_interrupted_collector() {
    let report = ReportState::new();
    assert!(report.try_begin(0, 0b11));
    assert!(matches!(report.claim(1), Some(Participation::Capture)));
    assert!(report.claim(1).is_none());
    assert_eq!(report.ready(), 0);
    report.mark_ready(1);
    assert_eq!(report.ready(), 0b10);
    assert!(!report.all_ready());
}

#[test]
fn closed_report_excludes_late_and_inflight_publications_from_its_frozen_mask() {
    let report = ReportState::new();
    assert!(report.try_begin(0, 0b111));
    assert!(matches!(report.claim(0), Some(Participation::Capture)));
    report.mark_ready(0);
    assert!(matches!(report.claim(1), Some(Participation::Capture)));
    let frozen = report.close();
    assert_eq!(frozen, 0b001);
    report.mark_ready(1);
    assert_eq!(frozen & (1 << 1), 0);
    assert_eq!(report.ready(), 0b011);
    assert!(matches!(report.claim(2), Some(Participation::Park)));
    assert!(report.claim(2).is_none());
    assert!(!report.all_ready());
    assert!(!report.try_begin(0, 1));
}

#[test]
fn observing_ready_acquires_the_snapshot_payload() {
    let report = Arc::new(ReportState::new());
    let payload = Arc::new(AtomicUsize::new(0));
    assert!(report.try_begin(0, 0b11));
    thread::scope(|scope| {
        scope.spawn(|| {
            assert!(matches!(report.claim(1), Some(Participation::Capture)));
            payload.store(0x1234, Ordering::Relaxed);
            report.mark_ready(1);
        });
        while report.ready() & (1 << 1) == 0 {
            thread::yield_now();
        }
        assert_eq!(payload.load(Ordering::Relaxed), 0x1234);
    });
}

#[test]
fn wait_budget_expires_at_the_deadline() {
    let mut budget = WaitBudget::new(100, 20, 10);
    assert!(!budget.is_exhausted(100));
    assert!(!budget.is_exhausted(119));
    assert!(budget.is_exhausted(120));
}

#[test]
fn wait_budget_terminates_with_a_stopped_or_regressing_clock() {
    for sample in [100, 99, 0] {
        let mut budget = WaitBudget::new(100, 20, 3);
        for _ in 0..3 {
            assert!(!budget.is_exhausted(sample));
        }
        assert!(budget.is_exhausted(sample));
    }
}

#[test]
fn out_of_range_cpu_ids_cannot_claim_or_corrupt_membership() {
    let report = ReportState::new();
    for cpu in [usize::BITS as usize, usize::MAX] {
        assert_eq!(cpu_bit(cpu), None);
        assert!(!report.try_begin(cpu, usize::MAX));
        assert!(report.claim(cpu).is_none());
        report.mark_ready(cpu);
    }
    assert!(!report.is_active());
    assert_eq!(report.ready(), 0);
    assert_eq!(
        cpu_bit(usize::BITS as usize - 1),
        Some(1usize << (usize::BITS - 1))
    );
}
