// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `input` class: one const-constructed static plus the admission-gated
//! consumer data plane (the Linux `struct class input` analogue).

use alloc::{boxed::Box, string::String};

use devnode::{InputNodeMetadata, NodeClass};
use driver_base::{
    ClassDevice,
    class::class_api::{Class, ClassHooks},
};
use kerrno::KResult;

use super::{Event, EventType, InputDevice, InputDeviceId};

pub type InputDeviceImpl = Box<dyn InputDevice + Send + Sync>;

pub static INPUT: Class<InputDeviceImpl, InputHooks> = Class::new(InputHooks);

pub struct InputHooks;

impl ClassHooks<InputDeviceImpl> for InputHooks {
    fn node_class(&self, runtime: &InputDeviceImpl) -> Option<NodeClass> {
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
}

impl InputDevice for ClassDevice<Box<dyn InputDevice + Send + Sync>> {
    fn name(&self) -> &str {
        self.name()
    }

    fn device_id(&self) -> InputDeviceId {
        self.with(|device| device.device_id())
    }

    fn physical_location(&self) -> &str {
        self.with(|device| device.physical_location())
    }

    fn unique_id(&self) -> &str {
        self.with(|device| device.unique_id())
    }

    fn get_event_bits(&self, ty: EventType, out: &mut [u8]) -> KResult<bool> {
        self.with_available(|device| device.get_event_bits(ty, out))
    }

    fn read_event(&self) -> KResult<Event> {
        self.with_available(|device| device.read_event())
    }
}
