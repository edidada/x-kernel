// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `input` class: one const-constructed static plus the admission-gated
//! consumer data plane (the Linux `struct class input` analogue).

use alloc::{boxed::Box, format, string::String, vec, vec::Vec};

use devnode::{DevNodeType, PublishError, bitmap_value, publish_auto_indexed, withdraw};
use driver_base::{
    Publication,
    class::class_api::{Class, ClassHooks, PublicationNode},
};
use kerrno::KResult;

use super::{Event, EventType, InputDevice, InputDeviceId};

pub type InputDeviceImpl = Box<dyn InputDevice + Send + Sync>;

pub static INPUT: Class<InputDeviceImpl, InputHooks> = Class::new(InputHooks);

pub struct InputHooks;

/// The input node type (`eventN`): the class owns how it projects.
static INPUT_NODE: DevNodeType = DevNodeType::new(
    "input",
    13,
    64,
    32,
    |index| format!("input/event{index}"),
    // physical_path here already ends with the `input` segment; the
    // canonical layout nests inputN under it and eventN under inputN.
    |physical, index| format!("{physical}/input{index}/event{index}"),
);

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn property_name(event_type: u8) -> Option<&'static str> {
    Some(match event_type {
        1 => "KEY",
        2 => "REL",
        3 => "ABS",
        4 => "MSC",
        5 => "SW",
        0x11 => "LED",
        0x12 => "SND",
        0x15 => "FF",
        _ => return None,
    })
}

impl ClassHooks<InputDeviceImpl> for InputHooks {
    fn publish(&self, runtime: &InputDeviceImpl, node: &PublicationNode) -> KResult<()> {
        let mut event_types = vec![0; (EventType::COUNT as usize).div_ceil(8)];
        let mut event_codes = alloc::collections::BTreeMap::new();
        for raw in 0..EventType::COUNT {
            let Some(event_type) = EventType::from_repr(raw) else {
                continue;
            };
            let mut codes = vec![0; event_type.bits_count().div_ceil(8)];
            if runtime
                .get_event_bits(event_type, &mut codes)
                .is_ok_and(|is_supported| is_supported)
            {
                event_types[raw as usize / 8] |= 1 << (raw as usize % 8);
                event_codes.insert(raw, codes);
            }
        }

        let id = runtime.device_id();
        let mut properties = Vec::new();
        properties.push((
            String::from("PRODUCT"),
            format!(
                "{:x}/{:x}/{:x}/{:x}",
                id.bus_type, id.vendor, id.product, id.version
            ),
        ));
        properties.push((String::from("NAME"), sanitize(runtime.name())));
        properties.push((String::from("PHYS"), sanitize(runtime.physical_location())));
        if !runtime.unique_id().is_empty() {
            properties.push((String::from("UNIQ"), sanitize(runtime.unique_id())));
        }
        properties.push((String::from("PROP"), String::from("0")));
        properties.push((String::from("EV"), bitmap_value(&event_types)));
        for (event_type, bits) in &event_codes {
            if let Some(key) = property_name(*event_type) {
                properties.push((String::from(key), bitmap_value(bits)));
            }
        }

        match publish_auto_indexed(
            node.id.raw(),
            &INPUT_NODE,
            &format!("{}/input", node.parent_path),
            None,
            properties,
            Some(node.driver_name.clone()),
        ) {
            Ok(_) => {}
            Err(PublishError::ResourceBusy) => {
                // Index space exhausted: NO fallback node. Any synthesized
                // dev_t either aliases a live device (devfs cdev_add would
                // overwrite it; removing either device tears down both) or
                // fabricates a node userspace cannot distinguish. The class
                // device itself keeps working; only the /dev and uevent
                // projections are absent, loudly.
                log::error!(
                    "input: event index space exhausted ({}); device '{}' gets no /dev node and \
                     no uevent",
                    INPUT_NODE.index_count,
                    runtime.name()
                );
            }
            Err(PublishError::AlreadyExists) => {
                log::error!(
                    "input: devnode id {} already published; skipping",
                    node.id.raw()
                );
            }
        }
        Ok(())
    }

    fn unpublish(&self, _runtime: &InputDeviceImpl, node: &PublicationNode) {
        withdraw(node.id.raw());
    }
}

impl InputDevice for Publication<Box<dyn InputDevice + Send + Sync>> {
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
