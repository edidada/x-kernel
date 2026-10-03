// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Async worker lifecycle: lazy spawn, idle park, and shutdown-driven exit.
//!
//! Workers are **sibling threads of the owning process** created through the
//! ordinary clone path ([`kprocess::Thread::prepare_thread_clone`] →
//! [`ktask::TaskInner::new_user`] → [`kprocess::publish_user_task`]) with a
//! pure kernel-mode entry loop. This is what makes user-buffer operations
//! sound: a worker dereferences user VAs through the same guarded
//! user-memory path as a syscall, resolves against its own process's address
//! space, and shares the process fd table and credentials — without any
//! mm-adoption machinery.
//!
//! # Exit contract
//!
//! Workers participate in process thread membership, so the process's
//! address space and fd table stay alive for as long as any worker runs.
//! Retirement happens when the context shuts down or the thread/process is
//! exiting; the loop then calls [`posix_process::do_exit`] so membership,
//! cgroup charge, and — when the worker is the last member — the process
//! tail are handled by the ordinary exit path. Blocked operations unwind
//! through the same `interruptible` waits the synchronous syscalls use, so
//! the group-exit SIGKILL broadcast unblocks them.
//!
//! The spawn context must be a sleepable user-thread context (the submit
//! path): spawning walks the cgroup and identity allocators.

use alloc::sync::Arc;

use kerrno::{KError, KResult};
use kpoll::IoEvents;
use ktask::future::{block_on, interruptible, poll_io};

use crate::context::{AsyncContext, CompletedOp, PreparedOp, WorkSource};

/// Upper bound on workers per context.
///
/// Queued operations beyond the cap wait for a worker to drain; this bounds
/// kernel-stack usage per context while allowing a few concurrent blocking
/// operations.
pub(crate) const WORKER_CAP: u32 = 4;

/// Ensures a worker exists when queued work exceeds idle capacity.
///
/// Called from the submit path (sleepable user-thread context). The slot is
/// reserved under the context lock before spawning so concurrent submitters
/// cannot oversubscribe the cap; a failed spawn releases the reservation.
///
/// # Errors
///
/// Forwards thread-clone, publication, or commit errors.
pub(crate) fn ensure_worker(ctx: &Arc<AsyncContext>) -> KResult<()> {
    if ctx.wants_worker() {
        let ordinal = ctx.next_worker_ordinal();
        ctx.reserve_worker();
        if let Err(err) = spawn_worker(ctx, ordinal) {
            ctx.unreserve_worker();
            return Err(err);
        }
    }
    Ok(())
}

/// Spawns one worker thread of the current process, returning its task
/// handle (used by tests to observe the worker's scheduler state).
fn spawn_worker(ctx: &Arc<AsyncContext>, ordinal: u32) -> KResult<ktask::KtaskRef> {
    let current_thread = kprocess::current_user_thread();
    let prepared = current_thread.prepare_thread_clone()?;
    let name = alloc::format!("kaiow/{}/{}", prepared.process().pid(), ordinal);
    debug!("asyncio spawn worker {name}");
    let page_table_root = prepared.page_table_root();
    let (thread, task_number) = prepared.into_parts();

    let worker_ctx = ctx.clone();
    let mut task =
        ktask::TaskInner::new_user(move || worker_loop(worker_ctx), name, task_number, thread);
    // Mirror the clone syscall: copy the creator's affinity and install the
    // process page-table root so kernel-mode user-VA access resolves against
    // the owning process's address space.
    task.set_cpumask(ktask::current().cpumask());
    task.ctx_mut().set_page_table_root(page_table_root);
    kprocess::publish_user_task(task)?.commit(|_| Ok(()))
}

/// Worker entry loop: dispatch queued operations, park when idle, retire on
/// shutdown or process exit.
fn worker_loop(ctx: Arc<AsyncContext>) {
    loop {
        match ctx.pop_ready() {
            Some(op) => {
                ctx.account_inflight();
                let res = execute(&op);
                ctx.publish_completion(CompletedOp {
                    cookie: op.cookie,
                    res,
                });
            }
            None => {
                if should_retire(&ctx) {
                    break;
                }
                park_until_work(&ctx);
            }
        }
        if should_retire(&ctx) {
            break;
        }
    }
    ctx.retire_worker();
    // Exit as a normal thread of the process. `group_exit = false`: the
    // leader's exit already broadcast SIGKILL to surviving siblings, which is
    // what unwinds any in-flight operation.
    posix_process::do_exit(0, false);
}

/// Spike stand-in for the operation table: every queued operation completes
/// successfully. The syscall steps replace this with the real dispatch.
fn execute(op: &PreparedOp) -> i64 {
    let _ = op;
    0
}

