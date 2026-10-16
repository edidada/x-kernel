// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `input` class: its publication API, removal entries, and the
//! input node projection (eventN with the udev property set) —
//! instantiated on the shared machinery in `driver-base::class`.

use alloc::{string::String, vec::Vec};

use devnode::{InputNodeMetadata, NodeClass};
use driver_base::{
    ClassAvailabilityCallback, ClassDevice, ClassDeviceId, ClassParent, ClassRegistry,
    ClassRemovalCallback, ClassWithdraw,
    class::class_api::{ClassHooks, class_notify},
};
use kerrno::KResult;
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;

use super::{EventType, InputDevice};

pub type InputDeviceImpl = Box<dyn InputDevice + Send + Sync>;
use alloc::boxed::Box;

fn node_class(runtime: &InputDeviceImpl) -> Option<NodeClass> {
    let mut event_types = alloc::vec![0; (EventType::COUNT as usize).div_ceil(8)];
    let mut event_codes = alloc::collections::BTreeMap::new();
    for raw in 0..EventType::COUNT {
        let Some(event_type) = EventType::from_repr(raw) else {
            continue;
        };
        let mut codes = alloc::vec![0; event_type.bits_count().div_ceil(8)];
        if runtime
            .get_event_bits(event_type, &mut codes)
            .is_ok_and(|is_supported| is_supported)
        {
            event_types[raw as usize / 8] |= 1 << (raw as usize % 8);
            event_codes.insert(raw, codes);
        }
    }
    let id = runtime.device_id();
    Some(NodeClass::Input(InputNodeMetadata {
        name: String::from(runtime.name()),
        physical_location: String::from(runtime.physical_location()),
        unique_id: String::from(runtime.unique_id()),
        bus_type: id.bus_type,
        vendor: id.vendor,
        product: id.product,
        version: id.version,
        event_types,
        event_codes: event_codes.into_iter().collect(),
    }))
}

static HOOKS: ClassHooks<InputDeviceImpl> = ClassHooks {
    publish: |_| Ok(()),
    unpublish: |_| {},
    node_class,
};

driver_base::class_api!(
    "input",
    InputDeviceImpl,
    INPUT_DEVICES,
    input_class_registry,
    publish_input,
    input_devices,
    find_input_device,
    subscribe_input_available,
    subscribe_input_removed,
    HOOKS
);

/// Unpublish one `input` class device by publication id.
pub fn unpublish_input(id: ClassDeviceId) {
    driver_base::class::class_api::class_remove(input_class_registry, &HOOKS, id);
}

/// Guard form: withdraws this publication when the activation drops.
pub fn input_withdraw(device: &ClassDevice<InputDeviceImpl>) -> ClassWithdraw {
    ClassWithdraw::new(unpublish_input, device.id())
}
