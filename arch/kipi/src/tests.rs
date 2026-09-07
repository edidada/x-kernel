// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicUsize, Ordering};

use kcpu_id_map::{LogicalCpuId, for_each_present_logical_cpu, raw_cpu_id};
use unittest::{assert, assert_eq, assert_ne, def_test};

use crate::{IPI_QUEUE_READY, KipiError};

struct RestoreLocalGenericIpi;

impl Drop for RestoreLocalGenericIpi {
    fn drop(&mut self) {
        let _ = crate::enable_ipi();
    }
}

struct WithdrawIpiReadiness {
    cpu: LogicalCpuId,
    was_ready: bool,
}

impl WithdrawIpiReadiness {
    fn new(cpu: LogicalCpuId) -> Self {
        Self {
            cpu,
            was_ready: IPI_QUEUE_READY[cpu.as_usize()].swap(false, Ordering::AcqRel),
        }
    }
}

impl Drop for WithdrawIpiReadiness {
    fn drop(&mut self) {
        IPI_QUEUE_READY[self.cpu.as_usize()].store(self.was_ready, Ordering::Release);
    }
}

// QEMU integration test: keep the boot-owned queue/handler alive, restore
// delivery on every exit, and bound the wait for a real self-IPI.
#[def_test(serial)]
fn test_local_ipi_reenable_resumes_pending_work() {
    static HIT: AtomicUsize = AtomicUsize::new(0);

    let _pin = kspin::NoPreempt::new();
    let cpu = khal::percpu::this_cpu_id();
    assert!(crate::is_ipi_queue_ready(cpu));
    let ready: [bool; kbuild_config::NR_CPUS] =
        core::array::from_fn(|index| IPI_QUEUE_READY[index].load(Ordering::Acquire));
    let _restore = RestoreLocalGenericIpi;
    HIT.store(0, Ordering::Relaxed);
    {
        let _irq = kspin::IrqSave::new();
        assert!(crate::disable_ipi());
        assert!(crate::disable_ipi());
        assert!(!crate::is_ipi_queue_ready(cpu));
        for (index, was_ready) in ready.into_iter().enumerate() {
            if index != cpu.as_usize() {
                assert_eq!(IPI_QUEUE_READY[index].load(Ordering::Acquire), was_ready);
            }
        }
        // Model a producer that passed readiness validation before disable:
        // its enqueue remains valid because local queue storage is retained.
        crate::IPI_EVENT_QUEUE.with_current(|queue| {
            queue.lock().push(
                cpu,
                (|| {
                    HIT.fetch_add(1, Ordering::Release);
                })
                .into(),
            );
        });
        crate::ipi_handler();
        assert_eq!(HIT.load(Ordering::Acquire), 0);
        assert!(crate::enable_ipi());
        assert!(crate::enable_ipi());
        assert!(crate::is_ipi_queue_ready(cpu));
    }

    let start = khal::time::monotonic_time();
    while HIT.load(Ordering::Acquire) == 0
        && khal::time::monotonic_time().saturating_duration_since(start)
            < ktime_types::TimeSpan::from_secs(2)
    {
        core::hint::spin_loop();
    }
    assert_eq!(HIT.load(Ordering::Acquire), 1);
}

#[def_test]
fn test_error_display_messages() {
    assert_eq!(
        alloc::format!("{}", KipiError::InvalidCpuId),
        "Invalid CPU ID"
    );
    assert_eq!(
        alloc::format!("{}", KipiError::TargetCpuNotReady),
        "Target CPU is not ready for IPI"
    );
    assert_eq!(alloc::format!("{}", KipiError::QueueFull), "IPI queue full");
    assert_eq!(
        alloc::format!("{}", KipiError::CallbackFailed),
        "Callback execution failed"
    );
}

#[def_test]
fn test_error_equality() {
    assert_ne!(KipiError::InvalidCpuId, KipiError::CallbackFailed);
}

#[def_test]
fn test_error_debug_format() {
    let text = alloc::format!("{:?}", KipiError::InvalidCpuId);
    assert!(text.contains("InvalidCpuId"));
}

#[def_test]
fn test_run_on_cpu_rejects_non_present_cpu() {
    let mut non_present_cpu = None;
    for logical_cpu_id in 0..kbuild_config::NR_CPUS {
        let logical_cpu_id = LogicalCpuId::new(logical_cpu_id);
        if raw_cpu_id(logical_cpu_id).is_none() {
            non_present_cpu = Some(logical_cpu_id);
            break;
        }
    }

    if let Some(cpu_id) = non_present_cpu {
        assert_eq!(
            crate::run_on_cpu(cpu_id, || {}),
            Err(KipiError::InvalidCpuId)
        );
    }
}

#[def_test(serial)]
fn test_run_on_cpu_rejects_not_ready_present_cpu() {
    let _pin = kspin::NoPreempt::new();
    let current_cpu = khal::percpu::this_cpu_id();
    let mut remote_cpu = None;
    for_each_present_logical_cpu(|_, cpu_id, _| {
        if cpu_id != current_cpu && remote_cpu.is_none() {
            remote_cpu = Some(cpu_id);
        }
    });

    if let Some(cpu_id) = remote_cpu {
        let _readiness = WithdrawIpiReadiness::new(cpu_id);
        let result = crate::run_on_cpu(cpu_id, || {});
        assert_eq!(result, Err(KipiError::TargetCpuNotReady));
    }
}

#[def_test(serial)]
fn test_run_on_each_cpu_does_not_execute_locally_when_remote_not_ready() {
    static HIT: AtomicUsize = AtomicUsize::new(0);

    let _pin = kspin::NoPreempt::new();
    let current_cpu = khal::percpu::this_cpu_id();
    let mut remote_cpu = None;
    for_each_present_logical_cpu(|_, cpu_id, _| {
        if cpu_id != current_cpu && remote_cpu.is_none() {
            remote_cpu = Some(cpu_id);
        }
    });

    if let Some(cpu_id) = remote_cpu {
        let _readiness = WithdrawIpiReadiness::new(cpu_id);
        HIT.store(0, Ordering::SeqCst);
        let result = crate::run_on_each_cpu(|| {
            HIT.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(result, Err(KipiError::TargetCpuNotReady));
        assert_eq!(HIT.load(Ordering::SeqCst), 0);
    }
}

#[def_test(serial)]
fn test_run_on_each_cpu_via_ipi_failure_does_not_leave_local_event() {
    static HIT: AtomicUsize = AtomicUsize::new(0);

    let _pin = kspin::NoPreempt::new();
    let current_cpu = khal::percpu::this_cpu_id();
    let mut remote_cpu = None;
    for_each_present_logical_cpu(|_, cpu_id, _| {
        if cpu_id != current_cpu && remote_cpu.is_none() {
            remote_cpu = Some(cpu_id);
        }
    });

    if let Some(cpu_id) = remote_cpu {
        let _readiness = WithdrawIpiReadiness::new(cpu_id);
        HIT.store(0, Ordering::SeqCst);
        let result = crate::run_on_each_cpu_via_ipi(|| {
            HIT.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(result, Err(KipiError::TargetCpuNotReady));

        crate::ipi_handler();
        assert_eq!(HIT.load(Ordering::SeqCst), 0);
    }
}
