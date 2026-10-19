// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

// Real exit tails block twice and optionally migrate across a remote window.
// Both resume boundaries check saved/hardware roots and MM scheduling metadata.
// Noexec controls also retain M through the runtime. Exec cases replace that
// binding twice while the unpublished sibling must keep using its captured M.

use alloc::{format, string::String, sync::Arc, vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kprocess::{
    ForkAddressSpace, ForkFdTable, ForkFs, ForkParent, ForkSignalActions, ProcessForkConfig,
};
use ktask::{TaskInner, TaskState, WaitQueue};
use ktime_types::{MonotonicInstant, TimeSpan};
use unittest::{TestResult, def_test};
use unittest_support::TestUserValue;

use crate::task::execve::{exec_owned, lifecycle_tests::executable_root};

struct Stages {
    command: AtomicUsize,
    acknowledgement: AtomicUsize,
    command_queue: WaitQueue,
    finish_queue: WaitQueue,
    leader_stage: AtomicUsize,
    // The leader publishes exit; only the parent releases both held tails.
    has_leader_exited: AtomicBool,
    can_finish: AtomicBool,
}

const START: usize = 1;
const RESUME_FIRST: usize = 2;
const END_WINDOW: usize = 3;
const FIRST_PARKED: usize = 1;
const WINDOW_HELD: usize = 2;
const SECOND_PARKED: usize = 3;

#[derive(Clone, Copy)]
#[repr(usize)]
enum LeaderStage {
    Prepared             = 1,
    FirstBoundaryChecked = 2,
    SecondBoundaryChecked = 3,
    ExitPublished        = 4,
}

fn leader_stage_label(stage: usize) -> &'static str {
    match stage {
        0 => "starting",
        1 => "prepared",
        2 => "first-boundary-checked",
        3 => "second-boundary-checked",
        4 => "exit-published",
        _ => "invalid",
    }
}

fn renew_stage_deadline(
    stage: &mut usize,
    next: usize,
    deadline: &mut MonotonicInstant,
    now: MonotonicInstant,
) {
    if next > *stage {
        *stage = next;
        *deadline = now + TimeSpan::from_secs(5);
    }
}

impl Stages {
    fn new() -> Self {
        Self {
            command: AtomicUsize::new(0),
            acknowledgement: AtomicUsize::new(0),
            command_queue: WaitQueue::new(),
            finish_queue: WaitQueue::new(),
            leader_stage: AtomicUsize::new(0),
            has_leader_exited: AtomicBool::new(false),
            can_finish: AtomicBool::new(false),
        }
    }

    fn advance(&self, stage: LeaderStage) {
        let stage = stage as usize;
        // A single leader publishes a finite sequence, never a heartbeat.
        assert!(stage > self.leader_stage.load(Ordering::Acquire));
        self.leader_stage.store(stage, Ordering::Release);
    }

    fn diagnostics(&self) -> String {
        format!(
            "stage={} command={} ack={} leader_exited={} can_finish={}",
            leader_stage_label(self.leader_stage.load(Ordering::Acquire)),
            self.command.load(Ordering::Acquire),
            self.acknowledgement.load(Ordering::Acquire),
            self.has_leader_exited.load(Ordering::Acquire),
            self.can_finish.load(Ordering::Acquire),
        )
    }

    fn command(&self, value: usize) -> bool {
        // Only the leader writes commands; only the sibling acknowledges them.
        // Release/acquire publishes completed observations before advancing.
        self.command.store(value, Ordering::Release);
        self.command_queue.notify_one(false)
    }

    fn acknowledge(&self, value: usize) {
        self.acknowledgement.store(value, Ordering::Release);
    }

    fn wait_command(&self, value: usize) {
        let has_timed_out = self
            .command_queue
            .wait_timeout_until(TimeSpan::from_secs(5), || {
                self.command.load(Ordering::Acquire) >= value
            });
        assert!(
            !has_timed_out,
            "exit-tail timeout: wait command {value}; {}; state={:?}",
            self.diagnostics(),
            ktask::current().state(),
        );
    }

