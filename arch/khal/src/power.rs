// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU power management.
//!
//! [`halt`] and [`power_off`] are the system power terminals for normal
//! execution contexts. Before entering the bare platform terminal they request a best-effort
//! stop of other CPUs through [`SmpStopIf`]. A bounded timeout does not prove
//! that remote execution has stopped; callers must not use return as a
//! resource-reclamation barrier. When the stop provider is unavailable (UP builds) or
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
//! exchanges IPIs, which can deadlock when the panicking CPU holds a lock
//! that the other CPUs spin on with interrupts disabled.

use kerrno::KResult;
#[cfg(feature = "smp")]
pub use kplat::sys::boot_ap;
pub use kplat::sys::{
    halt as platform_halt, power_off as platform_power_off,
    suspend_to_ram as platform_suspend_to_ram,
};

/// `kiface` for stopping all other CPUs on the terminal path.
///
/// The provider requests other present CPUs to stop with a bounded wait and
/// must be callable with local interrupts disabled. Return does not certify
/// remote acknowledgment after timeout.
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

/// Requests other CPUs to stop, then halts the system with power kept.
///
/// See the module documentation for the terminal contract; this never
/// returns.
pub fn halt() -> ! {
    SmpStopIf::stop_other_cpus();
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
    SmpStopIf::stop_other_cpus();
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

/// Reset-aware platform sleep operations for the system PM coordinator.
/// These operations do not freeze tasks, drain devices or stop DMA themselves.
#[cfg(target_arch = "x86_64")]
#[kiface::interface]
pub trait SystemSleepIf {
    /// Reads the platform clock that continues across sleep, independently of
    /// the guest monotonic mapping. Read failure leaves accounting unsettled.
    fn persistent_time() -> Option<ktime_types::SystemTime>;
    /// Claims wake resources on the boot CPU before freezing ordinary execution.
    /// # Safety
    /// Caller must serialize PM and exclude CPU hotplug until finish.
    unsafe fn prepare(sleep_types: [u8; 2]) -> KResult;
    /// Saves a remote CPU and calls its IRQ-off acknowledgment/wait callback.
    /// # Safety
    /// The callback and argument must remain valid through resumed return;
    /// local IRQs and migration must be disabled and devices remain usable.
    unsafe fn park_cpu(wait: extern "C" fn(*mut ()), argument: *mut ());
    /// Enters platform sleep on the boot CPU, returning on its saved stack.
    /// # Safety
    /// Other CPUs, devices, NMI sources, IRQs and clocks must be quiesced.
    unsafe fn enter() -> KResult;
    /// Restarts saved remote contexts, retaining their software wait barriers.
    /// # Safety
    /// Boot CPU IRQs remain disabled and all shared resources stay excluded.
    unsafe fn restore_cpus() -> KResult;
    /// Releases wake resources after all CPUs have left their resume trampoline.
    /// # Safety
    /// Caller must have joined remote CPU callbacks before releasing ownership.
    unsafe fn finish();
}
