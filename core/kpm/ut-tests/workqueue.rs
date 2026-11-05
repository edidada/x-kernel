// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real private queues using permanently boot-installed worker/timer services.
//! Each fixture thaws, drains handles and closes its queues; no registry is replaced.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use kworkng::{CpuPolicy, QueueConfig, WorkHandle};
use unittest::{assert, assert_eq, def_test};

use super::*;

fn queue(name: &str) -> NormalQueue {
    let mut cpus = Vec::new();
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| cpus.push(cpu));
    NormalQueue::new(
        name,
        QueueConfig::new(&cpus, CpuPolicy::CurrentCpu, 1).unwrap(),
    )
    .unwrap()
}

#[def_test(serial)]
fn queue_set_holds_pending_work_while_unselected_queue_progresses_then_thaws() {
    let queues = [queue("pm-selected0"), queue("pm-selected1")];
    let peer = queue("pm-unselected");
    let cancellation = QueueFreezeCancellation::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let first_calls = calls.clone();
    let first = WorkHandle::new(move || {
        first_calls.fetch_add(1, Ordering::Release);
    })
    .unwrap();
    let second_calls = calls.clone();
    let second = WorkHandle::new(move || {
        second_calls.fetch_add(1, Ordering::Release);
    })
    .unwrap();
    let peer_calls = Arc::new(AtomicUsize::new(0));
    let callback_peer_calls = peer_calls.clone();
    let peer_work = WorkHandle::new(move || {
        callback_peer_calls.fetch_add(1, Ordering::Release);
    })
    .unwrap();
    let mut set = WorkqueueFreezeSession::new(SuspendWorkqueues {
        queues: &queues,
        timeout: TimeSpan::from_secs(5),
        cancellation: &cancellation,
    });
    set.freeze().unwrap();
    first.queue(&queues[0], None).unwrap();
    second.queue(&queues[1], None).unwrap();
    peer_work.queue(&peer, None).unwrap();
    peer.flush().unwrap();
    assert_eq!(peer_calls.load(Ordering::Acquire), 1);
    assert_eq!(calls.load(Ordering::Acquire), 0);
    cancellation.cancel(); // A completed freeze retains ownership through recovery.
    assert_eq!(calls.load(Ordering::Acquire), 0);
    set.thaw();
    set.thaw();
    for queue in &queues {
        queue.flush().unwrap();
    }
    assert_eq!(calls.load(Ordering::Acquire), 2);
    first.shutdown().unwrap();
    second.shutdown().unwrap();
    peer_work.shutdown().unwrap();
    for queue in &queues {
        queue.close_and_drain().unwrap();
    }
    peer.close_and_drain().unwrap();
}

#[def_test(serial)]
fn busy_queue_retains_successful_prefix_until_explicit_cleanup_without_stealing_owner() {
    let queues = [queue("pm-prefix"), queue("pm-busy")];
    let cancellation = QueueFreezeCancellation::new();
    let external = queues[1].freeze(TimeSpan::ZERO, &cancellation).unwrap();
    let mut set = WorkqueueFreezeSession::new(SuspendWorkqueues {
        queues: &queues,
        timeout: TimeSpan::ZERO,
        cancellation: &cancellation,
    });
    assert_eq!(
        set.freeze(),
        Err(WorkqueueFreezeError {
            index: 1,
            error: QueueFreezeError::Busy
        })
    );
    assert!(matches!(
        queues[0].freeze(TimeSpan::ZERO, &cancellation),
        Err(QueueFreezeError::Busy)
    ));
    set.thaw();
    drop(queues[0].freeze(TimeSpan::ZERO, &cancellation).unwrap());
    assert!(matches!(
        queues[1].freeze(TimeSpan::ZERO, &cancellation),
        Err(QueueFreezeError::Busy)
    ));
    drop(external);
    set.freeze().unwrap();
    set.thaw();
    for queue in &queues {
        queue.close_and_drain().unwrap();
    }
}

#[def_test(serial)]
fn duplicate_queue_alias_fails_at_later_position_and_releases_original_pause() {
    let original = queue("pm-duplicate");
    let queues = [original.clone(), original.clone()];
    let cancellation = QueueFreezeCancellation::new();
    let mut set = WorkqueueFreezeSession::new(SuspendWorkqueues {
        queues: &queues,
        timeout: TimeSpan::ZERO,
        cancellation: &cancellation,
    });
    assert_eq!(
        set.freeze(),
        Err(WorkqueueFreezeError {
            index: 1,
            error: QueueFreezeError::Busy
        })
    );
    set.thaw();
    drop(original.freeze(TimeSpan::ZERO, &cancellation).unwrap());
    original.close_and_drain().unwrap();
}

#[def_test(serial)]
fn cancelled_or_closed_queue_set_can_be_cleaned_without_leaving_pauses() {
    let queues = [queue("pm-cancel"), queue("pm-closed")];
    let cancelled = QueueFreezeCancellation::new();
    cancelled.cancel();
    let mut set = WorkqueueFreezeSession::new(SuspendWorkqueues {
        queues: &queues,
        timeout: TimeSpan::ZERO,
        cancellation: &cancelled,
    });
    assert_eq!(
        set.freeze(),
        Err(WorkqueueFreezeError {
            index: 0,
            error: QueueFreezeError::Cancelled
        })
    );
    set.thaw();
    let fresh = QueueFreezeCancellation::new();
    queues[1].close_and_drain().unwrap();
    let mut retry = WorkqueueFreezeSession::new(SuspendWorkqueues {
        queues: &queues,
        timeout: TimeSpan::ZERO,
        cancellation: &fresh,
    });
    assert_eq!(
        retry.freeze(),
        Err(WorkqueueFreezeError {
            index: 1,
            error: QueueFreezeError::Closed
        })
    );
    retry.thaw();
    drop(queues[0].freeze(TimeSpan::ZERO, &fresh).unwrap());
    queues[0].close_and_drain().unwrap();
}
