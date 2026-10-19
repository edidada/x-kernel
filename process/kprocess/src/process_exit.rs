// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use ksignal::{ChildExitInfo, ChildExitSignalInfo, SigchldChildExitSignalInfo, Signo};
use ktask::KtaskRef;

use crate::{Process, ProcessExitPublication, process_signals, wait_reap};

/// Starts thread exit, retiring external TID visibility while retaining accounting.
/// Complete user-memory cleanup before this call. Its MM pin permits scheduling
/// during subsequent resource teardown, but not further user-memory access.
/// Returns whether this is the last live thread, which owns process cleanup.
pub fn begin_thread_exit(process: &Arc<Process>, task: &KtaskRef, exit_code: i32) -> bool {
    process.begin_thread_exit(task, exit_code)
}

/// Completes cleanup and atomically releases nonleader membership and CPU totals.
/// Returns true for the last completed member, which must publish process exit.
/// A leader remains an accounting member until wait/autoreap or exec takeover
/// releases its identity.
pub fn finish_thread_exit(process: &Arc<Process>, task: &KtaskRef, exit_code: i32) -> bool {
    process.exit_thread(task, exit_code)
}

/// Marks the process thread group as group-exited.
pub fn mark_group_exited(process: &Process) {
    process.group_exit();
}

/// Transitions the process into waitable zombie state and reparents surviving children.
pub fn finalize_process_exit(process: &Arc<Process>) {
    finalize_process_exit_with_publication(process, ProcessExitPublication::WaitableZombie);
}

/// Transitions the process into an exited state and reparents surviving children.
pub fn finalize_process_exit_with_publication(
    process: &Arc<Process>,
    publication: ProcessExitPublication,
) {
    process.exit_with_publication(publication);
}

/// Completes process exit after the last thread has finished runtime cleanup.
///
/// The caller must release process-owned mm, files, filesystem context, and
/// namespace capabilities before calling this function.
///
/// The parent wait event is the tail publication step: a parent woken by
/// `wait*()` should only observe a fully resolved child-exit state, either a
/// waitable zombie or an already-detached autoreaped child.
pub fn complete_process_exit(process: &Arc<Process>) -> bool {
    let transition =
        process.finish_exit_in_process_domain(ProcessExitPublication::WaitableZombie, |parent| {
            let siginfo = sigchld_child_exit_signal_info(process, parent);
            process_signals::prepare_child_exit_to_process_ref(parent, siginfo)
                .map(|delivery| {
                    let autoreap = delivery.should_autoreap();
                    (delivery, autoreap)
                })
                .inspect_err(|err| {
                    warn!(
                        "failed to prepare child-exit SIGCHLD for child {} parent {}: {err:?}",
                        process.pid(),
                        parent.pid()
                    );
                })
                .ok()
        });

    if let Some(parent) = transition.reparented_zombie_parent {
        notify_child_exit(&parent);
    }
    if transition.autoreaped {
        wait_reap::reap_detached_process_identity(process);
    }

    let Some(parent) = transition.parent else {
        return false;
    };

    if let Some(delivery) = transition.prepared_sigchld {
        delivery.commit_and_interrupt();
    } else if let Some(signo) = transition.exit_signal
        && signo != Signo::SIGCHLD
        && let Err(err) = process_signals::send_to_process_ref(
            &parent,
            Some(child_exit_signal_info(process, &parent, signo).into_signal_info()),
        )
    {
        warn!(
            "failed to send child-exit signal {signo:?} for child {} parent {}: {err:?}",
            process.pid(),
            parent.pid()
        );
    }

    notify_child_exit(&parent);

    transition.autoreaped
}

/// Wakes parent waiters after child-exit signal/autoreap handling is complete.
pub fn notify_child_exit(parent: &Process) {
    parent.notify_child_exit();
}

pub(crate) fn child_exit_signal_info(
    process: &Process,
    parent: &Process,
    signo: Signo,
) -> ChildExitSignalInfo {
    ChildExitSignalInfo::new(signo, child_exit_info(process, parent))
}

pub(crate) fn sigchld_child_exit_signal_info(
    process: &Process,
    parent: &Process,
) -> SigchldChildExitSignalInfo {
    ChildExitSignalInfo::new_sigchld(child_exit_info(process, parent))
}

fn child_exit_info(process: &Process, parent: &Process) -> ChildExitInfo {
    let (utime, stime) = process.process_cpu_times();
    let uid = process
        .credentials_snapshot()
        .map(|credentials| credentials.ruid())
        .unwrap_or(0);
    // si_pid is filled per the waiting parent's active PID namespace
    // (pid_nr_ns), keeping the SIGCHLD / signalfd -> wait -> kill round trip
    // in one number space; the child keeps its root number internally.
    let observer = parent.active_pid_namespace();
    let pid = process
        .number_in_namespace(&observer)
        .unwrap_or_else(|| process.pid());
    ChildExitInfo::from_wait_status(pid, uid, process.exit_code(), utime, stime)
}
