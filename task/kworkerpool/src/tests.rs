// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![cfg(unittest)]

use kcpu_id_map::LogicalCpuId;
use ktime_types::{MonotonicInstant, TimeSpan};
use unittest::{assert, assert_eq, def_test};

use super::*;

type TestPool = WorkerPool<u8, u8, u8, 4, 8>;

fn now(ns: u64) -> MonotonicInstant {
    MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(ns))
}

fn pool() -> TestPool {
    TestPool::new(
        PoolId::new(BuiltinPoolKind::Normal, LogicalCpuId::new(0)),
        WorkerPoolPolicy::new(WorkerPoolPolicyConfig {
            min_workers: 1,
            initial_workers: 1,
            max_workers: 4,
            idle_retire_after: Some(TimeSpan::from_nanos(10)),
            create_retry_delay: TimeSpan::from_nanos(10),
            cpu_intensive_threshold: TimeSpan::from_nanos(10),
            dynamic_create: true,
            idle_retire: true,
        }),
    )
}

fn bh_pool() -> TestPool {
    TestPool::new(
        PoolId::new(BuiltinPoolKind::Bh, LogicalCpuId::new(0)),
        WorkerPoolPolicy::for_kind(BuiltinPoolKind::Bh),
    )
}

fn entry(owner: u8, key: u8, payload: u8) -> PoolEntry<u8, u8, u8> {
    PoolEntry::new(owner, key, payload)
}

#[def_test]
fn runnable_enqueue_wakes_idle_worker() {
    let mut pool = pool();
    pool.install_worker(WorkerId::new(0)).unwrap();

    let actions = pool.enqueue_runnable(entry(1, 1, 10), now(1)).unwrap();

    assert_eq!(
        pool.worker_state(WorkerId::new(0)),
        Some(WorkerState::Preparing)
    );
    assert!(actions.immediate().any(|action| {
        matches!(
            action,
            ImmediateAction::WakeWorker {
                worker,
                ..
            } if worker == WorkerId::new(0)
        )
    }));
}

#[def_test]
fn deferred_entries_promote_by_owner_fifo() {
    let mut pool = pool();
    pool.install_worker(WorkerId::new(0)).unwrap();
    pool.enqueue_deferred(entry(1, 1, 10)).unwrap();
    pool.enqueue_deferred(entry(2, 1, 20)).unwrap();
    pool.enqueue_deferred(entry(1, 2, 11)).unwrap();

    let (promoted, actions) = pool.promote_deferred(&1, 2, now(1));

    assert_eq!(promoted, 2);
    assert!(
        actions
            .immediate()
            .any(|action| matches!(action, ImmediateAction::WakeWorker { .. }))
    );
    let first = match pool.pop_runnable(WorkerId::new(0)).unwrap() {
        DispatchResult::Entry(entry) => entry,
        DispatchResult::Empty => panic!("expected promoted entry"),
    };
    assert_eq!(first.into_payload(), 10);
}

#[def_test]
fn claim_failed_keeps_prepared_worker_while_runnable_remains() {
    let mut pool = pool();
    pool.install_worker(WorkerId::new(0)).unwrap();
    pool.enqueue_runnable(entry(1, 1, 10), now(1)).unwrap();
    pool.enqueue_runnable(entry(1, 2, 11), now(1)).unwrap();

    let _ = pool.pop_runnable(WorkerId::new(0)).unwrap();
    let (result, _) = pool.worker_claim_failed(WorkerId::new(0), now(2)).unwrap();

    assert_eq!(result, ClaimFailedResult::Retry);
    assert_eq!(
        pool.worker_state(WorkerId::new(0)),
        Some(WorkerState::Preparing)
    );
}

