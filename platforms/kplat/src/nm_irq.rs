// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform NMI (or pseudo-NMI) interface.
//!
//! # Architecture
//!
//! This module defines the **NMI mode** — whether the platform supports
//! hardware NMI, falls back to pseudo-NMI, or has no NMI at all.  It is
//! deliberately **not** tied to any specific NMI source.  Source‑specific
//! configuration (counter thresholds, event numbers, handler registration)
//! belongs to the source's own subsystem, which implements [`PeriodicNmiSourceIf`]
//! behind a source‑neutral interface.
//!
//! ```text
//!   Consumer (watchdog)
//!     → khal::nmi::enable_periodic_nmi(period_ns, cb)
//!       → PeriodicNmiSourceIf provider (e.g. PMU cycle counter backend)
//!         → source-owned local delivery preparation
//!           → AArch64 PMU: kirq::prepare_percpu_nmi(desc)
//!             → GIC NMI attribute / pseudo-NMI priority transaction
//!           → native-NMI sources: their controller-specific routing
//!   pmu feature init (kruntime)
//!     → khal::pmu::register_overflow_dispatch(hwirq)
//!       → pmu-only build: kirq::try_register_startup(...)
//!       → PMU as NMI source (nmi-pmu): kirq::register_nmi_startup(...)
//!         → Line-owned RCU action (shared by true NMI and IRQ fallback)
//! ```

use kplat_macros::device_interface;

/// NMI callback type, invoked in NMI context on the current CPU.
pub type NmiCb = fn();

/// Runtime NMI capability reported by the platform.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NmiMode {
    /// Native hardware NMI, unaffected by ordinary IRQ masking.
    Hardware,
    /// Pseudo-NMI implemented via high-priority IRQ + PMR masking.
    Pseudo,
    /// No NMI support.
    None,
}

/// Static descriptor for the NMI implementation.
#[derive(Clone, Copy, Debug)]
pub struct NmiSourceInfo {
    /// Human-readable name for diagnostics.
    pub name: &'static str,
}

#[device_interface]
pub trait NmiMechanismIf {
    /// System‑wide (once‑per‑boot) NMI initialization.
    ///
    /// Called during `early_driver_init`, after the interrupt controller
    /// distributor has been mapped and initialised. Typically queries
    /// controller‑wide NMI capability (for example, `GICD_TYPER.NMI` on GICv3.3);
    /// CPU-wide enables belong to `late_init`. Source routing and attributes
    /// belong to the source backend's local preparation transaction.
    fn early_init() -> bool;

    /// Per‑CPU NMI initialization.
    ///
    /// Called during `prepare_interrupts` / `prepare_local_interrupts`, after the controller
    /// CPU interface is ready.  Typically configures per‑redistributor
    /// registers and per‑CPU architecture‑level NMI enables (e.g.
    /// `SCTLR_EL1.NMI`, `PSTATE.ALLINT`, `GICR_INMIR`).
    ///
    /// Source‑specific timer / counter setup (e.g. PMU cycle counter
    /// threshold) is **not** done here — it belongs to the source's own
    /// subsystem.
    fn late_init() -> bool;

    /// Current operating NMI mode.
    ///
    /// Source backends query this before preparing delivery. Native NMI does
    /// not require promotion of an ordinary IRQ or a hardware IRQ identifier.
    fn mode() -> NmiMode;

    /// Returns the static descriptor for this NMI implementation.
    fn info() -> NmiSourceInfo;
}

/// Source‑neutral periodic NMI provider.
///
/// Consumers such as the hardlockup watchdog ask for a periodic NMI through
/// this interface without knowing which hardware source (PMU cycle counter,
/// future sources) generates the events.  The platform implements this trait
/// in the source's own subsystem. Enabling the project's NMI facility still
/// requires exactly one periodic provider; that provider need not use a PMU.
///
/// kiface allows exactly one provider per interface.  With a single source
/// the platform implements this trait directly in the source backend; when a
/// second source is added, the platform NMI module should become the sole
/// provider and dispatch to per‑source backends.
#[device_interface]
pub trait PeriodicNmiSourceIf {
    /// Registers the selected source's dispatch actions once during allocated
    /// boot, before activation. The source owns routing and handler identity.
    fn register_source() -> bool;

