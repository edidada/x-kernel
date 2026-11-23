// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Reset-aware x86 guest system sleep coordinator.

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use khal::power::SystemSleepIf;
use ktask::{
    KCpuMask, KtaskRef,
    cpu_park::ParkedCpus,
    freezer::{FreezeCancellation, infrastructure_tasks},
};
use ktime::{RtcSleepAccounting, RtcSleepError};
use ktime_types::TimeSpan;

use super::*;

struct Affinity(KCpuMask);
impl Drop for Affinity {
    fn drop(&mut self) {
        assert!(ktask::set_current_affinity(self.0));
    }
}
struct Platform;
impl Drop for Platform {
    fn drop(&mut self) {
        // SAFETY: before CPU parking there are no remote references. Failed
        // acquisition and early returns drop the parking guard and join its
        // callbacks. On resume the caller drops that guard before this one,
        // so no remote CPU can still use the wake page or saved contexts.
        unsafe { SystemSleepIf::finish() };
    }
}
struct Persistent;
impl PersistentClock for Persistent {
    fn read(&mut self) -> Option<ktime_types::SystemTime> {
        SystemSleepIf::persistent_time()
    }

    fn resolution(&self) -> TimeSpan {
        TimeSpan::from_secs(1)
    }
}

fn driver_error(error: driver_base::DriverError) -> KError {
    log::warn!("guest suspend device error: {error:?}");
    match error {
        driver_base::DriverError::Unsupported => KError::OperationNotSupported,
        driver_base::DriverError::ResourceBusy => KError::ResourceBusy,
        _ => KError::Io,
    }
}
fn freeze_error(error: SuspendError) -> KError {
    log::warn!("guest suspend preparation error: {error:?}");
    KError::ResourceBusy
}

/// Suspends the guest through the real task/queue, CPU and platform chain.
/// Returns after recovery; no unimplemented device contract is accepted.
///
/// # Errors
/// Reports invalid context, unavailable platform/device support, or preparation
/// failure after rollback. Time-source failure after actual wake is reported
/// after restoring execution; unrecoverable CPU/device restoration is fatal.
///
/// # Panics
/// A failed hardware restoration cannot safely thaw tasks and is fatal.
pub fn suspend_guest() -> KResult<()> {
    if !ktask::is_sleepable_context() {
        return Err(KError::BadState);
    }
    let old_mask = ktask::current().cpumask();
    let boot = LogicalCpuId::new(0);
    if !ktask::set_current_affinity(KCpuMask::one_shot_logical(boot)) {
        return Err(KError::ResourceBusy);
    }
    let _affinity = Affinity(old_mask);
    assert_eq!(khal::percpu::this_cpu_id(), boot);
    let mut devices = kdriver::power::PowerDevices::prepare().map_err(driver_error)?;
    let mut accounting = RtcSleepAccounting::new(
        Persistent,
        TimeSpan::from_secs(86400),
        TimeSpan::from_millis(100),
    )
    .map_err(|_| KError::BadState)?;
    let exemptions: alloc::vec::Vec<KtaskRef> =
        infrastructure_tasks().map_err(|_| KError::ResourceBusy)?;
    let sleep_types = kdriver::power::s3_sleep_types().ok_or(KError::OperationNotSupported)?;
    let task_cancel = FreezeCancellation::new();
    let queue_cancel = kworkng::QueueFreezeCancellation::new();
    let preparation = freeze_system_workqueues(
        SuspendTasks {
            exempt: &exemptions,
            timeout: TimeSpan::from_secs(5),
            cancellation: &task_cancel,
        },
        SuspendWorkqueues {
            queues: &[],
            timeout: TimeSpan::from_secs(5),
            cancellation: &queue_cancel,
        },
    )
    .map_err(freeze_error)?;
    // SAFETY: preparation holds exclusive system-PM admission until thaw.
    // Affinity holds us on the boot CPU. The present-CPU map is fixed after
    // boot, and the backend claims its wake page before publishing any work.
    unsafe { SystemSleepIf::prepare(sleep_types) }?;
    let platform = Platform;
    // Ordinary task/worker producers are frozen; recovery is synchronous
    // and does not depend on pending work in the kernel normal service.
    let queues = kworkng::freeze_system_normal_queue(TimeSpan::from_secs(5), &queue_cancel)
        .map_err(|_| KError::ResourceBusy)?;
    log::info!("guest suspend: tasks and normal queue frozen");
    if let Err(error) = devices.suspend() {
        assert!(
            devices.resume().is_ok(),
            "device preparation rollback failed"
        );
        drop(platform);
        drop(queues);
        drop(preparation);
        return Err(driver_error(error));
    }
    log::info!("guest suspend: device callbacks completed");
    let interval = sleep_interval(&mut accounting);
    assert!(devices.resume().is_ok(), "guest device recovery failed");
    let result = match interval {
        Ok((cpus, result)) => {
            drop(cpus);
            result
        }
        Err(error) => Err(error),
    };
    // Remote callbacks have joined after device recovery; restoring external
    // delivery and thawing producers is now safe.
    drop(platform);
    drop(queues);
    drop(preparation);
    log::info!("guest resume: CPUs, devices, queues and tasks restored");
    result
}

