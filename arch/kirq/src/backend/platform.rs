// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform IRQ manager interface and claimed-interrupt guard.
//!
//! `kirq` owns generic descriptor, dispatch, and completion ordering. Platform
//! irqchip backends implement this module's trait with concrete controller
//! behavior such as line configuration, mask/unmask, claim/ack, EOI or
//! deactivate, priority, and IPI delivery.

use alloc::sync::Arc;
use core::marker::PhantomData;

use kcpu_id_map::KCpuMask;

use crate::{
    IrqAffinity, IrqController, IrqDesc, IrqFlags, IrqHandler, IrqPolarity, IrqRef, IrqSource,
    IrqTrigger, Virq, state::DYNAMIC_VIRQ_BASE,
};

/// IRQ handler invoked on dispatch.
///
/// Each registration owns its `Arc<dyn IrqHandler>` — the Rust-native
/// counterpart of Linux's `dev_id` — with no side table or trampoline.
pub type Handler = Arc<dyn IrqHandler>;

/// Semantic inter-processor interrupt classes owned by the IRQ backend.
///
/// Callers select why an IPI is being sent, while the platform irqchip owns
/// the concrete vector or SGI assigned to that class. This mirrors Linux's
/// separation between an IPI message number and its virq/hwirq mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpiKind {
    /// Shared ordinary IPI used for normal cross-CPU work.
    Generic,
    /// Diagnostic request asking target CPUs to collect a backtrace snapshot.
    CpuBacktrace,
}

/// Target CPU(s) for inter-processor interrupts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetCpu {
    /// Target the current CPU.
    Self_,
    /// Target a specific CPU by ID.
    Specific(usize),
    /// Target all CPUs except the caller.
    AllButSelf {
        /// Logical CPU ID of the caller to exclude.
        me: usize,
        /// Number of logical CPUs considered by the broadcast.
        total: usize,
    },
    /// Target exactly the logical CPUs present in this mask.
    Mask(KCpuMask),
}

/// An IRQ claimed by the platform and not yet completed.
///
/// Completion must happen on the CPU that claimed the interrupt. The guard is
/// not `Send`, and its `Drop` path completes the interrupt if the generic IRQ
/// tail did not call [`DispatchedIrq::complete`] explicitly.
#[derive(Debug)]
pub struct DispatchedIrq {
    irq: Virq,
    completion_cookie: usize,
    completed: bool,
    _not_send: PhantomData<*mut ()>,
}

impl DispatchedIrq {
    /// Creates a dispatched IRQ with an opaque completion cookie.
    pub const fn new(irq: Virq, completion_cookie: usize) -> Self {
        Self {
            irq,
            completion_cookie,
            completed: false,
            _not_send: PhantomData,
        }
    }

    /// Returns the interrupt number claimed by the backend.
    ///
    /// Normal IRQ dispatch returns an OS-visible virq. NMI dispatch returns a
    /// raw hwirq because pseudo-NMI dispatch is intentionally independent from
    /// the normal `IRQ_STATE` translation table.
    pub const fn irq(&self) -> Virq {
        self.irq
    }

    /// Completes the claimed interrupt exactly once.
    ///
    /// Generic IRQ dispatch calls this after primary handler and wake
    /// compatibility fanout. Edge-style backends may have already EOIed in
    /// dispatch and use a no-op cookie, but level-triggered backends must delay
    /// their final EOI or deactivate until this point.
    pub fn complete(mut self) {
        self.complete_inner();
    }

    fn complete_inner(&mut self) {
        if self.completed {
            return;
        }
        self.completed = true;
        platform_complete_irq(self.completion_cookie);
    }
}

impl Drop for DispatchedIrq {
    fn drop(&mut self) {
        self.complete_inner();
    }
}

/// A pending interrupt claimed by the platform, before subscriber dispatch.
#[derive(Debug)]
pub struct PendingIrq {
    source: IrqRef,
    completion_cookie: usize,
    completed: bool,
    _not_send: PhantomData<*mut ()>,
}

