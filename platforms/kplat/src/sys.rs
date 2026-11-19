// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform system control interface.

#[cfg(feature = "smp")]
use kcpu_id_map::LogicalCpuId;
use kerrno::KResult;
use kplat_macros::device_interface;

#[device_interface]
pub trait CpuStartIf {
    #[cfg(feature = "smp")]
    /// Boots an application processor selected by logical CPU ID.
    ///
    /// Platform implementations must translate the logical CPU ID to any
    /// firmware- or hardware-specific raw CPU identifier before issuing the
    /// actual boot command.
    fn boot_ap(logical_cpu_id: LogicalCpuId, stack_top: usize) -> KResult;
}

#[device_interface]
pub trait PlatformPowerIf {
    /// Halts the calling CPU without cutting system power.
    ///
    /// This is a bare platform terminal: it stops execution of the calling
    /// CPU (masking local interrupts first) and must never invoke the
    /// platform power-off agent. Higher-level cleanup such as filesystem
    /// sync, process teardown, or device removal is owned by callers and
    /// must have completed before this is reached.
    fn halt() -> !;

    /// Powers off the system through the platform power-off agent.
    ///
    /// This is a bare platform terminal (PSCI `SYSTEM_OFF`, SBI SRST
    /// shutdown, ACPI PM power-off port, and so on, depending on the
    /// platform). Higher-level cleanup such as filesystem sync, process
    /// teardown, or device removal is owned by callers and must have
    /// completed before this is reached.
    fn power_off() -> !;

    /// Suspends the system to RAM through the platform sleep agent (ACPI
    /// S3 on x86-64).
    ///
    /// Unlike `halt` and `power_off` this is not a terminal: it returns an
    /// error when the platform cannot suspend, and a platform that slept
    /// and resumed inline may return successfully. Higher-level cleanup —
    /// filesystem sync, device quiesce, stopping other CPUs — is owned by
    /// callers and must have completed before this is reached.
    fn suspend_to_ram() -> KResult;
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