#[def_test]
fn cpu_intensive_running_worker_allows_another_worker() {
    let mut pool = pool();
    pool.install_worker(WorkerId::new(0)).unwrap();
    pool.install_worker(WorkerId::new(1)).unwrap();
    pool.enqueue_runnable(entry(1, 1, 10), now(1)).unwrap();
    let first = match pool.pop_runnable(WorkerId::new(0)).unwrap() {
        DispatchResult::Entry(entry) => entry,
        DispatchResult::Empty => panic!("expected entry"),
    };
    assert_eq!(first.into_payload(), 10);
    let (token, _) = pool.worker_started(WorkerId::new(0), now(1)).unwrap();

    pool.enqueue_runnable(entry(1, 2, 11), now(2)).unwrap();
    let actions = pool.worker_tick(WorkerId::new(0), token, now(12)).unwrap();

    assert_eq!(
        pool.worker_state(WorkerId::new(1)),
        Some(WorkerState::Preparing)
    );
    assert!(actions.immediate().any(|action| matches!(
        action,
        ImmediateAction::WakeWorker {
            worker,
            ..
        } if worker == WorkerId::new(1)
    )));
}

#[def_test]
fn manager_spawn_is_requested_when_no_worker_can_run() {
    let mut pool = pool();
    pool.install_worker(WorkerId::new(0)).unwrap();
    pool.enqueue_runnable(entry(1, 1, 10), now(1)).unwrap();
    let _ = pool.pop_runnable(WorkerId::new(0)).unwrap();
    let (token, _) = pool.worker_started(WorkerId::new(0), now(1)).unwrap();
    pool.enqueue_runnable(entry(1, 2, 11), now(2)).unwrap();

    let actions = pool
        .worker_blocked(WorkerId::new(0), token, now(2))
        .unwrap();

    assert!(
        actions
            .immediate()
            .any(|action| matches!(action, ImmediateAction::WakeManager { .. }))
    );
    assert_eq!(
        pool.next_management_action(now(2)),
        Some(ManagementAction::SpawnWorker {
            pool: PoolId::new(BuiltinPoolKind::Normal, LogicalCpuId::new(0)),
            worker: WorkerId::new(1),
        })
    );
}

#[def_test]
fn retire_requested_can_be_cancelled_before_exit() {
    let mut pool = pool();
    pool.install_worker(WorkerId::new(0)).unwrap();
    pool.install_worker(WorkerId::new(1)).unwrap();
    let (_, actions) = pool.worker_ready_to_park(WorkerId::new(1), now(1)).unwrap();
    assert!(
        actions
            .immediate()
            .any(|action| matches!(action, ImmediateAction::ArmWorkerTimer { .. }))
    );

    pool.retire_timer_expired(now(12));
    assert_eq!(
        pool.next_management_action(now(12)),
        Some(ManagementAction::RetireWorker {
            pool: PoolId::new(BuiltinPoolKind::Normal, LogicalCpuId::new(0)),
            worker: WorkerId::new(1),
        })
    );
    assert_eq!(
        pool.worker_state(WorkerId::new(1)),
        Some(WorkerState::RetireRequested)
    );

    let actions = pool.enqueue_runnable(entry(1, 1, 10), now(13)).unwrap();
    assert_eq!(
        pool.worker_state(WorkerId::new(1)),
        Some(WorkerState::Preparing)
    );
    assert!(actions.immediate().any(|action| matches!(
        action,
        ImmediateAction::WakeWorker {
            worker,
            ..
        } if worker == WorkerId::new(1)
    )));
}

#[def_test]
fn retire_requested_worker_exits_to_empty() {
    let mut pool = pool();
    pool.install_worker(WorkerId::new(0)).unwrap();
    pool.install_worker(WorkerId::new(1)).unwrap();
    let _ = pool.worker_ready_to_park(WorkerId::new(1), now(1)).unwrap();

    let _ = pool.retire_timer_expired(now(12));
    assert_eq!(
        pool.next_management_action(now(12)),
        Some(ManagementAction::RetireWorker {
            pool: PoolId::new(BuiltinPoolKind::Normal, LogicalCpuId::new(0)),
            worker: WorkerId::new(1),
        })
    );
    let (decision, _) = pool
        .worker_ready_to_park(WorkerId::new(1), now(13))
        .unwrap();
    assert_eq!(decision, ParkDecision::Exit);
    assert_eq!(
        pool.worker_state(WorkerId::new(1)),
        Some(WorkerState::Exiting)
    );

    pool.worker_exit_complete(WorkerId::new(1)).unwrap();
    assert_eq!(
        pool.worker_state(WorkerId::new(1)),
        Some(WorkerState::Empty)
    );
    assert_eq!(pool.installed_workers(), 1);
}

