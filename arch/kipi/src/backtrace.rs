// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU-backtrace IPI lifecycle.
//!
//! The irqchip owns the concrete vector selected for
//! [`kirq::IpiKind::CpuBacktrace`]. This module owns registration, per-CPU
//! enablement, readiness publication, and NMI-safe mask delivery, so diagnostic
//! consumers never choose or send a raw SGI themselves.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use kcpu_id_map::KCpuMask;

static CPU_BACKTRACE_REGISTERED: AtomicBool = AtomicBool::new(false);
static CPU_BACKTRACE_DELIVERY: [AtomicU8; kbuild_config::NR_CPUS] =
    [const { AtomicU8::new(CpuBacktraceDelivery::Unavailable as u8) }; kbuild_config::NR_CPUS];

/// Result of initializing the CPU-backtrace IPI on the calling CPU.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuBacktraceDelivery {
    /// The vector is enabled and promoted to NMI delivery.
    Nmi,
    /// The vector is enabled as a regular per-CPU IRQ fallback.
    RegularIrq,
    /// The vector could not be enabled on this CPU.
    Unavailable,
}

/// Returns the calling CPU's currently enabled backtrace delivery mode.
///
/// An unsupported, not-yet-initialized or stopping CPU reports `Unavailable`.
/// This read is lock-free and can be used in IRQ/NMI context.
pub fn cpu_backtrace_delivery() -> CpuBacktraceDelivery {
    delivery_for_cpu(khal::percpu::this_cpu_id().as_usize())
}

fn delivery_for_cpu(cpu: usize) -> CpuBacktraceDelivery {
    match CPU_BACKTRACE_DELIVERY[cpu].load(Ordering::Acquire) {
        n if n == CpuBacktraceDelivery::Nmi as u8 => CpuBacktraceDelivery::Nmi,
        n if n == CpuBacktraceDelivery::RegularIrq as u8 => CpuBacktraceDelivery::RegularIrq,
        _ => CpuBacktraceDelivery::Unavailable,
    }
}

/// Registers the semantic CPU-backtrace handler once during primary boot.
///
/// `kirq::register_nmi` also installs the same handler in the regular IRQ
/// table. Consequently, a platform that cannot promote the vector to NMI can
/// still wake CPUs whose ordinary interrupts remain enabled.
pub fn register_cpu_backtrace_handler(handler: fn()) -> bool {
    let Some(desc) = kirq::ipi_desc(kirq::IpiKind::CpuBacktrace) else {
        return false;
    };
    let handler = Arc::new(move |_| {
        // A sender may have snapshotted readiness before terminal withdrawal.
        // The handler stays installed and rejects delivery once unready.
        if cpu_backtrace_delivery() == CpuBacktraceDelivery::Unavailable {
            return kirq::IrqEvent::HANDLED;
        }
        #[cfg(unittest)]
        if tests::handle_probe() {
            return kirq::IrqEvent::HANDLED;
        }
        handler();
        kirq::IrqEvent::HANDLED
    });
    let registered = kirq::register_nmi(desc, handler);
    if registered {
        CPU_BACKTRACE_REGISTERED.store(true, Ordering::Release);
    }
    registered
}

