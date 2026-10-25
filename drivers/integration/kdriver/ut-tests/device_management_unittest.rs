// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kspin::{IrqSave, NoPreempt};
use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test(serial)]
fn executes_once_and_releases_for_reuse() {
    let lock = new_device_management_lock().unwrap();
    let mut calls = 0;
    assert_eq!(lock.run_locked(&mut || calls += 1), Ok(()));
    assert_eq!(calls, 1);
    assert_eq!(lock.run_locked(&mut || calls += 1), Ok(()));
    assert_eq!(calls, 2);
}

#[def_test(serial)]
fn recursion_rejects_callback_but_independent_lock_succeeds() {
    let lock = new_device_management_lock().unwrap();
    let independent = new_device_management_lock().unwrap();
    let mut nested_result = Ok(());
    let mut independent_result = Err(DeviceManagementError::Reentrant);
    let mut nested_calls = 0;
    let mut independent_calls = 0;
    assert_eq!(
        lock.run_locked(&mut || {
            nested_result = lock.run_locked(&mut || nested_calls += 1);
            independent_result = independent.run_locked(&mut || independent_calls += 1);
        }),
        Ok(())
    );
    assert_eq!(nested_result, Err(DeviceManagementError::Reentrant));
    assert_eq!(nested_calls, 0);
    assert_eq!(independent_result, Ok(()));
    assert_eq!(independent_calls, 1);
    assert_eq!(lock.run_locked(&mut || nested_calls += 1), Ok(()));
    assert_eq!(nested_calls, 1);
}

fn rejects_invalid_context(lock: &dyn DeviceManagementLock) -> bool {
    let mut was_called = false;
    let creation_is_rejected = matches!(
        new_device_management_lock(),
        Err(DeviceManagementError::InvalidContext)
    );
    let result = lock.run_locked(&mut || was_called = true);
    creation_is_rejected && result == Err(DeviceManagementError::InvalidContext) && !was_called
}

#[def_test(serial)]
fn irq_off_and_preempt_disabled_reject_creation_and_execution() {
    let lock = new_device_management_lock().unwrap();
    {
        let _guard = IrqSave::new();
        assert!(rejects_invalid_context(lock.as_ref()));
    }
    {
        let _guard = NoPreempt::new();
        assert!(rejects_invalid_context(lock.as_ref()));
    }
    let mut context_wins_over_recursion = false;
    assert_eq!(
        lock.run_locked(&mut || {
            let _guard = IrqSave::new();
            context_wins_over_recursion = rejects_invalid_context(lock.as_ref());
        }),
        Ok(())
    );
    assert!(context_wins_over_recursion);
    assert_eq!(lock.run_locked(&mut || {}), Ok(()));
}

#[cfg(feature = "hwrng")]
#[def_test(serial)]
fn rcu_read_scope_rejects_creation_and_execution() {
    let lock = new_device_management_lock().unwrap();
    let is_rejected = krcu::with_read(|_| rejects_invalid_context(lock.as_ref()));
    assert!(is_rejected);
    assert_eq!(lock.run_locked(&mut || {}), Ok(()));
}

#[cfg(feature = "hwrng")]
fn wait_bounded(mut ready: impl FnMut() -> bool) -> bool {
    let deadline_ns = khal::time::monotonic_time_nanos().saturating_add(5_000_000_000);
    loop {
        if ready() {
            return true;
        }
        if khal::time::monotonic_time_nanos() >= deadline_ns {
            return false;
        }
        ktask::yield_now();
    }
}

#[cfg(feature = "hwrng")]
#[def_test(serial)]
fn contending_task_enters_only_after_owner_releases() {
    use core::sync::atomic::{AtomicBool, Ordering};

    use kcompletion::Completion;

    let lock = new_device_management_lock().unwrap();
    let started = Arc::new(Completion::new());
    let finished = Arc::new(Completion::new());
    let has_entered = Arc::new(AtomicBool::new(false));
    let has_succeeded = Arc::new(AtomicBool::new(false));
    let mut contender = None;
    let mut has_started = false;
    let mut was_blocked = false;
    let mut entered_while_owned = false;
    let owner_result = lock.run_locked(&mut || {
        contender = Some(ktask::spawn({
            let lock = lock.clone();
            let started = started.clone();
            let finished = finished.clone();
            let has_entered = has_entered.clone();
            let has_succeeded = has_succeeded.clone();
            move || {
                started.complete();
                let result = lock.run_locked(&mut || {
                    has_entered.store(true, Ordering::Release);
                });
                has_succeeded.store(result.is_ok(), Ordering::Release);
                finished.complete();
            }
        }));
        has_started = wait_bounded(|| started.try_wait());
        if let Some(task) = contender.as_ref() {
            was_blocked = wait_bounded(|| {
                task.state() == ktask::TaskState::Blocked || finished.is_completed()
            }) && task.state() == ktask::TaskState::Blocked;
        }
        entered_while_owned = has_entered.load(Ordering::Acquire);
    });
    let has_finished = wait_bounded(|| finished.try_wait());
    let exit_code = contender.as_ref().and_then(join_bounded);
    assert_eq!(owner_result, Ok(()));
    assert!(has_started);
    assert!(was_blocked);
    assert!(!entered_while_owned);
    assert!(has_finished);
    assert_eq!(exit_code, Some(0));
    assert!(has_entered.load(Ordering::Acquire));
    assert!(has_succeeded.load(Ordering::Acquire));
    assert_eq!(lock.run_locked(&mut || {}), Ok(()));
}

#[cfg(feature = "hwrng")]
fn join_bounded(task: &ktask::KtaskRef) -> Option<i32> {
    wait_bounded(|| task.state() == ktask::TaskState::Exited).then(|| task.join())
}

#[cfg(feature = "hwrng")]
#[def_test(serial)]
fn independent_lock_completes_on_another_task_while_owner_holds_first() {
    use core::sync::atomic::{AtomicBool, Ordering};

    use kcompletion::Completion;

    let first = new_device_management_lock().unwrap();
    let second = new_device_management_lock().unwrap();
    let finished = Arc::new(Completion::new());
    let has_succeeded = Arc::new(AtomicBool::new(false));
    let mut worker = None;
    let mut completed_while_owned = false;
    let owner_result = first.run_locked(&mut || {
        worker = Some(ktask::spawn({
            let second = second.clone();
            let finished = finished.clone();
            let has_succeeded = has_succeeded.clone();
            move || {
                let mut calls = 0;
                let result = second.run_locked(&mut || calls += 1);
                has_succeeded.store(result.is_ok() && calls == 1, Ordering::Release);
                finished.complete();
            }
        }));
        completed_while_owned = wait_bounded(|| finished.try_wait());
    });
    let exit_code = worker.as_ref().and_then(join_bounded);
    assert_eq!(owner_result, Ok(()));
    assert!(completed_while_owned);
    assert_eq!(exit_code, Some(0));
    assert!(has_succeeded.load(Ordering::Acquire));
}