fn sleep_interval(
    accounting: &mut RtcSleepAccounting<Persistent>,
) -> KResult<(ParkedCpus, KResult<()>)> {
    let session = accounting.begin().map_err(|_| KError::Io)?;
    // SAFETY: prepared platform owns reset-aware per-CPU storage; all task and
    // device producers are excluded until this function joins every callback.
    let mut cpus = unsafe { ParkedCpus::acquire_for_system_sleep(TimeSpan::from_secs(5)) }
        .map_err(|_| KError::ResourceBusy)?;
    log::info!(
        "guest suspend: {} remote CPUs acknowledged",
        cpus.remote_cpu_count()
    );
    let result = sleep_with_parked_cpus(&mut cpus, session);
    Ok((cpus, result))
}

// Borrow the guard so every recoverable exit returns it to the coordinator,
// which resumes devices before releasing the remote CPUs.
fn sleep_with_parked_cpus(
    cpus: &mut ParkedCpus,
    mut session: ktime::RtcSleepSession<'_, Persistent>,
) -> KResult<()> {
    let critical = NoPreemptIrqSave::new();
    khal::quiesce_nmi();
    let mut events = ktask::timer_suspend::suspend_local_timer().map_err(|_| KError::BadState)?;
    let clock = match khal::time::suspend_monotonic_clock() {
        Ok(clock) => clock,
        Err(_) => {
            assert!(events.restore_hardware().is_ok());
            assert!(events.resume().is_ok());
            return Err(KError::BadState);
        }
    };
    assert!(session.mark_suspended().is_ok());
    log::info!("guest suspend: entering platform S3");
    // SAFETY: the acknowledged CPU barrier, device exclusions, masked IRQs and
    // paused events establish the platform entry contract.
    let entered = unsafe { SystemSleepIf::enter() };
    assert!(
        // SAFETY: wake returned to the boot CPU with shared exclusions retained.
        unsafe { SystemSleepIf::restore_cpus() }.is_ok(),
        "guest CPU restoration failed"
    );
    assert!(khal::time::resume_monotonic_clock(&clock).is_ok());
    assert!(events.restore_hardware().is_ok());
    drop(critical);
    // SAFETY: affinity is fixed to the boot CPU; clock/controller access is
    // restored and external IRQs stay masked while local accounting runs.
    unsafe { cpus.allow_local_scheduling() };
    let settled: Result<TimeSpan, RtcSleepError> = session.resume_from_rtc();
    drop(session);
    {
        let _critical = NoPreemptIrqSave::new();
        assert!(events.resume().is_ok());
    }
    // Remote callbacks stay held until time is continuous and accounted for.
    // Device IRQs stay externally masked; device producers are still frozen.
    log::info!("guest resume: platform returned, sleep accounting {settled:?}");
    entered.and_then(|_| settled.map(|_| ()).map_err(|_| KError::Io))
}
