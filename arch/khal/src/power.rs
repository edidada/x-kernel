// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU power management.
//!
//! [`halt`] and [`power_off`] are the system power terminals for normal
//! execution contexts. Before entering the bare platform terminal they stop
//! every other CPU through [`SmpStopIf`], so no surviving CPU keeps
//! executing tasks, handling interrupts, or mutating shared state while the
//! system goes down. When the stop provider is unavailable (UP builds) or
//! not ready yet (terminals reached before runtime initialization
//! completes) the stop is a no-op and the terminal proceeds with the
//! calling CPU only.
//!
//! All terminals here are bare endpoints: they never return and never
//! perform higher-level cleanup such as filesystem sync, process teardown,
//! or device removal. Callers must finish such cleanup before invoking
//! them.
//!
//! Panic and crash paths must use [`platform_halt`] or
//! [`platform_power_off`] directly instead of these terminals: the SMP stop
//! protocol is only supported in ordinary execution context. A panic may
//! originate in NMI context and must not wait for cooperative IRQ delivery
//! or the normal stop deadline before reaching the platform terminal.

use kerrno::KResult;
#[cfg(feature = "smp")]
pub use kplat::sys::boot_ap;
pub use kplat::sys::{
    halt as platform_halt, power_off as platform_power_off,
    suspend_to_ram as platform_suspend_to_ram,
};

/// `kiface` for stopping all other CPUs on the terminal path.
///
/// The provider must stop every present CPU except its caller before
/// returning, best-effort and bounded, and must be callable with local
/// interrupts disabled. Callers must remain pinned through the following
/// platform terminal; a provider may leave local interrupts masked and IPI
/// readiness withdrawn. This operation has no resume path.
/// Providers degrade to a no-op while their machinery is not ready yet
/// (for example before IPI delivery is initialized).
///
/// SMP builds use the provider implemented by `kipi`; uniprocessor
/// builds use the local no-op provider below because there are no
/// remote CPUs to stop. Linking `kipi` enables the `smp` feature on
/// this crate, which compiles the fallback out, so exactly one
/// provider is linked in either case.
#[kiface::interface]
pub trait SmpStopIf {
    /// Stops every present CPU except the caller, best-effort and
    /// bounded in time.
    fn stop_other_cpus();
}

#[cfg(not(feature = "smp"))]
#[kiface::provide]
impl SmpStopIf {
    fn stop_other_cpus() {}
}

/// Stops every other CPU, then halts the system with power kept.
///
/// See the module documentation for the terminal contract; this never
/// returns.
pub fn halt() -> ! {
    // Keep the terminal CPU pinned even after the stop provider returns:
    // dropping its own guard must not reschedule before the final park.
    let _terminal = kspin::NoPreempt::new();
    SmpStopIf::stop_other_cpus();
    // Establish the local terminal mask even when the stop provider is a no-op.
    karch::prepare_cpu_stop();
    // Quiesce the terminal CPU's local NMI source so an NMI-driven
    // hard-lockup watchdog cannot wake it from the final stop loop and
    // panic into a power-off.
    crate::quiesce_nmi();
    platform_halt()
}

/// Stops every other CPU, then powers off the system.
///
/// See the module documentation for the terminal contract; this never
/// returns.
pub fn power_off() -> ! {
    let _terminal = kspin::NoPreempt::new();
    SmpStopIf::stop_other_cpus();
    // Firmware power-off can finish without ever reaching `karch::stop_cpu()`.
    karch::prepare_cpu_stop();
    crate::quiesce_nmi();
    platform_power_off()
}

/// Requests suspend-to-RAM through the platform sleep agent.
///
/// Unlike the terminals above this is not a system terminal: it performs no
/// SMP stop — the minimal suspend path has no resume-side CPU plug-in —
/// and returns an error when the platform cannot suspend, so callers may
/// apply their own policy (fall back, or refuse the request). Callers own
/// the pre-suspend cleanup (filesystem sync, device quiesce) exactly as
/// they do for the terminals.
pub fn suspend_to_ram() -> KResult<()> {
    platform_suspend_to_ram()
}
