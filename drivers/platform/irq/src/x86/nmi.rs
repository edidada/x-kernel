// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Native vector 2 claims source identities; none carries LAPIC EOI ownership.

use kplat::nm_irq::{NativeNmiSource, NativeNmiSourceIf};

/// Backend source namespace, deliberately outside the 8-bit CPU-vector range.
pub const PERIODIC_SOURCE: usize = 0x100;
/// Explicit notification request; never interpreted as a counter overflow.
pub const NOTIFY_SOURCE: usize = 0x101;

pub fn descriptor(source: NativeNmiSource) -> kirq::IrqDesc {
    kirq::IrqDesc::new(number(source), kirq::IrqTrigger::EdgeRising)
        .with_virq(number(source))
        .with_flags(kirq::IrqFlags::PER_CPU)
}

fn number(source: NativeNmiSource) -> usize {
    match source {
        NativeNmiSource::Periodic => PERIODIC_SOURCE,
        NativeNmiSource::Notification => NOTIFY_SOURCE,
    }
}

pub(super) fn source(desc: kirq::IrqDesc) -> Option<NativeNmiSource> {
    if desc.domain.is_some() || desc.controller != kirq::IrqController::Unknown {
        return None;
    }
    match desc.hwirq {
        PERIODIC_SOURCE => Some(NativeNmiSource::Periodic),
        NOTIFY_SOURCE => Some(NativeNmiSource::Notification),
        _ => None,
    }
}

pub(super) static CHIP: kirq::PerCpuNmiChip = kirq::PerCpuNmiChip {
    prepare: |desc| update(desc, NativeNmiSourceIf::prepare),
    teardown: |desc| update(desc, NativeNmiSourceIf::teardown),
};

fn update(
    desc: kirq::IrqDesc,
    update: fn(NativeNmiSource) -> bool,
) -> Result<(), kirq::IrqDescError> {
    let source = source(desc).ok_or(kirq::IrqDescError::UnsupportedFlow { hwirq: desc.hwirq })?;
    if update(source) {
        Ok(())
    } else {
        Err(kirq::IrqDescError::ConfigurationFailed { hwirq: desc.hwirq })
    }
}

/// # Safety
/// Called in KIRQ's effective NMI/RCU scope for a captured native vector 2.
pub(super) unsafe fn dispatch() {
    // One vector may cover both causes. A software request never fabricates PMI
    // status, and an unrelated external NMI acquires neither source action.
    if NativeNmiSourceIf::claim_notification() {
        // SAFETY: this identity is mapped at source startup; a software NMI IPI
        // has no LAPIC ISR/EOI obligation, encoded by the backend's zero cookie.
        let claim = unsafe { kirq::PendingIrq::new(kirq::IrqRef::Virq(NOTIFY_SOURCE), 0) };
        kirq::generic_handle_nmi(claim);
    }
    if NativeNmiSourceIf::claim_periodic() {
        // SAFETY: the backend uniquely claimed its stopped owned counter. Cookie
        // zero avoids EOI; producer reload belongs to the explicit completion.
        let claim = unsafe { kirq::PendingIrq::new(kirq::IrqRef::Virq(PERIODIC_SOURCE), 0) };
        kirq::generic_handle_nmi(claim);
        NativeNmiSourceIf::complete_periodic();
    }
}
