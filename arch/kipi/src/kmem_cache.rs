// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Allocation-free IPI coordination for slab magazine reclaim.
//!
//! The generic IPI queue allocates callback objects and executes them from an
//! IRQ handler, which is unsuitable for allocator reclaim.  This module uses
//! one static sequence, static target bits, and per-target request words
//! instead.  An IPI merely completes a state transition already staged by the
//! initiating task; the initiator drains magazine objects later with IRQs
//! enabled and outside the target handler.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kalloc::{KmemCacheFlushIf, TASK_MAGAZINE_FLUSH_SEQUENCE_MAX};
use kbuild_config::{IPI_IRQ, NR_CPUS};
use kcpu_id_map::for_each_present_logical_cpu;
use khal::percpu::this_cpu_id;
use kirq::TargetCpu;
use kspin::NoPreempt;
use ktime_types::TimeSpan;

use crate::tlb;

/// Serialize full cache reclaim.  A competing shrink falls back to central
/// reclaim rather than spinning while another CPU's magazines are frozen.
static FLUSH_ACTIVE: AtomicBool = AtomicBool::new(false);

/// The sequence owned by the active session, or zero when no session exists.
static ACTIVE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Next nonzero sequence, bounded by kalloc's packed state representation.
static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// CPUs included in the active session.
static FLUSH_TARGETS: [AtomicBool; NR_CPUS] = [const { AtomicBool::new(false) }; NR_CPUS];

/// Per-target IPI request generations.
///
/// A target handler only completes an already-staged request.  It never turns
/// an idle magazine into a frozen one, so a delayed IPI after `end` is benign.
static PENDING_SEQUENCE_BY_CPU: [AtomicU64; NR_CPUS] = [const { AtomicU64::new(0) }; NR_CPUS];

const FLUSH_WARN: TimeSpan = TimeSpan::from_secs(1);

#[kiface::provide]
impl KmemCacheFlushIf {
    fn begin_global_magazine_flush() -> u64 {
        if FLUSH_ACTIVE
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return 0;
        }

        let sequence = next_sequence();
        ACTIVE_SEQUENCE.store(sequence, Ordering::Release);
        clear_session_state();

        let current_cpu = this_cpu_id().as_usize();
        if tlb::all_cpus_started() {
            for_each_present_logical_cpu(|_, cpu, _| {
                FLUSH_TARGETS[cpu.as_usize()].store(true, Ordering::Relaxed);
            });
        } else {
            // During AP bring-up the TLB protocol has not opened the IPI gate
            // yet.  Only the current CPU can safely contribute magazines.
            FLUSH_TARGETS[current_cpu].store(true, Ordering::Relaxed);
        }

        assert!(
            FLUSH_TARGETS[current_cpu].load(Ordering::Relaxed),
            "the current CPU must belong to its slab magazine flush"
        );

        for_each_target_cpu(|cpu| {
            kalloc::request_task_magazine_flush_on_cpu(cpu, sequence);
        });

        // `MagazineReclaimGuard` pins the initiator around this provider call,
        // so no local task can hold an active magazine borrow here.
        assert!(
            kalloc::complete_current_task_magazine_flush(sequence),
            "initiator task magazine access unexpectedly remained active"
        );

        for_each_target_cpu(|cpu| {
            if cpu == current_cpu {
                return;
            }
            PENDING_SEQUENCE_BY_CPU[cpu].store(sequence, Ordering::Release);
            // `notify_cpu()` provides the publish-before-notify ordering
            // required for the target handler to observe this request.
            kirq::notify_cpu(IPI_IRQ, TargetCpu::Specific(cpu));
        });

        wait_for_targets(sequence);
        for_each_target_cpu(|cpu| {
            assert!(
                kalloc::drain_frozen_task_magazines_on_cpu(cpu, sequence),
                "CPU {cpu} magazine drain began before its frozen acknowledgement"
            );
        });
        sequence
    }

    fn end_global_magazine_flush(sequence: u64) {
        assert_eq!(
            ACTIVE_SEQUENCE.load(Ordering::Acquire),
            sequence,
            "ending an inactive or different slab magazine flush"
        );

        // A handler which was delayed after reading this word only attempts to
        // complete the pre-staged pending state.  Once the state below becomes
        // idle it cannot re-freeze it, so clearing before unfreezing is safe.
        for_each_target_cpu(|cpu| {
            PENDING_SEQUENCE_BY_CPU[cpu].store(0, Ordering::Release);
        });
        for_each_target_cpu(|cpu| {
            kalloc::unfreeze_task_magazines_on_cpu(cpu, sequence);
        });
        ACTIVE_SEQUENCE.store(0, Ordering::Release);
        FLUSH_ACTIVE.store(false, Ordering::Release);
    }
}

/// Handle a static slab-reclaim request on the current CPU's IPI path.
///
/// This only reads atomics and advances the current CPU's already-staged
/// state.  In particular it does not borrow magazines, take a class lock,
/// allocate memory, or enter the generic IPI callback queue.
pub(crate) fn handle_flush_request() {
    let _no_preempt = NoPreempt::new();
    let cpu = this_cpu_id().as_usize();
    let sequence = PENDING_SEQUENCE_BY_CPU[cpu].load(Ordering::Acquire);
    if sequence == 0 || ACTIVE_SEQUENCE.load(Ordering::Acquire) != sequence {
        return;
    }

    let _completed = kalloc::complete_current_task_magazine_flush(sequence);
}

fn clear_session_state() {
    for cpu in 0..NR_CPUS {
        FLUSH_TARGETS[cpu].store(false, Ordering::Relaxed);
        PENDING_SEQUENCE_BY_CPU[cpu].store(0, Ordering::Relaxed);
    }
}

fn next_sequence() -> u64 {
    loop {
        let current = NEXT_SEQUENCE.load(Ordering::Relaxed);
        let next = if current == TASK_MAGAZINE_FLUSH_SEQUENCE_MAX {
            1
        } else {
            current + 1
        };
        if NEXT_SEQUENCE
            .compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return next;
        }
    }
}

fn for_each_target_cpu(mut f: impl FnMut(usize)) {
    for (cpu, target) in FLUSH_TARGETS.iter().enumerate() {
        if target.load(Ordering::Relaxed) {
            f(cpu);
        }
    }
}

fn wait_for_targets(sequence: u64) {
    let start = khal::time::monotonic_time();
    let mut warned = false;
    for_each_target_cpu(|cpu| {
        while !kalloc::is_task_magazine_flush_complete(cpu, sequence) {
            let elapsed = khal::time::monotonic_time().saturating_duration_since(start);
            if !warned && elapsed >= FLUSH_WARN {
                warned = true;
                warn!(
                    "slab magazine flush wait: sequence={} stuck_on_target_cpu={}",
                    sequence, cpu
                );
            }
            core::hint::spin_loop();
        }
    });
}
