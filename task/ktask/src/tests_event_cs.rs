// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Isolated event-listener integration regressions, using real kernel guards.

use alloc::{sync::Arc, task::Wake, vec::Vec};
use core::{
    future::Future,
    pin::pin,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::{Context, Waker},
};

use event_listener::Event;
use kcpu_id_map::KCpuMaskExt;
use ktime_types::TimeSpan;
use unittest::{assert, assert_eq, def_test};

fn local_state() -> (bool, bool) {
    (karch::local_irq_enabled(), crate::current().can_preempt(0))
}

#[def_test(serial)]
fn nested_sections_restore_irq_and_preempt_state() {
    let before = local_state();
    let (outer, inner, after_inner) = critical_section::with(|_| {
        let outer = local_state();
        let inner = critical_section::with(|_| Err::<(), _>(local_state()));
        (outer, inner, local_state())
    });
    assert_eq!(outer, (false, false));
    assert_eq!(inner, Err((false, false)));
    assert_eq!(after_inner, (false, false));
    assert_eq!(local_state(), before);
    {
        let _guard = kspin::NoPreemptIrqSave::new();
        critical_section::with(|_| critical_section::with(|_| ()));
        assert_eq!(local_state(), (false, false));
    }
    assert_eq!(local_state(), before);
}

#[def_test(serial)]
fn distinct_cpus_serialize_nested_sections() {
    let cpus = kcpu_id_map::nr_cpus().min(4);
    if cpus < 2 {
        return unittest::TestResult::Ignored;
    }
    let count = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let invalid = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for cpu in 0..cpus {
        let (count, active, invalid, seen) =
            (count.clone(), active.clone(), invalid.clone(), seen.clone());
        let task = crate::prepare_task(crate::TaskInner::new_pidless_kthread(
            move || {
                seen.fetch_or(
                    1 << khal::percpu::this_cpu_id().as_usize(),
                    Ordering::Relaxed,
                );
                for _ in 0..4000 {
                    critical_section::with(|_| {
                        if active.fetch_add(1, Ordering::Relaxed) != 0 {
                            invalid.store(true, Ordering::Relaxed);
                        }
                        critical_section::with(|_| {
                            // Separate atomic load/store detects lost exclusion without
                            // introducing undefined behavior when the provider is wrong.
                            let value = count.load(Ordering::Relaxed);
                            core::hint::spin_loop();
                            count.store(value + 1, Ordering::Relaxed);
                        });
                        active.fetch_sub(1, Ordering::Relaxed);
                    });
                }
            },
            "event-cs-contender".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        task.set_cpumask(crate::KCpuMask::one_shot_logical(cpu.into()));
        tasks.push(task);
    }
    for task in &tasks {
        crate::activate_task(task);
    }
    for task in &tasks {
        assert_eq!(task.join(), 0);
    }
    assert_eq!(seen.load(Ordering::Relaxed), (1 << cpus) - 1);
    assert!(!invalid.load(Ordering::Relaxed));
    assert_eq!(count.load(Ordering::Relaxed), cpus * 4000);
}

struct IrqWake {
    calls: AtomicUsize,
    valid: AtomicBool,
}

impl Wake for IrqWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let valid = critical_section::with(|_| {
            local_state() == (false, false) && kirq::context::is_in_interrupt_context()
        });
        self.valid.store(valid, Ordering::Release);
        self.calls.fetch_add(1, Ordering::Release);
    }
}