    /// Arm a periodic NMI with period `period_ns`.
    ///
    /// Must be called on **each CPU** so every CPU arms its own source.
    /// The provider keeps setup, rollback and producer start on the calling
    /// CPU. This local exclusion does not serialize a remote source shutdown;
    /// the source owner must exclude concurrent shutdown and restart.
    /// The backend owns its source identity, callback publication and delivery
    /// preparation. It must make dispatch state ready before enabling the local
    /// producer, without requiring promotion of an ordinary IRQ. For example,
    /// AArch64 PMU startup registers its Line handler through `PmuInterruptIf`,
    /// and this transaction prepares the disabled PPI before starting a counter.
    /// Native NMI sources may configure a different controller route instead.
    /// `handler` runs in NMI context on each sample. Returns false when the
    /// mechanism/source is unavailable, ownership conflicts or local setup fails.
    fn enable_periodic_nmi(period_ns: u64, handler: NmiCb) -> bool;

    /// Quiesce the calling CPU's periodic NMI source for a terminal stop.
    ///
    /// This is intentionally a one-way lifecycle operation: it stops a
    /// source armed by [`PeriodicNmiSourceIf::enable_periodic_nmi`] without
    /// unregistering its callback or undoing its IRQ/NMI-line configuration.
    /// It is a no-op when the local source was never armed. Callers must be
    /// pinned to the CPU whose source they are stopping and must not attempt
    /// to re-arm that source afterwards.
    fn quiesce_periodic_nmi();

    /// Stops and releases the calling CPU's periodic source for later reuse.
    ///
    /// The owner must serialize this operation against source start and other
    /// users of the same interrupt line. The provider pins the whole local
    /// transaction; task or ordinary IPI context is allowed, NMI is not.
    /// True confirms producer stop and local controller teardown before local
    /// callback storage is cleared. False retains storage for retry and does
    /// not authorize handler removal or source reuse. Unsupported mechanisms
    /// and shared source ownership are rejected before stopping production.
    /// A prepared local line still needs teardown when no periodic counter was
    /// armed; absence of a producer alone is not shutdown completion.
    ///
    /// This neither broadcasts nor removes the Line's shared handler. The owner
    /// must collect successful execution on every participant before removing
    /// that handler. A later restart must prepare and enable local delivery again.
    fn shutdown_periodic_nmi() -> bool;

    /// Requests prompt samples on other armed CPUs, best effort from NMI.
    /// Publication must precede hardware notification. False means unavailable
    /// or incomplete notification; consumers must retain bounded fallback waits.
    fn request_peer_samples() -> bool;
}

/// Native shared-vector source identities, independent of CPU vector numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeNmiSource {
    /// Locally reserved periodic hardware counter.
    Periodic,
    /// Explicit software request delivered by an NMI IPI.
    Notification,
}

/// Platform-owned native-NMI producer/controller integration.
/// Generic registration, action publication and dispatch remain in KIRQ.
/// Claim/complete run pinned in NMI; control runs pinned outside NMI, with
/// ordinary IRQs masked. None may allocate, sleep or take ordinary IRQ locks.
#[device_interface]
pub trait NativeNmiSourceIf {
    /// Claims only an owned periodic overflow and stops it before dispatch.
    fn claim_periodic() -> bool;
    /// Claims a published IPI notification request, independent of PMU state.
    fn claim_notification() -> bool;
    /// Prepares a disabled source before KIRQ publishes its local action.
    fn prepare(source: NativeNmiSource) -> bool;
    /// Removes disabled source routing after the owner has stopped production.
    fn teardown(source: NativeNmiSource) -> bool;
    /// Changes local delivery admission; this does not start a counter.
    fn set_enabled(source: NativeNmiSource, enabled: bool);
    /// Reloads and restarts a claimed counter only if its owner remains armed.
    fn complete_periodic();
    /// Stops every local producer, then retires the native KIRQ source Lines.
    /// Requires sleepable task context and retryable all-CPU synchronous work.
    fn shutdown_sources() -> kerrno::KResult<()>;
    /// Republishes all native source Lines after a completed global shutdown.
    fn restore_sources() -> kerrno::KResult<()>;
}