/// Configures and enables the CPU-backtrace vector on the calling CPU.
///
/// NMI promotion is attempted first. If it is unavailable, the line remains
/// usable as a regular per-CPU IRQ; callers must retain a bounded wait because
/// that fallback cannot interrupt a CPU that has masked ordinary IRQs.
/// Repeated enable is idempotent. The handler and NMI attributes remain
/// installed until terminal stop; must not be called after that CPU has
/// withdrawn its readiness for stopping.
/// Call after global handler registration. Pins the caller and masks ordinary
/// IRQs until controller setup and readiness publication complete; this is not
/// an NMI-context operation.
pub fn enable_cpu_backtrace() -> CpuBacktraceDelivery {
    let _local = kspin::NoPreemptIrqSave::new();
    let current = cpu_backtrace_delivery();
    if current != CpuBacktraceDelivery::Unavailable {
        return current;
    }
    if !CPU_BACKTRACE_REGISTERED.load(Ordering::Acquire) {
        return CpuBacktraceDelivery::Unavailable;
    }
    let Some(desc) = kirq::ipi_desc(kirq::IpiKind::CpuBacktrace) else {
        return CpuBacktraceDelivery::Unavailable;
    };
    let cpu = khal::percpu::this_cpu_id();
    // Readiness also certifies that the NMI-safe irqchip sender can resolve
    // this logical CPU without lazily initializing the CPU map in NMI context.
    if kcpu_id_map::raw_cpu_id(cpu).is_none() {
        return CpuBacktraceDelivery::Unavailable;
    }

    #[cfg(feature = "nmi")]
    let nmi = khal::nmi::mode() != khal::nmi::NmiMode::None && khal::irq::configure_nmi(desc.hwirq);
    #[cfg(not(feature = "nmi"))]
    let nmi = false;
    if kirq::try_enable_percpu_irq(desc).is_err() {
        return CpuBacktraceDelivery::Unavailable;
    }

    let delivery = if nmi {
        CpuBacktraceDelivery::Nmi
    } else {
        CpuBacktraceDelivery::RegularIrq
    };
    // Delivery and readiness are one publication: a reader can never see
    // ready=true with a stale delivery mode or before local hardware enable.
    CPU_BACKTRACE_DELIVERY[cpu.as_usize()].store(delivery as u8, Ordering::Release);
    delivery
}

/// Withdraws dispatch readiness for terminal stop without entering IRQ control.
/// The caller is pinned and has masked CPU-local interrupt delivery.
pub(super) fn withdraw_local_readiness() {
    CPU_BACKTRACE_DELIVERY[khal::percpu::this_cpu_id().as_usize()]
        .store(CpuBacktraceDelivery::Unavailable as u8, Ordering::Release);
}

/// Returns the exact set of CPUs that enabled the CPU-backtrace vector.
///
/// This is lock-free and allocation-free so a watchdog NMI can safely take a
/// readiness snapshot before sending a request.
pub fn cpu_backtrace_ready_mask() -> KCpuMask {
    let mut ready = KCpuMask::new();
    for cpu in 0..kbuild_config::NR_CPUS {
        if delivery_for_cpu(cpu) != CpuBacktraceDelivery::Unavailable {
            ready.set(cpu, true);
        }
    }
    ready
}

/// Sends a CPU-backtrace request to exactly `target_mask`.
///
/// This function may run in NMI context. The irqchip implementation for
/// [`kirq::IpiKind::CpuBacktrace`] must not allocate, log, or acquire locks.
/// Returns `false` if the mask contains a CPU that has not enabled the
/// backtrace vector, or if the irqchip cannot issue the complete set. Some
/// targets may already have been notified on failure. Success means the
/// request was issued, not that the targets acknowledged it.
pub fn send_cpu_backtrace(target_mask: KCpuMask) -> bool {
    if !(target_mask & !cpu_backtrace_ready_mask()).is_empty() {
        return false;
    }
    kirq::send_ipi(
        kirq::IpiKind::CpuBacktrace,
        kirq::TargetCpu::Mask(target_mask),
    )
}

// QEMU integration coverage: real IRQ/NMI delivery cannot be tested by a local
// state-machine unit test. The probe is scoped; the boot-owned IRQ handler
// itself remains installed for the kernel lifetime.
#[cfg(unittest)]
mod tests {
    use core::sync::atomic::AtomicUsize;

    use kcpu_id_map::KCpuMaskExt;
    use khal::percpu::this_cpu_id;
    use kspin::NoPreempt;
    use ktime_types::TimeSpan;
    use unittest::{assert, assert_eq, def_test};

    use super::*;