#[def_test]
fn spawn_failure_retry_blocks_immediate_recreate() {
    let mut pool = pool();
    pool.enqueue_runnable(entry(1, 1, 10), now(1)).unwrap();
    assert_eq!(
        pool.next_management_action(now(1)),
        Some(ManagementAction::SpawnWorker {
            pool: PoolId::new(BuiltinPoolKind::Normal, LogicalCpuId::new(0)),
            worker: WorkerId::new(0),
        })
    );
    let actions = pool
        .spawn_complete(WorkerId::new(0), ManagementComplete::SpawnFailed, now(1))
        .unwrap();
    assert!(actions.is_empty());
    assert_eq!(pool.next_management_action(now(2)), None);

    let actions = pool.enqueue_runnable(entry(1, 2, 11), now(12)).unwrap();
    assert!(
        actions
            .immediate()
            .any(|action| matches!(action, ImmediateAction::WakeManager { .. }))
    );
    assert_eq!(
        pool.next_management_action(now(12)),
        Some(ManagementAction::SpawnWorker {
            pool: PoolId::new(BuiltinPoolKind::Normal, LogicalCpuId::new(0)),
            worker: WorkerId::new(0),
        })
    );
}

#[def_test]
fn spawn_complete_accepts_provider_installed_worker() {
    let mut pool = pool();
    pool.install_worker(WorkerId::new(0)).unwrap();
    pool.enqueue_runnable(entry(1, 1, 10), now(1)).unwrap();
    let _ = pool.pop_runnable(WorkerId::new(0)).unwrap();
    let (token, _) = pool.worker_started(WorkerId::new(0), now(1)).unwrap();
    pool.enqueue_runnable(entry(1, 2, 11), now(2)).unwrap();
    let _ = pool
        .worker_blocked(WorkerId::new(0), token, now(2))
        .unwrap();

    assert_eq!(
        pool.next_management_action(now(2)),
        Some(ManagementAction::SpawnWorker {
            pool: PoolId::new(BuiltinPoolKind::Normal, LogicalCpuId::new(0)),
            worker: WorkerId::new(1),
        })
    );
    pool.install_worker(WorkerId::new(1)).unwrap();

    let actions = pool
        .spawn_complete(WorkerId::new(1), ManagementComplete::Spawned, now(3))
        .unwrap();

    assert_eq!(
        pool.worker_state(WorkerId::new(1)),
        Some(WorkerState::Preparing)
    );
    assert!(actions.immediate().any(|action| matches!(
        action,
        ImmediateAction::WakeWorker {
            worker,
            ..
        } if worker == WorkerId::new(1)
    )));
}

#[def_test]
fn bh_pool_never_requests_manager_spawn() {
    let mut pool = bh_pool();
    pool.install_worker(WorkerId::new(0)).unwrap();
    pool.enqueue_runnable(entry(1, 1, 10), now(1)).unwrap();
    let _ = pool.pop_runnable(WorkerId::new(0)).unwrap();
    let (token, _) = pool.worker_started(WorkerId::new(0), now(1)).unwrap();
    pool.enqueue_runnable(entry(1, 2, 11), now(2)).unwrap();

    let actions = pool
        .worker_blocked(WorkerId::new(0), token, now(2))
        .unwrap();

    assert!(actions.is_empty());
    assert_eq!(pool.next_management_action(now(2)), None);
}

#[def_test]
fn runnable_and_deferred_share_total_capacity() {
    let mut pool = pool();
    pool.install_worker(WorkerId::new(0)).unwrap();
    for key in 0..7 {
        pool.enqueue_deferred(entry(1, key, key)).unwrap();
    }
    pool.enqueue_runnable(entry(1, 7, 7), now(1)).unwrap();

    assert!(matches!(
        pool.enqueue_deferred(entry(1, 8, 8)),
        Err(WorkerPoolError::QueueFull(_))
    ));
}