    fn wait_parked(&self, value: usize, task: &ktask::KtaskRef) {
        self.wait_observed("leader wait sibling parked", task, || {
            self.acknowledgement.load(Ordering::Acquire) == value
                && task.state() == TaskState::Blocked
        });
    }

    fn wait_finish(&self) {
        let has_timed_out = self
            .finish_queue
            .wait_timeout_until(TimeSpan::from_secs(5), || {
                self.can_finish.load(Ordering::Acquire)
            });
        assert!(
            !has_timed_out,
            "exit-tail timeout: wait finish; {}; state={:?}",
            self.diagnostics(),
            ktask::current().state(),
        );
    }

    fn finish(&self) -> usize {
        self.can_finish.store(true, Ordering::Release);
        // Both held tails must resume. Count delivery for the protocol regression.
        self.finish_queue.notify_many(usize::MAX, false)
    }

    fn wait_observed(
        &self,
        label: &str,
        task: &ktask::KtaskRef,
        mut condition: impl FnMut() -> bool,
    ) {
        let deadline = khal::time::monotonic_time() + TimeSpan::from_secs(5);
        let queue = WaitQueue::new();
        while !condition() {
            assert!(
                khal::time::monotonic_time() < deadline,
                "exit-tail timeout: {label}; {}; state={:?} cpu={:?}",
                self.diagnostics(),
                task.state(),
                task.cpu_id(),
            );
            // ACK publication precedes actual blocking; no notification marks
            // that scheduler transition. Poll only the observer, not the tail.
            queue.wait_timeout(TimeSpan::from_millis(1));
        }
    }

    fn wait_leader_parked(&self, task: &ktask::KtaskRef) {
        let mut stage = 0;
        let mut deadline = khal::time::monotonic_time() + TimeSpan::from_secs(5);
        let queue = WaitQueue::new();
        loop {
            let next = self.leader_stage.load(Ordering::Acquire);
            let now = khal::time::monotonic_time();
            renew_stage_deadline(&mut stage, next, &mut deadline, now);
            if self.has_leader_exited.load(Ordering::Acquire) && task.state() == TaskState::Blocked
            {
                return;
            }
            assert!(
                now < deadline,
                "exit-tail timeout: parent wait leader exited and blocked; {}; state={:?} cpu={:?}",
                self.diagnostics(),
                task.state(),
                task.cpu_id(),
            );
            queue.wait_timeout(TimeSpan::from_millis(1));
        }
    }
}

pub(in crate::task) fn bounded_wait(condition: impl FnMut() -> bool) {
    bounded_wait_labeled("exit-tail", condition);
}

fn bounded_wait_labeled(label: &str, mut condition: impl FnMut() -> bool) {
    let deadline = khal::time::monotonic_time() + TimeSpan::from_secs(5);
    let queue = WaitQueue::new();
    while !condition() {
        assert!(
            khal::time::monotonic_time() < deadline,
            "exit-tail timeout: {label}"
        );
        queue.wait_timeout(TimeSpan::from_millis(1));
    }
}

/// Captures only root metadata; no MM user, pin or mutex guard escapes.
pub(in crate::task) fn capture_root(mm: &memspace::MmSpace) -> impl Fn() -> usize + Clone + use<> {
    let root = mm.page_table_root();
    let residency = mm.cpu_residency().clone();
    #[cfg(target_arch = "aarch64")]
    let asid = mm.user_asid_context().unwrap().clone();
    // Only metadata escapes; no MM user, structural pin or mutex guard.
    move || {
        let _guard = kspin::NoPreemptIrqSave::new();
        #[cfg(target_arch = "aarch64")]
        let expected = asid.hardware_root();
        #[cfg(not(target_arch = "aarch64"))]
        let expected = root.into();
        let actual = karch::read_user_page_table();
        assert_eq!(ktask::current_user_page_table_root(), expected);
        assert_eq!(actual, expected);
        #[cfg(target_arch = "aarch64")]
        assert_eq!(karch::user_page_table_root_paddr(actual), root);
        assert!(
            residency
                .snapshot()
                .get(khal::percpu::this_cpu_id().as_usize())
        );
        root.as_usize()
    }
}

