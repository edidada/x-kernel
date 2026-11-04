// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Native adapter SMP acceptance, not physical IRQ/controller acceptance.
//! Explicit predicates establish ordering; the guest runner owns the watchdog.
//! Callback storage is independent of the release-side sentinel, so
//! bypassing core drain can fail this test without freeing callback memory.
//! The entered Release publishes CPU/context observations to the coordinator's
//! Acquire. Opening the gate uses Release/Acquire to permit callback return;
//! joining workers then bounds all callback and release-side resource lifetimes.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::*;

const TEST_VIRQ: usize = 0x7f00_1000;

fn wait_for(mut predicate: impl FnMut() -> bool) {
    while !predicate() {
        ktask::yield_now();
    }
}

fn spawn_on(cpu: usize, action: impl FnOnce() + Send + 'static) -> ktask::KtaskRef {
    let task = ktask::TaskInner::new_kthread(
        action,
        "native-irq-tail".into(),
        kbuild_config::TASK_STACK_SIZE,
    )
    .expect("test task allocation");
    let mut mask = ktask::KCpuMask::new();
    mask.set(cpu, true);
    task.set_cpumask(mask);
    ktask::spawn_task(task)
}

struct Affinity(ktask::KCpuMask);

impl Drop for Affinity {
    fn drop(&mut self) {
        core::assert!(ktask::set_current_affinity(self.0));
    }
}

struct OpenGateOnDrop(Arc<AtomicBool>);

impl Drop for OpenGateOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

struct Sentinel(Arc<AtomicBool>);

impl Drop for Sentinel {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

struct ActionOwner {
    resource: IrqResource,
    token: IrqHandlerToken,
}

#[derive(Default)]
struct CallbackTail {
    entered: AtomicBool,
    gate: Arc<AtomicBool>,
    cpu: AtomicUsize,
    is_hardirq: AtomicBool,
    calls: AtomicUsize,
}

impl CallbackTail {
    fn handler(self: &Arc<Self>) -> IrqHandlerFn {
        let tail = self.clone();
        Arc::new(move || {
            tail.calls.fetch_add(1, Ordering::Relaxed);
            tail.cpu
                .store(khal::percpu::this_cpu_id().as_usize(), Ordering::Relaxed);
            tail.is_hardirq
                .store(kirq::context::is_in_hardirq(), Ordering::Relaxed);
            tail.entered.store(true, Ordering::Release);
            while !tail.gate.load(Ordering::Acquire) {
                core::hint::spin_loop();
            }
            IrqAction::Handled
        })
    }
}

impl Drop for ActionOwner {
    fn drop(&mut self) {
        NativeIrq.release_irq(self.resource, self.token);
    }
}

#[def_test(serial)]
fn native_irq_smp_release_waits_for_callback_tail_and_preserves_shared_peer() {
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let _affinity = Affinity(ktask::current().cpumask());
    let mut coordinator_mask = ktask::KCpuMask::new();
    coordinator_mask.set(0, true);
    assert!(ktask::set_current_affinity(coordinator_mask));
    assert_eq!(khal::percpu::this_cpu_id().as_usize(), 0);
    assert!(kirq::descriptor(TEST_VIRQ).is_none());
    assert_eq!(
        kirq::try_map(kirq::IrqDesc::from_virq(TEST_VIRQ)),
        Ok(TEST_VIRQ)
    );
    let resource = IrqResource::plain(Virq::new(TEST_VIRQ), driver_os_api::IrqTrigger::Unknown(0));
    let tail = Arc::new(CallbackTail::default());
    let destroyed = Arc::new(AtomicBool::new(false));
    let peer_calls = Arc::new(AtomicUsize::new(0));
    let token = NativeIrq.request_irq(resource, tail.handler()).unwrap();
    let owner = ActionOwner { resource, token };
    let peer = ActionOwner {
        resource,
        token: NativeIrq
            .request_irq(
                resource,
                Arc::new({
                    let peer_calls = peer_calls.clone();
                    move || {
                        peer_calls.fetch_add(1, Ordering::Relaxed);
                        IrqAction::Handled
                    }
                }),
            )
            .unwrap(),
    };
    let gate_cleanup = OpenGateOnDrop(tail.gate.clone());
    let dispatch_ok = Arc::new(AtomicBool::new(false));
    let dispatcher = spawn_on(1, {
        let dispatch_ok = dispatch_ok.clone();
        move || dispatch_ok.store(kirq::synthetic_irq::dispatch(TEST_VIRQ), Ordering::Release)
    });
    wait_for(|| tail.entered.load(Ordering::Acquire));
    let about_to_release = Arc::new(AtomicBool::new(false));
    let returned = Arc::new(AtomicBool::new(false));
    let release_cpu = Arc::new(AtomicUsize::new(usize::MAX));
    let releaser = spawn_on(0, {
        let about_to_release = about_to_release.clone();
        let returned = returned.clone();
        let release_cpu = release_cpu.clone();
        let sentinel = Sentinel(destroyed.clone());
        move || {
            release_cpu.store(khal::percpu::this_cpu_id().as_usize(), Ordering::Relaxed);
            about_to_release.store(true, Ordering::Release);
            drop(owner);
            drop(sentinel);
            returned.store(true, Ordering::Release);
        }
    });
    wait_for(|| {
        about_to_release.load(Ordering::Acquire)
            && (releaser.state() == ktask::TaskState::Blocked
                || returned.load(Ordering::Acquire)
                || releaser.state() == ktask::TaskState::Exited)
    });
    let blocked = releaser.state() == ktask::TaskState::Blocked;
    let returned_early = returned.load(Ordering::Acquire);
    let destroyed_early = destroyed.load(Ordering::Acquire);
    drop(gate_cleanup);
    let dispatch_exit = dispatcher.join();
    let release_exit = releaser.join();
    let removed_again = matches!(
        kirq::try_free_irq_action(
            kirq::IrqSpec::PlainVirq(TEST_VIRQ),
            match token {
                IrqHandlerToken::SharedAction(id) => kirq::IrqActionToken::new(id),
                _ => unreachable!("NativeIrq returns a shared action token"),
            },
        ),
        Ok(None)
    );
    let peer_before = peer_calls.load(Ordering::Acquire);
    let second_dispatch = kirq::synthetic_irq::dispatch(TEST_VIRQ);
    let peer_after = peer_calls.load(Ordering::Acquire);
    drop(peer);
    assert!(!returned_early && !destroyed_early);
    assert!(blocked);
    assert_eq!(dispatch_exit, 0);
    assert_eq!(release_exit, 0);
    assert!(removed_again);
    assert!(dispatch_ok.load(Ordering::Acquire) && second_dispatch);
    assert_eq!(tail.cpu.load(Ordering::Acquire), 1);
    assert_eq!(release_cpu.load(Ordering::Acquire), 0);
    assert!(tail.is_hardirq.load(Ordering::Acquire));
    assert_eq!(tail.calls.load(Ordering::Acquire), 1);
    assert_eq!(peer_before, 1);
    assert_eq!(peer_after, 2);
    assert!(destroyed.load(Ordering::Acquire));
    assert!(kirq::descriptor(TEST_VIRQ).is_none());
}