/// Parks the worker until work arrives or the context shuts down.
///
/// The wait is `interruptible`, so a signal (notably the group-exit
/// SIGKILL) unblocks it; spurious wakeups simply fall back into the loop,
/// which re-checks the retirement conditions.
fn park_until_work(ctx: &Arc<AsyncContext>) {
    ctx.enter_idle();
    // Errors are intentionally dropped: `WouldBlock` is impossible with
    // `non_blocking = false`, and `Interrupted` (or `Registration` failures)
    // fall through to the loop's retire checks.
    let _ = block_on(interruptible(poll_io(
        &WorkSource(ctx),
        IoEvents::IN,
        false,
        || {
            if ctx.has_ready() || ctx.is_shutdown() {
                Ok(())
            } else {
                Err(KError::WouldBlock)
            }
        },
    )));
    ctx.leave_idle();
}

/// Returns whether the worker must leave its loop: context shutdown, thread
/// exit in progress, or process group exit.
fn should_retire(ctx: &Arc<AsyncContext>) -> bool {
    if ctx.is_shutdown() {
        return true;
    }
    let thread = kprocess::current_user_thread();
    thread.is_exiting() || thread.process().is_group_exited()
}

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;

    use ktask::{TaskState, WaitQueue};
    use ktime_types::TimeSpan;
    use unittest::def_test;

    use super::*;

    /// Waits for a scheduler-observable condition with short parks instead
    /// of a busy spin, failing the test after a bounded deadline.
    fn wait_until(condition: impl Fn() -> bool) {
        let deadline = khal::time::monotonic_time() + TimeSpan::from_secs(5);
        let queue = WaitQueue::new();
        while !condition() {
            assert!(
                khal::time::monotonic_time() < deadline,
                "kasyncio worker test timed out"
            );
            queue.wait_timeout(TimeSpan::from_millis(1));
        }
    }

    /// Spawns one worker directly (bypassing the growth policy) so the test
    /// holds the worker's task handle for state observation.
    fn spawn_direct(ctx: &Arc<AsyncContext>) -> ktask::KtaskRef {
        ctx.push_ready(PreparedOp { cookie: 1 })
            .expect("seed op below queue limit");
        ctx.reserve_worker();
        spawn_worker(ctx, ctx.next_worker_ordinal()).expect("worker spawn must succeed")
    }

    #[def_test(user, serial)]
    fn worker_executes_queued_op_and_exits_on_shutdown() {
        let ctx = AsyncContext::new().expect("context creation");
        ctx.push_ready(PreparedOp { cookie: 0xABCD })
            .expect("op below queue limit");
        ensure_worker(&ctx).expect("worker spawn must succeed");

        wait_until(|| ctx.has_completions());
        let completions: alloc::vec::Vec<_> = ctx.take_completions().into_iter().collect();
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].cookie, 0xABCD);
        assert_eq!(completions[0].res, 0);

        ctx.shutdown();
        wait_until(|| ctx.total_workers() == 0);
    }

    #[def_test(user, serial)]
    fn shutdown_wakes_an_idle_parked_worker() {
        let ctx = AsyncContext::new().expect("context creation");
        let worker = spawn_direct(&ctx);
        wait_until(|| ctx.has_completions());
        wait_until(|| worker.state() == TaskState::Blocked);

        ctx.shutdown();
        wait_until(|| ctx.total_workers() == 0);
    }

    #[def_test(user, serial)]
    fn interrupt_alone_does_not_retire_an_idle_worker() {
        // A signal without a group exit only interrupts the idle wait; the
        // worker observes no retire condition and goes back to sleep. The
        // interrupt flag is consumed by the first wait, so later waits are
        // clean (no busy spin).
        let ctx = AsyncContext::new().expect("context creation");
        let worker = spawn_direct(&ctx);
        wait_until(|| ctx.has_completions());
        wait_until(|| worker.state() == TaskState::Blocked);

        worker.interrupt();
        wait_until(|| worker.state() == TaskState::Blocked);
        assert_eq!(ctx.total_workers(), 1);

        ctx.shutdown();
        wait_until(|| ctx.total_workers() == 0);
    }

    #[def_test(user, serial)]
    fn worker_executes_a_second_op_after_reparking() {
        // After the first op completes the worker parks again; a second
        // submission must wake it, complete, and leave the idle accounting
        // balanced.
        let ctx = AsyncContext::new().expect("context creation");
        let worker = spawn_direct(&ctx);
        wait_until(|| ctx.has_completions());
        wait_until(|| worker.state() == TaskState::Blocked);
        assert_eq!(ctx.total_workers(), 1);
        assert_eq!(ctx.idle_workers(), 1);

        ctx.push_ready(PreparedOp { cookie: 2 })
            .expect("op below queue limit");
        wait_until(|| {
            ctx.take_completions()
                .into_iter()
                .any(|completion| completion.cookie == 2)
        });
        wait_until(|| worker.state() == TaskState::Blocked);
        assert_eq!(ctx.idle_workers(), 1);
    }
}