pub(in crate::task) fn check_current_root(empty_sentinel: Option<usize>) -> usize {
    let check = {
        let thread = kprocess::current_user_thread();
        let mm = thread
            .process()
            .pinned_address_space_for_teardown_observation()
            .unwrap();
        let mm = mm.lock();
        if let Some(sentinel) = empty_sentinel {
            assert_eq!(mm.vmas().count(), 0);
            assert!(mm.find_vma(sentinel.into()).is_none());
        }
        capture_root(&mm)
    };
    check()
}

#[expect(
    clippy::too_many_lines,
    reason = "CG-028-focused-functions: unittest scenario: one linear exit-tail test script in a \
              file test module"
)]
fn run_case(is_remote: bool, does_exec: bool) -> TestResult {
    let cpu = khal::percpu::this_cpu_id().as_usize();
    let affinity = ktask::current().cpumask();
    let other_cpu = (0..kcpu_id_map::nr_cpus()).find(|id| *id != cpu && affinity.get(*id));
    if is_remote && other_cpu.is_none() {
        unittest::ktest_println!("exit-tail SMP ignored: fewer than two eligible CPUs");
        return TestResult::Ignored;
    }
    let sibling_cpu = if is_remote { other_cpu.unwrap() } else { cpu };
    let parent = kprocess::current_user_thread();
    // Mapping owners stay in the surviving parent's independent MM.
    let sentinel = TestUserValue::from_value(0x1234u64).unwrap();
    let status = TestUserValue::from_value(-1i32).unwrap();
    let sentinel_addr = sentinel.as_user_ptr() as usize;
    let prepared = parent
        .prepare_process_fork(ProcessForkConfig {
            parent: ForkParent::Caller,
            address_space: ForkAddressSpace::Private,
            fs: ForkFs::Private,
            signal_actions: ForkSignalActions::Private,
            fd_table: ForkFdTable::Private,
            namespace_flags: kns::NamespaceFlags::empty(),
            exit_signal: Some(ksignal::Signo::SIGCHLD),
        })
        .unwrap();
    let pid = prepared.process().pid();
    if does_exec {
        let root = executable_root(kexec::test_executable_bytes());
        let fs = prepared.process().fs_context().unwrap();
        let mut fs = fs.lock();
        fs.set_root(root.clone()).unwrap();
        fs.set_pwd(root).unwrap();
    }
    assert_ne!(
        prepared.process().mm_id().unwrap(),
        parent.process().mm_id().unwrap()
    );
    let root = prepared.page_table_root();
    let (observed, residency, check_root) = {
        let mm = prepared
            .process()
            .pinned_address_space_for_teardown_observation()
            .unwrap();
        let guard = mm.lock();
        assert!(guard.find_vma(sentinel_addr.into()).is_some());
        let residency = guard.cpu_residency().clone();
        let check_root = capture_root(&guard);
        (Arc::downgrade(&mm), residency, check_root)
    };
    let stages = Arc::new(Stages::new());
    let child_stages = stages.clone();
    let child_mm = observed.clone();
    let (thread, number) = prepared.into_parts();
    let mut task = TaskInner::new_user(
        move || {
            let leader = kprocess::current_user_thread();
            let process = leader.process();
            let original_id = process.mm_id().unwrap();
            let original_root = check_current_root(None);
            let mut current_id = original_id;
            let mut current_root = original_root;
            let mut context = khal::uspace::UserContext::new(0x1234, 0x8000usize.into(), 0);
            let mut observe_boundary = || {
                assert_eq!(khal::percpu::this_cpu_id().as_usize(), cpu);
                assert_eq!(kprocess::scheduler::process_task_count(process), 1);
                if does_exec {
                    assert_eq!(
                        exec_owned(
                            &mut context,
                            "/app".into(),
                            vec!["app".into()],
                            vec![],
                            None
                        ),
                        Ok(0)
                    );
                    assert!(!leader.is_exiting());
                    let next_id = process.mm_id().unwrap();
                    let next_root = check_current_root(None);
                    assert_ne!(next_id, original_id);
                    assert_ne!(next_id, current_id);
                    assert_ne!(next_root, original_root);
                    assert_ne!(next_root, current_root);
                    current_id = next_id;
                    current_root = next_root;
                }
                let mm = child_mm.upgrade().expect("unfinished sibling lost M");
                let mm = mm.lock();
                assert_eq!(mm.mm_id(), original_id);
                if does_exec {
                    assert_eq!(mm.vmas().count(), 0);
                } else {
                    assert_eq!(process.mm_id().unwrap(), original_id);
                    assert!(mm.find_vma(sentinel_addr.into()).is_some());
                }
            };
            let prepared = leader.prepare_thread_clone().unwrap();
            assert_eq!(prepared.process().pid(), process.pid());
            let tid = prepared.tid();
            let sibling_root = prepared.page_table_root();
            let (thread, number) = prepared.into_parts();
            let sibling_stages = child_stages.clone();
            let sibling_check_root = check_root.clone();
            let mut task = TaskInner::new_user(
                move || {
                    sibling_stages.wait_command(START);
                    posix_process::do_exit(0, false);
                    sibling_stages.acknowledge(FIRST_PARKED);
                    sibling_stages.wait_command(RESUME_FIRST);
                    sibling_check_root();
                    assert_eq!(khal::percpu::this_cpu_id().as_usize(), cpu);
                    if is_remote {
                        assert!(!residency.snapshot().get(sibling_cpu));
                        assert!(ktask::set_current_affinity(ktask::KCpuMask::one_shot(
                            sibling_cpu
                        )));
                        assert_eq!(khal::percpu::this_cpu_id().as_usize(), sibling_cpu);
                        sibling_check_root();
                        assert!(residency.snapshot().get(sibling_cpu));
                        // Keep executing under M while the leader observes the
                        // second boundary. IRQs must service remote shootdowns.
                        let _guard = kspin::NoPreempt::new();
                        assert!(karch::local_irq_enabled());
                        let deadline = khal::time::monotonic_time() + TimeSpan::from_secs(5);
                        sibling_stages.acknowledge(WINDOW_HELD);
                        while sibling_stages.command.load(Ordering::Acquire) < END_WINDOW {
                            assert!(
                                khal::time::monotonic_time() < deadline,
                                "remote tail timeout: wait end window; {}",
                                sibling_stages.diagnostics(),
                            );
                            core::hint::spin_loop();
                        }
                    }
                    sibling_stages.acknowledge(SECOND_PARKED);
                    sibling_stages.wait_finish();
                    sibling_check_root();
                    assert_eq!(khal::percpu::this_cpu_id().as_usize(), sibling_cpu);
                },
                "exit-tail-sibling".into(),
                number,
                thread,
            );
            task.ctx_mut().set_page_table_root(sibling_root);
            task.set_cpumask(ktask::KCpuMask::one_shot(cpu));
            let sibling = kprocess::start_user_task(task);
            assert_eq!(kprocess::scheduler::process_task_count(process), 2);
            if does_exec {
                let mut rejected_context =
                    khal::uspace::UserContext::new(0x1234, 0x8000usize.into(), 0);
                let before = format!("{rejected_context:?}");
                let target = memspace::MmSpace::new_user_empty().unwrap();
                // SAFETY: the target preserves kernel mappings, no MM/user-access
                // window is held, and count two rejects before any activation.
                let rejected = unsafe { kprocess::replace_current_address_space(target, false) };
                assert_eq!(rejected, Err(kerrno::KError::InvalidInput));
                // Multi-thread exec is now handled through `de_thread()` and
                // no longer rejected up front; a missing path still fails while
                // resolving the image, before any thread-group or MM change.
                assert_eq!(
                    exec_owned(
                        &mut rejected_context,
                        "/missing".into(),
                        vec![],
                        vec![],
                        None
                    ),
                    Err(kerrno::KError::NotFound)
                );
                assert_eq!(format!("{rejected_context:?}"), before);
                assert_eq!(process.mm_id().unwrap(), original_id);
                assert_eq!(check_current_root(None), original_root);
            }
            child_stages.advance(LeaderStage::Prepared);
            child_stages.command(START);
            child_stages.wait_parked(FIRST_PARKED, &sibling);
            assert_eq!(sibling.cpu_id().as_usize(), cpu);
            assert!(kprocess::scheduler::task_by_tid(tid).is_err());
            // Boundary one: B is unpublished and has actually blocked.
            observe_boundary();
            child_stages.advance(LeaderStage::FirstBoundaryChecked);
            child_stages.command(RESUME_FIRST);
            if is_remote {
                child_stages.wait_observed("leader wait remote window held", &sibling, || {
                    child_stages.acknowledgement.load(Ordering::Acquire) == WINDOW_HELD
                });
                assert_eq!(sibling.state(), TaskState::Running);
                assert_eq!(sibling.cpu_id().as_usize(), sibling_cpu);
                // Boundary two: B stays on the other CPU with IRQs enabled.
                observe_boundary();
                child_stages.advance(LeaderStage::SecondBoundaryChecked);
                child_stages.command(END_WINDOW);
            }
            child_stages.wait_parked(SECOND_PARKED, &sibling);
            if !is_remote {
                // Same-CPU boundary two uses another real blocked interval.
                observe_boundary();
                child_stages.advance(LeaderStage::SecondBoundaryChecked);
            }
            posix_process::do_exit(0, false);
            child_stages
                .has_leader_exited
                .store(true, Ordering::Release);
            child_stages.advance(LeaderStage::ExitPublished);
            child_stages.wait_finish();
            assert_eq!(check_current_root(Some(sentinel_addr)), current_root);
            assert_eq!(khal::percpu::this_cpu_id().as_usize(), cpu);
            child_stages.wait_observed("leader wait sibling exited", &sibling, || {
                sibling.state() == TaskState::Exited
            });
            assert_eq!(sibling.join(), 0);
            drop(sibling);
        },
        "exit-tail-leader".into(),
        number,
        thread,
    );
    task.ctx_mut().set_page_table_root(root);
    task.set_cpumask(ktask::KCpuMask::one_shot(cpu));
    let child = kprocess::start_user_task(task);
    stages.wait_leader_parked(&child);
    assert_eq!(child.cpu_id().as_usize(), cpu);
    assert_eq!(
        crate::task::wait::sys_waitpid(pid as i32, status.as_user_ptr(), 0),
        Ok(pid as isize)
    );
    assert_eq!(status.read(), 0);
    {
        // This temporary structural observation adds no active MM user.
        let mm = observed.upgrade().expect("held exit tails lost their MM");
        assert_eq!(mm.lock().vmas().count(), 0);
    }
    stages.finish();
    stages.wait_observed("parent wait leader exited", &child, || {
        child.state() == TaskState::Exited
    });
    assert_eq!(child.join(), 0);
    drop(child);
    bounded_wait_labeled("parent wait old mm dropped", || {
        observed.upgrade().is_none()
    });
    assert_eq!(sentinel.read(), 0x1234);
    TestResult::Ok
}