    static ACTIVE: AtomicBool = AtomicBool::new(false);
    static SENDER: AtomicUsize = AtomicUsize::new(usize::MAX);
    static ARRIVED: AtomicUsize = AtomicUsize::new(0);
    static WITH_FRAME: AtomicUsize = AtomicUsize::new(0);
    #[cfg(feature = "nmi")]
    static BLOCKED: AtomicUsize = AtomicUsize::new(0);
    #[cfg(feature = "nmi")]
    static EXITED: AtomicUsize = AtomicUsize::new(0);
    static RELEASE: AtomicBool = AtomicBool::new(false);
    static RELAY_SENT: AtomicBool = AtomicBool::new(false);

    struct RestoreLocalIpiReadiness {
        cpu: usize,
        backtrace_delivery: CpuBacktraceDelivery,
    }

    impl Drop for RestoreLocalIpiReadiness {
        fn drop(&mut self) {
            CPU_BACKTRACE_DELIVERY[self.cpu]
                .store(self.backtrace_delivery as u8, Ordering::Release);
            crate::IPI_QUEUE_READY[self.cpu].store(true, Ordering::Release);
        }
    }

    // QEMU integration coverage for the software part of terminal quiescence.
    // Restore only the dispatch gates: the controller configuration, periodic
    // NMI source and CPU execution state remain unchanged throughout this test.
    #[def_test(serial)]
    fn test_terminal_ipi_quiesce_avoids_irq_control_and_queue_locks() {
        let _local = kspin::NoPreemptIrqSave::new();
        let cpu = this_cpu_id();
        assert!(crate::is_ipi_queue_ready(cpu));
        let _restore = RestoreLocalIpiReadiness {
            cpu: cpu.as_usize(),
            backtrace_delivery: cpu_backtrace_delivery(),
        };
        let (is_ready, delivery, is_initialized, is_backtrace_rejected) = crate::IPI_EVENT_QUEUE
            .with_current(|queue| {
                let _queue = queue.lock();
                kirq::test_support::with_irq_control_locked(|| {
                    crate::quiesce_local_ipis();
                    // A late Generic IPI must return before it touches the queue.
                    crate::ipi_handler();
                    (
                        crate::is_ipi_queue_ready(cpu),
                        cpu_backtrace_delivery(),
                        queue.is_inited(),
                        !send_cpu_backtrace(KCpuMask::one_shot_logical(cpu)),
                    )
                })
            });
        assert!(!is_ready);
        assert_eq!(delivery, CpuBacktraceDelivery::Unavailable);
        assert!(is_initialized);
        assert!(is_backtrace_rejected);
    }

    struct ProbeGuard;

    impl Drop for ProbeGuard {
        fn drop(&mut self) {
            RELEASE.store(true, Ordering::Release);
            ACTIVE.store(false, Ordering::Release);
        }
    }

    pub(super) fn handle_probe() -> bool {
        if !ACTIVE.load(Ordering::Acquire) {
            return false;
        }
        let cpu = this_cpu_id();
        if cpu.as_usize() == SENDER.load(Ordering::Relaxed) {
            // The sender receives its self-IPI with ordinary IRQs masked,
            // then relays to the peers from actual NMI context.
            let mut targets = cpu_backtrace_ready_mask();
            targets.set_logical(cpu, false);
            RELAY_SENT.store(send_cpu_backtrace(targets), Ordering::Release);
        }
        let bit = 1usize << cpu.as_usize();
        if khal::context::active_exception_context().is_some() {
            WITH_FRAME.fetch_or(bit, Ordering::Release);
        }
        ARRIVED.fetch_or(bit, Ordering::Release);
        true
    }

    fn wait_mask(state: &AtomicUsize, expected: usize) -> bool {
        let start = khal::time::monotonic_time();
        while state.load(Ordering::Acquire) & expected != expected {
            if khal::time::monotonic_time().saturating_duration_since(start)
                >= TimeSpan::from_secs(2)
            {
                return false;
            }
            core::hint::spin_loop();
        }
        true
    }

