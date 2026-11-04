// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{assert_eq, def_test};

use crate::{CoherentMem, DevIrq, Virq};

#[derive(Default)]
struct IrqGuardState {
    releases: AtomicUsize,
    provider_drops: AtomicUsize,
}

struct GuardProvider {
    state: Arc<IrqGuardState>,
    fail_request: bool,
}

fn guard_resource() -> crate::IrqResource {
    crate::IrqResource::mapped(
        Virq::new(903),
        crate::IrqTrigger::LevelHigh,
        crate::IrqController::Gic,
        Some(crate::IrqDomainId::new(0x7319)),
        37,
    )
}

impl crate::IrqOp for GuardProvider {
    fn request_irq(
        &self,
        resource: crate::IrqResource,
        handler: Arc<dyn crate::IrqHandler>,
    ) -> crate::ResResult<crate::IrqHandlerToken> {
        core::assert_eq!(resource, guard_resource());
        if self.fail_request {
            return Err(crate::ResError::Busy);
        }
        core::assert!(handler.handle(resource.virq().raw()).handled());
        Ok(crate::IrqHandlerToken::SharedAction(79))
    }

    fn release_irq(&self, resource: crate::IrqResource, token: crate::IrqHandlerToken) {
        core::assert_eq!(resource, guard_resource());
        core::assert_eq!(token, crate::IrqHandlerToken::SharedAction(79));
        core::assert_eq!(self.state.provider_drops.load(Ordering::Relaxed), 0);
        self.state.releases.fetch_add(1, Ordering::Relaxed);
    }

    fn set_irq_enabled(&self, _: crate::IrqResource, _: bool) {}
}

impl Drop for GuardProvider {
    fn drop(&mut self) {
        self.state.provider_drops.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test]
fn irq_guard_retains_provider_and_exact_registration_until_release() {
    let state = Arc::new(IrqGuardState::default());
    let provider: Arc<dyn crate::IrqOp> = Arc::new(GuardProvider {
        state: state.clone(),
        fail_request: false,
    });
    let guard = DevIrq::request_with_shared_provider(
        provider.clone(),
        guard_resource(),
        Arc::new(|virq| {
            core::assert_eq!(virq, 903);
            crate::IrqEvent::HANDLED
        }),
    )
    .unwrap();
    assert_eq!(Arc::strong_count(&provider), 2);
    drop(provider);
    assert_eq!(guard.virq(), Virq::new(903));
    assert_eq!(state.releases.load(Ordering::Relaxed), 0);
    assert_eq!(state.provider_drops.load(Ordering::Relaxed), 0);
    drop(guard);
    assert_eq!(state.releases.load(Ordering::Relaxed), 1);
    assert_eq!(state.provider_drops.load(Ordering::Relaxed), 1);
}

#[def_test]
fn rejected_irq_request_has_no_release_obligation_or_retained_callback() {
    let state = Arc::new(IrqGuardState::default());
    let provider: Arc<dyn crate::IrqOp> = Arc::new(GuardProvider {
        state: state.clone(),
        fail_request: true,
    });
    let handler: Arc<dyn crate::IrqHandler> = Arc::new(|_| crate::IrqEvent::HANDLED);
    let result = DevIrq::request_with_shared_provider(provider, guard_resource(), handler.clone());
    assert_eq!(result.unwrap_err(), crate::ResError::Busy);
    assert_eq!(Arc::strong_count(&handler), 1);
    assert_eq!(state.releases.load(Ordering::Relaxed), 0);
    assert_eq!(state.provider_drops.load(Ordering::Relaxed), 1);
}

#[def_test]
fn irq_and_dma_guards_release_only_their_acquisition() {
    let releases = Arc::new(AtomicUsize::new(0));
    let irq_release = releases.clone();
    let dma_release = releases.clone();
    let irq = DevIrq::new(
        Virq::new(41),
        Some(Arc::new(move || {
            irq_release.fetch_add(1, Ordering::Relaxed);
        })),
    );
    let dma = CoherentMem::new(
        0,
        0x4000,
        64,
        Some(Arc::new(move || {
            dma_release.fetch_add(10, Ordering::Relaxed);
        })),
    );
    assert_eq!(irq.virq(), Virq::new(41));
    assert_eq!(dma.bus_addr(), 0x4000);
    assert_eq!(dma.len(), 64);
    assert_eq!(releases.load(Ordering::Relaxed), 0);
    drop(irq);
    assert_eq!(releases.load(Ordering::Relaxed), 1);
    drop(dma);
    assert_eq!(releases.load(Ordering::Relaxed), 11);
}
