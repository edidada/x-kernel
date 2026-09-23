// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host integration for Linux-compatible device-node uevents.
//!
//! The device model owns lifecycle ordering. This crate translates its ordered
//! notifications into kobject netlink messages and shares the same formatter
//! with sysfs `uevent` attributes.

#![no_std]

extern crate alloc;

use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use kdevice::{DeviceNode, DeviceNodeLifecycleEvent, install_device_node_lifecycle_publisher};
use klazy::Once;

const UEVENT_GROUP: u32 = 1;

static INITIALIZATION: Once<()> = Once::new();

/// Device object whose properties and path are represented by one event.
/// Valid Linux kobject event action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UeventAction {
    /// Object creation.
    Add,
    /// Object removal.
    Remove,
    /// Object state or attribute change.
    Change,
    /// Object path change.
    Move,
    /// Object became available.
    Online,
    /// Object became unavailable.
    Offline,
    /// Driver binding.
    Bind,
    /// Driver unbinding.
    Unbind,
}

impl UeventAction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Remove => "remove",
            Self::Change => "change",
            Self::Move => "move",
            Self::Online => "online",
            Self::Offline => "offline",
            Self::Bind => "bind",
            Self::Unbind => "unbind",
        }
    }
}

impl TryFrom<&str> for UeventAction {
    type Error = ();

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "add" => Ok(Self::Add),
            "remove" => Ok(Self::Remove),
            "change" => Ok(Self::Change),
            "move" => Ok(Self::Move),
            "online" => Ok(Self::Online),
            "offline" => Ok(Self::Offline),
            "bind" => Ok(Self::Bind),
            "unbind" => Ok(Self::Unbind),
            _ => Err(()),
        }
    }
}

/// Initializes automatic device-node lifecycle publication.
///
/// This must run before device discovery starts. The device model rejects a
/// late installation so boot cannot silently omit earlier add events.
pub fn init() {
    INITIALIZATION.call_once(|| {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Boot installs the uevent publisher \
                      exactly once before discovery, as required by the device lifecycle contract"
        )]
        install_device_node_lifecycle_publisher(Arc::new(handle_lifecycle_event))
            .expect("device-node uevent publisher must initialize before device discovery");
    });
}

fn handle_lifecycle_event(event: DeviceNodeLifecycleEvent) {
    match event {
        DeviceNodeLifecycleEvent::Added(node) => publish_add(&node),
        DeviceNodeLifecycleEvent::Removed(node) => publish_remove(&node),
        DeviceNodeLifecycleEvent::Removing(_) => {}
    }
}

/// Returns the canonical sysfs physical path for a device node.
pub fn physical_device_path(node: &DeviceNode) -> String {
    String::from(node.physical_path())
}

fn uevent_path(node: &DeviceNode) -> String {
    format!("devices/{}/{}", node.physical_path(), node.devname())
}

/// Returns the Linux subsystem name for a projected device node.
pub fn subsystem_name(node: &DeviceNode) -> &'static str {
    node.subsystem()
}

fn sanitized_property(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

/// Formats a little-endian capability bitmap using Linux sysfs word order.
///
/// Bytes are grouped into native 64-bit words, rendered with the highest
/// non-zero word first, and separated by spaces. An empty or all-zero bitmap
/// is represented as `0`.
pub fn bitmap_value(bytes: &[u8]) -> String {
    let words: Vec<u64> = bytes
        .chunks(8)
        .map(|chunk| {
            let mut raw = [0u8; 8];
            raw[..chunk.len()].copy_from_slice(chunk);
            u64::from_le_bytes(raw)
        })
        .collect();
    if words.is_empty() {
        return "0".to_string();
    }
    let last = words.iter().rposition(|word| *word != 0).unwrap_or(0);
    words[..=last]
        .iter()
        .rev()
        .map(|word| format!("{word:x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Returns the properties represented by a sysfs `uevent` file or message.
///
/// Subsystem-generic: MAJOR/MINOR/DEVNAME plus the node's class-carried
/// properties and `MODALIAS` when the device model derived one from its
/// id-table match.
pub fn uevent_properties(node: &DeviceNode) -> Vec<(String, String)> {
    let number = node.device_number();
    let mut properties = alloc::vec![
        ("MAJOR".to_string(), number.major().to_string()),
        ("MINOR".to_string(), number.minor().to_string()),
        ("DEVNAME".to_string(), node.devname().to_string()),
    ];
    if let Some(modalias) = node.modalias() {
        properties.push(("MODALIAS".to_string(), modalias.to_string()));
    }
    properties.extend(node.properties().to_vec());
    properties
}

/// Renders the newline-delimited contents of a sysfs `uevent` attribute.
pub fn uevent_file_contents(node: &DeviceNode) -> Vec<u8> {
    let mut contents = String::new();
    for (key, value) in uevent_properties(node) {
        contents.push_str(&key);
        contents.push('=');
        contents.push_str(&value);
        contents.push('\n');
    }
    contents.into_bytes()
}

/// Publishes one action for a device object.
///
/// Lifecycle add/remove actions are generated internally by [`init`]. An
/// input-parent scope for a non-input device is ignored.
pub fn publish(action: UeventAction, node: &DeviceNode) {
    let action = action.as_str();
    let devpath = format!("/{}", uevent_path(node));
    let mut payload = format!("{action}@{devpath}\0ACTION={action}\0DEVPATH={devpath}\0");
    payload.push_str("SUBSYSTEM=");
    payload.push_str(subsystem_name(node));
    payload.push('\0');
    for (key, value) in uevent_properties(node) {
        payload.push_str(&key);
        payload.push('=');
        payload.push_str(&value);
        payload.push('\0');
    }
    knet::netlink::publish_kobject_uevent(UEVENT_GROUP, payload.as_bytes());
}

fn publish_add(node: &DeviceNode) {
    publish(UeventAction::Add, node);
}

fn publish_remove(node: &DeviceNode) {
    publish(UeventAction::Remove, node);
}

#[unittest::mod_test]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn property_values_cannot_inject_uevent_fields() {
        assert_eq!(
            sanitized_property("keyboard\0ACTION=remove\n"),
            "keyboard ACTION=remove "
        );
    }

    #[def_test]
    fn capability_bitmaps_use_linux_word_order() {
        assert_eq!(bitmap_value(&[1, 0, 0, 0, 2]), "200000001");
        assert_eq!(bitmap_value(&[0]), "0");
        assert_eq!(bitmap_value(&[]), "0");
    }

    #[def_test]
    fn actions_accept_only_the_linux_kobject_set() {
        assert_eq!(UeventAction::try_from("change"), Ok(UeventAction::Change));
        assert_eq!(UeventAction::try_from("change\0remove"), Err(()));
    }
}