#[def_test(serial)]
fn exit_tail_protocol_preserves_early_notifications_and_broadcasts_finish() {
    let early = Stages::new();
    assert!(!early.command(START));
    early.wait_command(START);
    assert_eq!(early.finish(), 0);
    early.wait_finish();

    let stages = Arc::new(Stages::new());
    let child_stages = stages.clone();
    let command_waiter = ktask::spawn(move || child_stages.wait_command(START));
    stages.wait_observed("protocol command listener", &command_waiter, || {
        command_waiter.state() == TaskState::Blocked
    });
    // Delivery, not elapsed time, proves the waiter had registered a listener.
    let was_command_delivered = stages.command(START);
    stages.wait_observed("protocol command waiter exited", &command_waiter, || {
        command_waiter.state() == TaskState::Exited
    });
    assert_eq!(command_waiter.join(), 0);
    assert!(was_command_delivered);

    let entered = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicUsize::new(0));
    let waiters = core::array::from_fn::<_, 2, _>(|_| {
        let stages = stages.clone();
        let entered = entered.clone();
        let finished = finished.clone();
        ktask::spawn(move || {
            entered.fetch_add(1, Ordering::Release);
            stages.wait_finish();
            finished.fetch_add(1, Ordering::Release);
        })
    });
    stages.wait_observed("protocol two finish listeners", &waiters[0], || {
        entered.load(Ordering::Acquire) == 2
            && waiters
                .iter()
                .all(|task| task.state() == TaskState::Blocked)
    });
    assert_eq!(finished.load(Ordering::Acquire), 0);
    let finish_deliveries = stages.finish();
    for task in &waiters {
        stages.wait_observed("protocol finish waiter exited", task, || {
            task.state() == TaskState::Exited
        });
        assert_eq!(task.join(), 0);
    }
    assert_eq!(finished.load(Ordering::Acquire), 2);
    assert_eq!(finish_deliveries, 2);
}