impl PendingIrq {
    /// Creates a pending IRQ claim with an opaque completion cookie.
    pub const fn new(source: IrqRef, completion_cookie: usize) -> Self {
        Self {
            source,
            completion_cookie,
            completed: false,
            _not_send: PhantomData,
        }
    }

    /// The raw claim source.
    pub const fn source(&self) -> IrqRef {
        self.source
    }

    /// Lock-free resolution to the OS-visible logical IRQ.
    pub fn resolve(&self) -> Option<Virq> {
        match self.source {
            IrqRef::Virq(virq) => Some(virq),
            IrqRef::Domain(domain_id, hwirq) => crate::domain::resolve(domain_id, hwirq),
        }
    }

    /// Completes the claim; idempotent, also run by `Drop`.
    pub fn complete(mut self) {
        self.complete_inner();
    }

    fn complete_inner(&mut self) {
        if self.completed {
            return;
        }
        self.completed = true;
        platform_complete_irq(self.completion_cookie);
    }
}

impl Drop for PendingIrq {
    fn drop(&mut self) {
        self.complete_inner();
    }
}

#[kiface::interface]
pub trait IntrManagerIf {
    /// Configures a controller-local IRQ source from a normalized descriptor.
    ///
    /// `kirq` calls this before enabling a non-MSI line. Backends should consume
    /// trigger, polarity, controller, source, affinity, and flags metadata that
    /// they understand, and ignore metadata that is not relevant to that
    /// controller. MSI descriptors are programmed through `MsiBackendIf` and do
    /// not use the normal line configure path.
    fn configure(desc: IrqDesc);

    /// Enables or disables a controller-local IRQ source.
    ///
    /// `id` is the backend hwirq or local source id, not a dynamic virq.
    /// Enabling is normally called after [`IntrManagerIf::configure`].
    /// Disabling should mask or otherwise stop delivery of that local source.
    /// MSI enablement is represented by the device-visible MSI message and does
    /// not use this hook.
    fn enable(id: usize, on: bool);

    /// Claims, classifies, and dispatches an interrupt taken with IRQs enabled.
    ///
    /// After the controller claim and required ack, the backend must call
    /// exactly one generic entry:
    ///
    /// - [`crate::generic_handle_irq`] for a normal IRQ;
    /// - [`crate::generic_handle_nmi`] for an NMI-class claim.
    ///
    /// The return value is the resolved normal `virq`, when one exists, and is
    /// used only as metadata by the outer deferred-execution hook. `None` may
    /// therefore mean an NMI-class claim, an unresolved normal claim, or a
    /// spurious vector; dispatch and completion have already happened before
    /// this method returns.
    fn dispatch_irq(id: usize) -> Option<Virq>;

    /// Claims and dispatches an interrupt taken through the NMI entry path.
    ///
    /// The backend must call [`crate::generic_handle_nmi`] with a
    /// [`DispatchedIrq`] containing the raw hwirq and completion cookie. Unlike
    /// [`IntrManagerIf::dispatch_irq`], this path never opens the normal IRQ
    /// window. It is only called where normal IRQs are already masked.
    fn dispatch_nmi(id: usize);

    /// Completes a claimed interrupt with its opaque completion cookie.
    ///
    /// `completion_cookie` must be the cookie returned by the matching dispatch
    /// call and must be completed on the same CPU that claimed it. Backends may
    /// treat cookie `0` as a no-op for interrupt classes that were completed
    /// during dispatch, but level-triggered lines must use this hook for their
    /// final EOI or deactivate after generic handler fanout.
    fn complete_irq(completion_cookie: usize);

    /// Resolves a semantic IPI class to its platform-owned IRQ descriptor.
    ///
    /// `None` means this irqchip cannot provide the requested IPI class.
    fn ipi_desc(kind: IpiKind) -> Option<IrqDesc>;

