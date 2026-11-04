// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Concrete RNG IRQ notification-tail drain using a fixture-owned native IRQ.
//!
//! The transport and DMA are controlled, not physical device acceptance. The
//! gate owns only atomics and a completion Arc, never ring pointers. Even a
//! negative control that frees rings before IRQ return cannot make the gated
//! tail dereference freed queue memory. The sole request completes normally;
//! this case is not pending-token cancellation coverage.
//! The entered Release publishes the IRQ CPU and notification count to the
//! coordinator's Acquire. The gate's Release/Acquire permits callback return;
//! joins finish all accesses before observations and fixture storage are retired.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering, fence};

use driver_os_api::{DevIrq, IrqOp, IrqResource};
use kerrno::KResult;
use kspin::{SpinNoIrq, SpinNoPreempt};
use subsys_hwrng::{HwrngDevice, completion::RngCompletion};
use unittest::{assert, assert_eq, def_test};
use virtio::mock_virtio::{MockQueue, MockReset, MockTransport};

use super::{MAPPINGS, RINGS, Runtime, UNSHARES};
use crate::{host_irq::NativeIrq, rng_completion::new_rng_completion};

struct TailCompletion {
    inner: Arc<dyn RngCompletion>,
    entered: AtomicBool,
    is_open: AtomicBool,
    irq_notifications: AtomicUsize,
    task_notifications: AtomicUsize,
    irq_cpu: AtomicUsize,
}

impl TailCompletion {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: new_rng_completion(),
            entered: AtomicBool::new(false),
            is_open: AtomicBool::new(false),
            irq_notifications: AtomicUsize::new(0),
            task_notifications: AtomicUsize::new(0),
            irq_cpu: AtomicUsize::new(usize::MAX),
        })
    }
}

impl RngCompletion for TailCompletion {
    fn wait_until(&self, ready: &mut dyn FnMut() -> bool) -> KResult {
        self.inner.wait_until(ready)
    }