    #[def_test(serial)]
    fn backtrace_enable_and_delivery() {
        if kirq::ipi_desc(kirq::IpiKind::CpuBacktrace).is_none() {
            return unittest::TestResult::Ignored;
        }
        let _pin = NoPreempt::new();
        let delivery = cpu_backtrace_delivery();
        assert!(delivery != CpuBacktraceDelivery::Unavailable);
        #[cfg(not(feature = "nmi"))]
        assert_eq!(delivery, CpuBacktraceDelivery::RegularIrq);
        let ready = cpu_backtrace_ready_mask();
        for peer in 0..kcpu_id_map::nr_cpus() {
            assert!(ready.get(peer));
        }
        assert_eq!(enable_cpu_backtrace(), delivery);
        assert_eq!(enable_cpu_backtrace(), delivery);
        assert_eq!(cpu_backtrace_ready_mask(), ready);

        SENDER.store(usize::MAX, Ordering::Relaxed);
        ARRIVED.store(0, Ordering::Relaxed);
        WITH_FRAME.store(0, Ordering::Relaxed);
        let probe = ProbeGuard;
        ACTIVE.store(true, Ordering::Release);
        let expected = ready
            .iter_logical()
            .fold(0, |mask, target| mask | (1usize << target.as_usize()));
        let sent = send_cpu_backtrace(ready);
        let received = sent && wait_mask(&ARRIVED, expected);
        drop(probe);

        assert!(received);
        assert_eq!(WITH_FRAME.load(Ordering::Acquire), expected);
    }

    #[cfg(feature = "nmi")]
    #[def_test(serial)]
    fn backtrace_nmi_relays_to_irq_masked_peers() {
        if khal::nmi::mode() == khal::nmi::NmiMode::None {
            log::warn!("CPU-backtrace NMI integration test requires an active NMI mechanism");
            return unittest::TestResult::Ignored;
        }
        let _pin = NoPreempt::new();
        let cpu = this_cpu_id();
        let expected = (1usize << kcpu_id_map::nr_cpus()) - 1;
        let peers = expected & !(1usize << cpu.as_usize());
        let ready = cpu_backtrace_ready_mask();
        for peer in 0..kcpu_id_map::nr_cpus() {
            assert!(ready.get(peer));
        }

        SENDER.store(cpu.as_usize(), Ordering::Relaxed);
        ARRIVED.store(0, Ordering::Relaxed);
        WITH_FRAME.store(0, Ordering::Relaxed);
        BLOCKED.store(0, Ordering::Relaxed);
        EXITED.store(0, Ordering::Relaxed);
        RELEASE.store(false, Ordering::Relaxed);
        RELAY_SENT.store(false, Ordering::Relaxed);
        let probe = ProbeGuard;
        ACTIVE.store(true, Ordering::Release);

        let queued = crate::run_on_each_cpu(move || {
            if this_cpu_id() == cpu {
                return;
            }
            let _irq = kspin::IrqSave::new();
            let bit = 1usize << this_cpu_id().as_usize();
            BLOCKED.fetch_or(bit, Ordering::Release);
            let start = khal::time::monotonic_time();
            while !RELEASE.load(Ordering::Acquire)
                && khal::time::monotonic_time().saturating_duration_since(start)
                    < TimeSpan::from_secs(2)
            {
                core::hint::spin_loop();
            }
            EXITED.fetch_or(bit, Ordering::Release);
        })
        .is_ok();
        let blocked = queued && wait_mask(&BLOCKED, peers);
        let received = if blocked {
            let _irq = kspin::IrqSave::new();
            send_cpu_backtrace(KCpuMask::one_shot_logical(cpu)) && wait_mask(&ARRIVED, expected)
        } else {
            false
        };
        // No peer may have left its IRQ-masked section before the probe
        // completes; otherwise a regular IRQ could produce a false pass.
        let stayed_masked = EXITED.load(Ordering::Acquire) == 0;
        RELEASE.store(true, Ordering::Release);
        let exited = wait_mask(&EXITED, peers);
        drop(probe);

        assert!(blocked);
        assert!(received);
        assert!(RELAY_SENT.load(Ordering::Acquire));
        assert!(stayed_masked);
        assert!(exited);
        assert_eq!(ARRIVED.load(Ordering::Acquire), expected);
        assert_eq!(WITH_FRAME.load(Ordering::Acquire), expected);
    }
}
