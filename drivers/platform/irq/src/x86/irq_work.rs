// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Permanent ordinary-IRQ notification for NMI-safe local reclamation.

extern crate alloc;
use alloc::sync::Arc;

static REGISTERED: lazyinit::LazyInit<bool> = lazyinit::LazyInit::new();

pub(super) fn prepare() -> Option<fn()> {
    let vector = usize::from(x86_apic::APIC_IRQ_WORK_VECTOR);
    if vector == kbuild_config::IPI_IRQ {
        return None;
    }
    let desc = kirq::IrqDesc::new(vector, kirq::IrqTrigger::EdgeRising)
        .with_virq(vector)
        .with_flags(kirq::IrqFlags::PER_CPU);
    REGISTERED.call_once(|| {
        if let Err(error) = kirq::try_map(desc) {
            log::warn!("x86 irq_work mapping failed: {error:?}");
            return false;
        }
        match kirq::try_register_percpu_startup(
            desc,
            Arc::new(|_| {
                kirq::irq_work::handle_irq();
                kirq::IrqEvent::HANDLED
            }),
        ) {
            Ok(true) => true,
            result => {
                log::warn!("x86 irq_work registration failed: {result:?}");
                false
            }
        }
    });
    if REGISTERED.get() != Some(&true) {
        return None;
    }
    if let Err(error) = kirq::try_enable_irq(desc) {
        log::warn!("x86 irq_work local enable failed: {error:?}");
        return None;
    }
    Some(x86_apic::raise_local_irq_work)
}