#[def_test]
fn exit_tail_parent_budget_renews_only_on_stage_progress() {
    let origin = MonotonicInstant::ORIGIN;
    let mut stage = 0;
    let mut deadline = origin + TimeSpan::from_secs(5);
    let first = LeaderStage::Prepared as usize;
    let second = LeaderStage::FirstBoundaryChecked as usize;

    renew_stage_deadline(
        &mut stage,
        0,
        &mut deadline,
        origin + TimeSpan::from_secs(4),
    );
    assert_eq!(deadline, origin + TimeSpan::from_secs(5));
    renew_stage_deadline(
        &mut stage,
        first,
        &mut deadline,
        origin + TimeSpan::from_secs(4),
    );
    assert_eq!(stage, first);
    assert_eq!(deadline, origin + TimeSpan::from_secs(9));
    for now in [5, 8, 9, 10] {
        renew_stage_deadline(
            &mut stage,
            first,
            &mut deadline,
            origin + TimeSpan::from_secs(now),
        );
        assert_eq!(deadline, origin + TimeSpan::from_secs(9));
    }
    renew_stage_deadline(
        &mut stage,
        0,
        &mut deadline,
        origin + TimeSpan::from_secs(10),
    );
    assert_eq!(stage, first);
    assert_eq!(deadline, origin + TimeSpan::from_secs(9));
    renew_stage_deadline(
        &mut stage,
        second,
        &mut deadline,
        origin + TimeSpan::from_secs(10),
    );
    assert_eq!(stage, second);
    assert_eq!(deadline, origin + TimeSpan::from_secs(15));
}

#[def_test(user, serial)]
fn real_exit_tails_survive_clear_and_reap_until_gc() -> TestResult {
    run_case(false, false)
}

#[def_test(user, serial)]
fn real_exit_tails_migrate_and_hold_remote_root_until_gc() -> TestResult {
    run_case(true, false)
}

#[def_test(user, serial)]
fn exit_tail_retains_m_across_two_execs() -> TestResult {
    run_case(false, true)
}

#[def_test(user, serial)]
fn remote_exit_tail_retains_m_across_two_execs() -> TestResult {
    run_case(true, true)
}