    /// Sends a semantic IPI to one or more CPUs.
    ///
    /// Implementations must ensure that all Normal-memory writes performed by
    /// the caller before `send_ipi()` become visible to the target CPU before
    /// that CPU can observe and handle the delivered IPI. TLB shootdown relies
    /// on this publish-before-notify ordering to make the request state visible
    /// before the target acknowledges it.
    ///
    /// The [`IpiKind::CpuBacktrace`] path may be invoked from NMI context. Its
    /// implementation must therefore avoid allocation, logging, and locks.
    /// Returns `false` when the class or target set cannot be issued in full;
    /// partial delivery is possible. Success is not a target acknowledgement.
    /// [`IpiKind::CpuBacktrace`] requires [`TargetCpu::Mask`]; other target
    /// forms may be rejected even on a backend supporting that class.
    fn send_ipi(kind: IpiKind, target: TargetCpu) -> bool;

    /// Sets a controller-local interrupt priority.
    ///
    /// Backends that do not expose priority levels may implement this as a
    /// no-op. The `id` is controller-local and follows the same namespace as
    /// [`IntrManagerIf::enable`].
    fn set_prio(id: usize, prio: u8);
}

#[inline]
fn platform_configure(desc: IrqDesc) {
    IntrManagerIf::configure(desc)
}

#[inline]
fn platform_enable(id: usize, on: bool) {
    IntrManagerIf::enable(id, on)
}

#[inline]
fn needs_platform_binding(desc: IrqDesc) -> bool {
    if desc.flags.contains(IrqFlags::MSI) {
        return false;
    }
    desc.domain.is_some()
        || desc.hwirq < DYNAMIC_VIRQ_BASE
        || !matches!(desc.trigger, IrqTrigger::Unknown(_))
        || desc.polarity != IrqPolarity::Unknown
        || desc.source != IrqSource::Unknown
        || desc.controller != IrqController::Unknown
        || desc.affinity != IrqAffinity::Any
        || !desc.flags.is_empty()
}

#[inline]
pub(crate) fn configure_platform_irq(desc: IrqDesc) {
    if needs_platform_binding(desc) {
        platform_configure(desc);
    }
}

#[inline]
pub(crate) fn set_platform_irq_enabled(desc: IrqDesc, on: bool) {
    if needs_platform_binding(desc) {
        platform_enable(desc.hwirq, on);
    }
}

#[inline]
pub(crate) fn platform_dispatch_irq(id: usize) -> Option<Virq> {
    IntrManagerIf::dispatch_irq(id)
}

#[inline]
pub(crate) fn platform_dispatch_nmi(id: usize) {
    IntrManagerIf::dispatch_nmi(id)
}

#[inline]
fn platform_complete_irq(completion_cookie: usize) {
    IntrManagerIf::complete_irq(completion_cookie)
}

/// Resolves a semantic IPI class to the descriptor owned by the active irqchip.
///
/// Returns `None` when the active backend does not implement `kind`.
#[inline]
pub fn ipi_desc(kind: IpiKind) -> Option<IrqDesc> {
    IntrManagerIf::ipi_desc(kind)
}

/// Sends a semantic IPI to `target` through the active irqchip backend.
///
/// Returns `false` when the backend cannot deliver the requested class or
/// target set. [`IpiKind::CpuBacktrace`] is safe to request from NMI context;
/// callers must not assume the same for other classes. A successful call
/// only issues the notification; it does not wait for target acknowledgement.
/// On failure, some targets may already have been notified.
#[inline]
pub fn send_ipi(kind: IpiKind, target: TargetCpu) -> bool {
    IntrManagerIf::send_ipi(kind, target)
}

#[inline]
pub fn set_prio(id: usize, prio: u8) {
    IntrManagerIf::set_prio(id, prio)
}
