// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use unittest::{assert_eq, def_test};

use crate::{
    IrqAction, IrqHandlerFn, IrqHandlerToken, IrqOp, IrqResource, IrqRouteDesc, ResResult, Virq,
};

struct MockIrqProvider;

impl IrqOp for MockIrqProvider {
    fn request_irq(&self, _irq: IrqResource, handler: IrqHandlerFn) -> ResResult<IrqHandlerToken> {
        match handler() {
            IrqAction::Handled => Ok(IrqHandlerToken::shared_action(7)),
            IrqAction::NotHandled => Ok(IrqHandlerToken::shared_action(8)),
        }
    }

    fn release_irq(&self, _irq: IrqResource, _token: IrqHandlerToken) {}

    fn set_irq_enabled(&self, _irq: IrqResource, _enabled: bool) {}
}

#[def_test]
fn irq_mapping_retains_shared_vocabulary_and_origin() {
    let trigger: crate::IrqTrigger = irq_types::IrqTrigger::Unknown(0x80);
    let controller: crate::IrqController = irq_types::IrqController::Gic;
    let domain: crate::IrqDomainId = irq_types::IrqDomainId::new(73);
    let route = IrqRouteDesc {
        hwirq: 9,
        trigger,
        controller,
        domain: Some(domain),
    };
    let mapped = MockIrqProvider.map_irq(route);
    assert_eq!(
        mapped,
        Ok(IrqResource::DomainMapped {
            virq: Virq::new(9),
            trigger,
            controller,
            domain: Some(domain),
            hwirq: 9,
        })
    );
    assert_eq!(domain.as_u32(), 73);
}

#[def_test]
fn irq_provider_accepts_model_owned_handler_results() {
    let provider = MockIrqProvider;
    let irq = IrqResource::plain(Virq::new(41), crate::IrqTrigger::LevelHigh);
    for (action, token) in [(IrqAction::Handled, 7), (IrqAction::NotHandled, 8)] {
        let handler: IrqHandlerFn = Arc::new(move || action);
        assert_eq!(
            provider.request_irq(irq, handler),
            Ok(IrqHandlerToken::shared_action(token))
        );
    }
}

#[def_test]
fn mapped_resource_keeps_hardware_and_virtual_irq_distinct() {
    let irq = IrqResource::mapped(
        Virq::new(41),
        crate::IrqTrigger::LevelHigh,
        crate::IrqController::Gic,
        Some(crate::IrqDomainId::new(73)),
        9,
    );
    assert_eq!(irq.virq(), Virq::new(41));
    assert_eq!(
        irq,
        IrqResource::DomainMapped {
            virq: Virq::new(41),
            trigger: crate::IrqTrigger::LevelHigh,
            controller: crate::IrqController::Gic,
            domain: Some(crate::IrqDomainId::new(73)),
            hwirq: 9,
        }
    );
}