#[def_test(serial)]
fn real_timer_irq_notifies_after_outer_section_exits() {
    let _pin = kspin::NoPreempt::new();
    assert!(karch::local_irq_enabled());
    let event = Arc::new(Event::new());
    let wake = Arc::new(IrqWake {
        calls: AtomicUsize::new(0),
        valid: AtomicBool::new(false),
    });
    let waker = Waker::from(wake.clone());
    let mut cx = Context::from_waker(&waker);
    let mut listener = pin!(event.listen());
    assert!(listener.as_mut().poll(&mut cx).is_pending());
    let irq_event = event.clone();
    let timer = crate::Timer::new(move |_: crate::TimerControl<'_>, _: crate::Expiration| {
        irq_event.notify(1);
    });
    let no_early_irq = critical_section::with(|_| {
        let deadline = khal::time::monotonic_time() + TimeSpan::from_millis(2);
        // SAFETY: This initialized CPU is pinned, the handler is IRQ-safe,
        // and the owned timer/event outlive dispatch and synchronous cancel.
        let started = unsafe { timer.start(deadline) }.is_ok();
        while khal::time::monotonic_time() < deadline + TimeSpan::from_millis(1) {
            core::hint::spin_loop();
        }
        started && wake.calls.load(Ordering::Acquire) == 0
    });
    let limit = khal::time::monotonic_time() + TimeSpan::from_secs(2);
    while wake.calls.load(Ordering::Acquire) == 0 && khal::time::monotonic_time() < limit {
        core::hint::spin_loop();
    }
    timer.cancel_sync();
    assert!(no_early_irq);
    assert_eq!(wake.calls.load(Ordering::Acquire), 1);
    assert!(wake.valid.load(Ordering::Acquire));
    assert!(listener.as_mut().poll(&mut cx).is_ready());
    assert!(karch::local_irq_enabled());
}

struct SchedulingWake {
    observer: crate::KtaskRef,
    phase: Arc<AtomicUsize>,
    ran: Arc<AtomicBool>,
    valid: AtomicBool,
}

impl Wake for SchedulingWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.phase.store(1, Ordering::Release);
        crate::activate_task(&self.observer);
        crate::current().set_preempt_pending(true);
        crate::check_preempt_pending();
        self.valid.store(
            local_state() == (false, false) && !self.ran.load(Ordering::Acquire),
            Ordering::Release,
        );
        self.phase.store(2, Ordering::Release);
    }
}

#[def_test(serial)]
fn event_waker_defers_reschedule_until_list_is_released() {
    let before = local_state();
    let (observer, wake) = {
        let _pin = kspin::NoPreempt::new();
        let cpu = khal::percpu::this_cpu_id();
        let phase = Arc::new(AtomicUsize::new(0));
        let ran = Arc::new(AtomicBool::new(false));
        let (observer_phase, observer_ran) = (phase.clone(), ran.clone());
        let observer = crate::prepare_task(crate::TaskInner::new_pidless_kthread(
            move || {
                observer_ran.store(true, Ordering::Release);
                let status = i32::from(observer_phase.load(Ordering::Acquire) != 2);
                drop(observer_phase);
                drop(observer_ran);
                crate::exit(status);
            },
            "event-cs-reschedule".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        observer.set_cpumask(crate::KCpuMask::one_shot_logical(cpu));
        let wake = Arc::new(SchedulingWake {
            observer: observer.clone(),
            phase,
            ran,
            valid: AtomicBool::new(false),
        });
        (observer, wake)
    };
    // Pin the notifier to the observer's CPU without holding NoPreempt over
    // notify: only the event-listener critical section may suppress scheduling.
    let saved_affinity = crate::current().cpumask();
    assert!(crate::set_current_affinity(observer.cpumask()));
    let event = Event::new();
    let waker = Waker::from(wake.clone());
    let mut cx = Context::from_waker(&waker);
    let mut listener = pin!(event.listen());
    assert!(listener.as_mut().poll(&mut cx).is_pending());
    event.notify(1);
    let status = observer.join();
    let restored = crate::set_current_affinity(saved_affinity);
    assert!(restored);
    assert_eq!(status, 0);
    assert!(wake.valid.load(Ordering::Acquire));
    assert!(listener.as_mut().poll(&mut cx).is_ready());
    assert_eq!(local_state(), before);
}
