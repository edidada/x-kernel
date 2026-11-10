// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The kobject-uevent wire and file formats for device nodes.
//!
//! This is the Linux `lib/kobject_uevent.c::kobject_uevent_env()` layer:
//! the rule that a uevent message or a sysfs `uevent` attribute file is
//! `ACTION@DEVPATH\0ACTION=..\0DEVPATH=..\0SUBSYSTEM=..\0<KEY=VALUE>..\0`
//! (and the newline form for the file), plus the capability-bitmap word
//! order shared by sysfs attributes. The rules belong to the node model
//! itself — every consumer (netlink transport, sysfs attributes, tests)
//! formats through this one module.
//!
//! Transport is NOT here: the netlink broadcast lives in
//! `drivers/uevent-netlink`, which subscribes to node lifecycle events and
//! serializes nodes through these functions.

use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};

use crate::DevNode;

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
    pub const fn as_str(self) -> &'static str {
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

/// The `DEVPATH` payload path for a node.
///
/// The canonical sysfs device path is carried ON the node (filled by the
/// publisher, which knows the per-class layout); deriving it here from
/// `physical_path + devname` produced paths the sysfs tree never created
/// for nested layouts like `input/inputN/eventN`.
pub fn uevent_devpath(node: &DevNode) -> String {
    format!("/devices/{}", node.sysfs_path())
}

/// The environment variables of one uevent: `ACTION`, `DEVPATH`,
/// `SUBSYSTEM`, then the node properties (MAJOR/MINOR/DEVNAME/MODALIAS
/// plus class-carried extras).
pub fn uevent_env(node: &DevNode) -> Vec<(String, String)> {
    let mut env = alloc::vec![
        (
            "MAJOR".to_string(),
            node.device_number().major().to_string()
        ),
        (
            "MINOR".to_string(),
            node.device_number().minor().to_string()
        ),
        ("DEVNAME".to_string(), node.devname().to_string()),
    ];
    if let Some(modalias) = node.modalias() {
        env.push(("MODALIAS".to_string(), modalias.to_string()));
    }
    env.extend(node.properties().to_vec());
    env
}

/// The full netlink kobject-uevent payload for `(action, node)`, including
/// the `action@devpath` message header and NUL separators.
pub fn uevent_payload(action: UeventAction, node: &DevNode) -> Vec<u8> {
    let action = action.as_str();
    let devpath = uevent_devpath(node);
    let mut payload = format!("{action}@{devpath}\0ACTION={action}\0DEVPATH={devpath}\0");
    payload.push_str("SUBSYSTEM=");
    payload.push_str(node.subsystem());
    payload.push('\0');
    for (key, value) in uevent_env(node) {
        payload.push_str(&key);
        payload.push('=');
        payload.push_str(&value);
        payload.push('\0');
    }
    payload.into_bytes()
}

/// Renders the newline-delimited contents of a sysfs `uevent` attribute
/// (the file form of [`uevent_env`]).
pub fn uevent_file_contents(node: &DevNode) -> Vec<u8> {
    let mut contents = String::new();
    for (key, value) in uevent_env(node) {
        contents.push_str(&key);
        contents.push('=');
        contents.push_str(&value);
        contents.push('\n');
    }
    contents.into_bytes()
}

/// Formats a little-endian capability bitmap using Linux sysfs word order.
///
/// Bytes are grouped into native 64-bit words, rendered with the highest
/// non-zero word first, and separated by spaces. An empty or all-zero
/// bitmap is represented as `0`.
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
        return String::from("0");
    }
    let last = words.iter().rposition(|word| *word != 0).unwrap_or(0);
    words[..=last]
        .iter()
        .rev()
        .map(|word| format!("{word:x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

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
