// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Software-only integration entry through the real IRQ dispatch and drain path.

/// Dispatches a caller-owned software descriptor in pinned hardirq context.
///
/// The caller keeps the descriptor identity stable through this call and joins
/// all dispatchers before releasing its fixture. Use a metadata-free high virq
/// with Legacy flow; no device may deliver this source. Handlers must not sleep.
/// Completion is a local closure, not an invented physical claim or EOI cookie.
/// This does not exercise controller routing or the architecture IRQ-exit path.
pub fn dispatch(virq: crate::Virq) -> bool {
    let desc = crate::descriptor(virq).expect("synthetic IRQ must be registered");
    assert!(virq >= super::DYNAMIC_VIRQ_BASE);
    assert_eq!(desc, crate::IrqDesc::from_virq(virq));
    let _context = crate::context::test_support::ScopedHardIrqContext::enter();
    let mut completed = false;
    let resolved = crate::dispatch::dispatch_claim(crate::IrqRef::Virq(virq), || {
        completed = true;
    });
    resolved == Some(virq) && completed
}