    fn notify(&self) {
        self.inner.notify();
        if kirq::context::is_in_hardirq() {
            self.irq_cpu
                .store(khal::percpu::this_cpu_id().as_usize(), Ordering::Relaxed);
            self.irq_notifications.fetch_add(1, Ordering::Relaxed);
            self.entered.store(true, Ordering::Release);
            while !self.is_open.load(Ordering::Acquire) {
                core::hint::spin_loop();
            }
        } else {
            self.task_notifications.fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct OpenGate(Arc<TailCompletion>);

impl Drop for OpenGate {
    fn drop(&mut self) {
        self.0.is_open.store(true, Ordering::Release);
    }
}

fn spawn_on(cpu: usize, action: impl FnOnce() + Send + 'static) -> ktask::KtaskRef {
    let task = ktask::TaskInner::new_kthread(
        action,
        "rng-notify-tail".into(),
        kbuild_config::TASK_STACK_SIZE,
    )
    .expect("test task allocation");
    let mut mask = ktask::KCpuMask::new();
    mask.set(cpu, true);
    task.set_cpumask(mask);
    ktask::spawn_task(task)
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
    while !predicate() {
        ktask::yield_now();
    }
}

/// Publish the sole submitted request as a normal eight-byte completion.
///
/// # Safety
/// The caller retains the live queue and its single outstanding request, has
/// observed its reader blocked, and has not dispatched IRQ or started stop.
unsafe fn complete_request(queue: &MockQueue) -> bool {
    if queue.size != 8 || queue.available == 0 || queue.used == 0 {
        return false;
    }
    let available = queue.available as *const u16;
    let available_index = available.wrapping_add(1);
    // SAFETY: The retained ring has an aligned u16 index at offset two.
    let index = unsafe { available_index.read_volatile() };
    if index != 1 {
        return false;
    }
    let available_head = available.wrapping_add(2);
    // SAFETY: The sole published entry occupies slot zero in the retained ring.
    let token = unsafe { available_head.read_volatile() };
    if usize::from(token) >= queue.size {
        return false;
    }
    let used = queue.used as *mut u8;
    let used_id = used.wrapping_add(4).cast::<u32>();
    // SAFETY: The first used element has an aligned u32 id at offset four.
    unsafe { used_id.write_volatile(u32::from(token)) };
    let used_length = used.wrapping_add(8).cast::<u32>();
    // SAFETY: Offset eight is its length; eight bytes fit the initialized RNG buffer.
    unsafe { used_length.write_volatile(8) };
    fence(Ordering::Release);
    let used_index = used.wrapping_add(2).cast::<u16>();
    // SAFETY: Publish the completed element through the aligned used index.
    unsafe { used_index.write_volatile(1) };
    true
}

#[derive(Debug)]
struct TailObservation {
    pub submitted: bool,
    pub tail_entered: bool,
    pub reader_exit: Option<i32>,
    pub reader_result: Option<KResult<usize>>,
    pub release_wait_observed: bool,
    pub release_blocked: bool,
    pub returned_early: bool,
    pub rings_retained: bool,
    pub completed_unmapped: bool,
    pub dispatcher_exit: Option<i32>,
    pub closer_exit: Option<i32>,
    pub stop_succeeded: bool,
    pub released_once: bool,
    pub all_resources_released: bool,
    pub unshared_once: bool,
    pub irq_notifications: usize,
    pub task_notifications: usize,
    pub irq_cpu: usize,
    pub release_cpu: usize,
    pub reset_confirmed: bool,
}

/// Caller supplies an isolated NativeIrq resource and its synthetic dispatcher,
/// pins the coordinator to CPU0, and retains the fixture through this call.
fn exercise(
    resource: IrqResource,
    dispatch: impl FnOnce() + Send + 'static,
) -> Result<TailObservation, &'static str> {
    let rings = RINGS.load(Ordering::Relaxed);
    let mappings = MAPPINGS.load(Ordering::Relaxed);
    let unshares = UNSHARES.load(Ordering::Relaxed);
    let completion = TailCompletion::new();
    let gate = OpenGate(completion.clone());
    let about_to_release = Arc::new(AtomicBool::new(false));
    let releases = Arc::new(AtomicUsize::new(0));
    let release_cpu = Arc::new(AtomicUsize::new(usize::MAX));
    let queue = Arc::new(SpinNoIrq::new(MockQueue::default()));
    let reset = Arc::new(MockReset::default());
    let mut transport = MockTransport::new_rng();
    transport.queue = Some(queue.clone());
    transport.reset = Some(reset.clone());
    let interrupt = transport.interrupt_status.clone();
    let runtime = Runtime::try_new_with_irq_factory(
        transport,
        virtio::StaticHal::default(),
        completion.clone(),
        {
            let about_to_release = about_to_release.clone();
            let releases = releases.clone();
            let release_cpu = release_cpu.clone();
            move |handler| {
                let token = NativeIrq
                    .request_irq(resource, handler)
                    .map_err(|_| driver_base::DriverError::Io)?;
                Ok(DevIrq::new(
                    resource.virq(),
                    Some(Arc::new(move || {
                        release_cpu
                            .store(khal::percpu::this_cpu_id().as_usize(), Ordering::Relaxed);
                        about_to_release.store(true, Ordering::Release);
                        NativeIrq.release_irq(resource, token);
                        releases.fetch_add(1, Ordering::Release);
                    })),
                ))
            }
        },
        ktask::yield_now,
    )
    .map_err(|_| "native RNG construction failed")?;
    let result = Arc::new(SpinNoPreempt::new(None));
    let reader = spawn_on(0, {
        let runtime = runtime.clone();
        let result = result.clone();
        move || {
            let observed = runtime.read(&mut [0xa5; 8]);
            *result.lock() = Some(observed);
        }
    });
    wait_until(|| {
        (MAPPINGS.load(Ordering::Relaxed) == mappings + 1
            && reader.state() == ktask::TaskState::Blocked)
            || reader.state() == ktask::TaskState::Exited
    });
    let submitted = MAPPINGS.load(Ordering::Relaxed) == mappings + 1
        && reader.state() == ktask::TaskState::Blocked;
    let published = if submitted {
        // SAFETY: The sole reader is blocked on its live request; no dispatcher
        // exists yet and runtime is retained until all worker tasks retire.
        unsafe { complete_request(&queue.lock()) }
    } else {
        false
    };
    let dispatcher = published.then(|| {
        interrupt.store(1, Ordering::Release);
        spawn_on(1, dispatch)
    });
    if let Some(dispatcher) = &dispatcher {
        wait_until(|| {
            completion.entered.load(Ordering::Acquire)
                || dispatcher.state() == ktask::TaskState::Exited
        });
    }
    let tail_entered = completion.entered.load(Ordering::Acquire);
    runtime.close_reads();
    let reader_exit = Some(reader.join());
    let reader_result = *result.lock();
    let stop_succeeded = Arc::new(AtomicBool::new(false));
    let closer = spawn_on(0, {
        let runtime = runtime.clone();
        let stop_succeeded = stop_succeeded.clone();
        move || {
            stop_succeeded.store(
                runtime.finish_stop(ktask::yield_now).is_ok(),
                Ordering::Release,
            );
        }
    });
    wait_until(|| {
        (about_to_release.load(Ordering::Acquire) && closer.state() == ktask::TaskState::Blocked)
            || releases.load(Ordering::Acquire) != 0
            || closer.state() == ktask::TaskState::Exited
    });
    let release_wait_observed = about_to_release.load(Ordering::Acquire);
    let release_blocked =
        about_to_release.load(Ordering::Acquire) && closer.state() == ktask::TaskState::Blocked;
    let returned_early =
        releases.load(Ordering::Acquire) != 0 || closer.state() == ktask::TaskState::Exited;
    let rings_retained = RINGS.load(Ordering::Relaxed) == rings + 2;
    let completed_unmapped = MAPPINGS.load(Ordering::Relaxed) == mappings
        && UNSHARES.load(Ordering::Relaxed) == unshares + 1;
    let reset_confirmed = reset.confirmations.load(Ordering::Relaxed) == 1;
    drop(gate);
    let dispatcher_exit = dispatcher.as_ref().map(|task| task.join());
    let closer_exit = Some(closer.join());
    let stop_succeeded = stop_succeeded.load(Ordering::Acquire);
    drop(runtime);
    Ok(TailObservation {
        submitted,
        tail_entered,
        reader_exit,
        reader_result,
        release_wait_observed,
        release_blocked,
        returned_early,
        rings_retained,
        completed_unmapped,
        dispatcher_exit,
        closer_exit,
        stop_succeeded,
        released_once: releases.load(Ordering::Acquire) == 1,
        all_resources_released: RINGS.load(Ordering::Relaxed) == rings
            && MAPPINGS.load(Ordering::Relaxed) == mappings,
        unshared_once: UNSHARES.load(Ordering::Relaxed) == unshares + 1,
        irq_notifications: completion.irq_notifications.load(Ordering::Relaxed),
        task_notifications: completion.task_notifications.load(Ordering::Relaxed),
        irq_cpu: completion.irq_cpu.load(Ordering::Relaxed),
        release_cpu: release_cpu.load(Ordering::Relaxed),
        reset_confirmed,
    })
}

struct Affinity(ktask::KCpuMask);

impl Drop for Affinity {
    fn drop(&mut self) {
        core::assert!(ktask::set_current_affinity(self.0));
    }
}

#[def_test(serial)]
fn concrete_rng_finish_stop_waits_for_native_irq_notification_tail() {
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let _affinity = Affinity(ktask::current().cpumask());
    let mut mask = ktask::KCpuMask::new();
    mask.set(0, true);
    assert!(ktask::set_current_affinity(mask));
    assert_eq!(khal::percpu::this_cpu_id().as_usize(), 0);
    const TEST_VIRQ: usize = 0x7f00_1001;
    assert!(kirq::descriptor(TEST_VIRQ).is_none());
    assert_eq!(
        kirq::try_map(kirq::IrqDesc::from_virq(TEST_VIRQ)),
        Ok(TEST_VIRQ)
    );
    let resource = IrqResource::plain(
        driver_os_api::Virq::new(TEST_VIRQ),
        driver_os_api::IrqTrigger::Unknown(0),
    );
    let peer_calls = Arc::new(AtomicUsize::new(0));
    let peer_token = NativeIrq
        .request_irq(
            resource,
            Arc::new({
                let peer_calls = peer_calls.clone();
                move || {
                    peer_calls.fetch_add(1, Ordering::Relaxed);
                    driver_os_api::IrqAction::Handled
                }
            }),
        )
        .unwrap();
    let peer = DevIrq::new(
        resource.virq(),
        Some(Arc::new(move || {
            NativeIrq.release_irq(resource, peer_token)
        })),
    );
    let dispatched = Arc::new(AtomicBool::new(false));
    let observations = exercise(resource, {
        let dispatched = dispatched.clone();
        move || dispatched.store(kirq::synthetic_irq::dispatch(TEST_VIRQ), Ordering::Release)
    });
    let peer_before = peer_calls.load(Ordering::Acquire);
    let second_dispatch = kirq::synthetic_irq::dispatch(TEST_VIRQ);
    let peer_after = peer_calls.load(Ordering::Acquire);
    drop(peer);
    let descriptor_removed = kirq::descriptor(TEST_VIRQ).is_none();
    let observations = observations.unwrap();
    assert!(observations.submitted && observations.tail_entered);
    assert_eq!(observations.reader_exit, Some(0));
    assert!(matches!(
        observations.reader_result,
        Some(Ok(8) | Err(kerrno::KError::BadState))
    ));
    assert!(!observations.returned_early);
    assert!(observations.release_wait_observed && observations.release_blocked);
    assert!(observations.reset_confirmed && observations.rings_retained);
    assert!(observations.completed_unmapped && observations.unshared_once);
    assert_eq!(observations.dispatcher_exit, Some(0));
    assert_eq!(observations.closer_exit, Some(0));
    assert!(observations.stop_succeeded && observations.released_once);
    assert!(observations.all_resources_released);
    assert_eq!(observations.irq_notifications, 1);
    assert_eq!(observations.task_notifications, 1);
    assert_eq!(observations.irq_cpu, 1);
    assert_eq!(observations.release_cpu, 0);
    assert!(dispatched.load(Ordering::Acquire));
    assert_eq!(peer_before, 1);
    assert!(second_dispatch);
    assert_eq!(peer_after, 2);
    assert!(descriptor_removed);
}
