// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The userland device-node projection: one registry behind devtmpfs,
//! sysfs, and uevent.
//!
//! Architecturally this is the *model-agnostic* identity that both device
//! models publish into:
//!
//! * a [`DevNodeType`] is **data** — subsystem name, major, index count,
//!   devname format — registered by class crates instead of a closed enum
//!   in the device model. Adding a subsystem (block, tty, netdev, ...)
//!   touches no framework code;
//! * a [`DevNode`] is one published node: devname, `dev_t`, physical path,
//!   optional `MODALIAS`, and class-specific uevent properties;
//! * publications are keyed by an opaque `u64` handle (the class
//!   publication id), so neither device model's id space leaks in.
//!
//! The uevent payload format produced from these nodes is a compatibility
//! surface (udev-visible); its property order and bitmap word order are
//! load-bearing.

#![no_std]

extern crate alloc;

use alloc::{format, string::String, sync::Arc, vec::Vec};

use kspin::SpinNoPreempt;

/// Firmware-stated location of a device (the model-agnostic form every
/// consumer — kclass parents, uevent paths — agrees on).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceLocation {
    Pci {
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
    },
    Mmio {
        base: usize,
        size: usize,
    },
    FirmwareNode {
        id: u16,
    },
    PlatformStatic {
        id: u16,
    },
    Bridge {
        domain: u16,
    },
}

/// Packed `dev_t`: the same layout the block core and devfs use
/// (`kdevice::DeviceNumber` re-exports this type).
#[derive(Default, Clone, PartialEq, Eq, PartialOrd, Ord, Copy, Hash)]
pub struct DeviceNumber(pub u64);

impl DeviceNumber {
    pub const fn new(major: u32, minor: u32) -> Self {
        let major = major as u64;
        let minor = minor as u64;
        Self(
            (major & 0xffff_f000) << 32
                | (major & 0x0000_0fff) << 8
                | (minor & 0xffff_ff00) << 12
                | (minor & 0x0000_00ff),
        )
    }

    pub const fn major(self) -> u32 {
        ((self.0 >> 32) & 0xffff_f000 | (self.0 >> 8) & 0x0000_0fff) as u32
    }

    pub const fn minor(self) -> u32 {
        ((self.0 >> 12) & 0xffff_ff00 | self.0 & 0x0000_00ff) as u32
    }
}

impl core::fmt::Debug for DeviceNumber {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DeviceNumber")
            .field("major", &self.major())
            .field("minor", &self.minor())
            .finish()
    }
}

/// One class's node type: how index → devname and `dev_t` are formed.
///
/// Registered by class crates (input, drm, block, ...); the framework never
/// enumerates subsystems.
#[derive(Clone)]
pub struct DevNodeType {
    /// Subsystem name (`"input"`, `"drm"`, `"block"`, ...).
    pub subsystem: &'static str,
    /// Registered major for this subsystem's nodes.
    pub major: u32,
    /// Minor base plus per-index count (input reserves 32 event minors).
    pub minor_base: u32,
    pub index_count: u32,
    /// devname for `index` (`input/event{index}`, `dri/card{index}`).
    pub devname_for: fn(index: u32) -> String,
}

impl DevNodeType {
    pub const fn new(
        subsystem: &'static str,
        major: u32,
        minor_base: u32,
        index_count: u32,
        devname_for: fn(index: u32) -> String,
    ) -> Self {
        Self {
            subsystem,
            major,
            minor_base,
            index_count,
            devname_for,
        }
    }

    pub fn device_number(&self, index: u32) -> DeviceNumber {
        DeviceNumber::new(self.major, self.minor_base + index)
    }

    pub fn devname(&self, index: u32) -> String {
        (self.devname_for)(index)
    }
}

/// One published userland device node.
#[derive(Clone)]
pub struct DevNode {
    id: u64,
    subsystem: &'static str,
    devname: String,
    device_number: DeviceNumber,
    /// Physical device path (`devices/...` prefix source for DEVPATH).
    physical_path: String,
    /// `MODALIAS` derived from the device's id-table match, when known.
    modalias: Option<String>,
    /// Class-specific uevent properties (`KEY=..`, `NAME=..`, ...).
    properties: Vec<(String, String)>,
    driver_name: Option<String>,
    available: bool,
}

impl DevNode {
    /// Opaque publication handle (class publication id).
    pub const fn id(&self) -> u64 {
        self.id
    }

    pub fn subsystem(&self) -> &'static str {
        self.subsystem
    }

    pub fn devname(&self) -> &str {
        &self.devname
    }

    pub const fn device_number(&self) -> DeviceNumber {
        self.device_number
    }

    pub fn physical_path(&self) -> &str {
        &self.physical_path
    }

    pub fn modalias(&self) -> Option<&str> {
        self.modalias.as_deref()
    }

    pub fn properties(&self) -> &[(String, String)] {
        &self.properties
    }

    pub fn is_available(&self) -> bool {
        self.available
    }

    pub fn driver_name(&self) -> Option<&str> {
        self.driver_name.as_deref()
    }
}

impl PartialEq for DevNode {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.subsystem == other.subsystem && self.devname == other.devname
    }
}

impl Eq for DevNode {}

/// Lifecycle events, in the order !796 established:
/// `projection add -> add uevent` / `mark removing -> projection quiesce ->
/// remove uevent -> projection delete`.
#[derive(Clone)]
pub enum DevNodeEvent {
    Added(DevNode),
    Removing(DevNode),
    Removed(DevNode),
}

pub type DevNodeCallback = Arc<dyn Fn(DevNodeEvent) + Send + Sync>;

struct Registry {
    nodes: Vec<DevNode>,
    subscribers: Vec<DevNodeCallback>,
    lifecycle_publisher: Option<DevNodeCallback>,
}

static REGISTRY: SpinNoPreempt<Registry> = SpinNoPreempt::new(Registry {
    nodes: Vec::new(),
    subscribers: Vec::new(),
    lifecycle_publisher: None,
});

/// Publish a node for `id`; `index` resolves devname/`dev_t` through
/// `node_type`. Returns the node.
pub fn publish_indexed(
    id: u64,
    node_type: &DevNodeType,
    index: u32,
    physical_path: &str,
    modalias: Option<String>,
    properties: Vec<(String, String)>,
) -> DevNode {
    publish(
        id,
        node_type.subsystem,
        node_type.devname(index),
        node_type.device_number(index),
        physical_path,
        modalias,
        properties,
        None,
    )
}

/// Publish a node with an explicit devname and `dev_t` (block disks name
/// themselves: `vda`, `sda`, ...).
pub fn publish(
    id: u64,
    subsystem: &'static str,
    devname: String,
    device_number: DeviceNumber,
    physical_path: &str,
    modalias: Option<String>,
    properties: Vec<(String, String)>,
    driver_name: Option<String>,
) -> DevNode {
    let node = DevNode {
        id,
        subsystem,
        devname,
        device_number,
        physical_path: String::from(physical_path),
        modalias,
        properties,
        driver_name,
        available: true,
    };
    let (subscribers, publisher) = {
        let mut registry = REGISTRY.lock();
        if let Some(existing) = registry.nodes.iter_mut().find(|n| n.id == id) {
            *existing = node.clone();
            return existing.clone();
        }
        registry.nodes.push(node.clone());
        (
            registry.subscribers.clone(),
            registry.lifecycle_publisher.clone(),
        )
    };
    dispatch_added(&node, subscribers, publisher);
    node
}

/// Remove the publication for `id` (idempotent).
pub fn withdraw(id: u64) {
    let node = {
        let mut registry = REGISTRY.lock();
        let Some(node) = registry.nodes.iter_mut().find(|n| n.id == id) else {
            return;
        };
        node.available = false;
        node.clone()
    };
    let (subscribers, publisher) = {
        let registry = REGISTRY.lock();
        (
            registry.subscribers.clone(),
            registry.lifecycle_publisher.clone(),
        )
    };
    for subscriber in &subscribers {
        subscriber(DevNodeEvent::Removing(node.clone()));
    }
    if let Some(publisher) = publisher {
        publisher(DevNodeEvent::Removed(node.clone()));
    }
    REGISTRY.lock().nodes.retain(|n| n.id != id);
    for subscriber in subscribers {
        subscriber(DevNodeEvent::Removed(node.clone()));
    }
}

/// Available-node snapshot.
pub fn nodes() -> Vec<DevNode> {
    REGISTRY
        .lock()
        .nodes
        .iter()
        .filter(|n| n.available)
        .cloned()
        .collect()
}

/// Subscribe to node events; returns the current snapshot.
pub fn subscribe(callback: DevNodeCallback) -> Vec<DevNode> {
    let mut registry = REGISTRY.lock();
    registry.subscribers.push(callback);
    registry
        .nodes
        .iter()
        .filter(|n| n.available)
        .cloned()
        .collect()
}

/// Install the single uevent lifecycle publisher. Must run before any
/// publication (boot-order discipline, same as !796).
pub fn install_lifecycle_publisher(callback: DevNodeCallback) -> Result<(), ()> {
    let mut registry = REGISTRY.lock();
    if registry.lifecycle_publisher.is_some() {
        return Err(());
    }
    if !registry.nodes.is_empty() {
        return Err(());
    }
    registry.lifecycle_publisher = Some(callback);
    Ok(())
}

fn dispatch_added(
    node: &DevNode,
    subscribers: Vec<DevNodeCallback>,
    publisher: Option<DevNodeCallback>,
) {
    for subscriber in subscribers {
        subscriber(DevNodeEvent::Added(node.clone()));
    }
    if let Some(publisher) = publisher {
        publisher(DevNodeEvent::Added(node.clone()));
    }
}

/// Format the physical device path (DEVPATH source) from a firmware
/// location. Lives here so both device models' adapters share one format.
pub fn physical_path_pci(segment: u16, bus: u8, device: u8, function: u8) -> String {
    format!("pci{segment:04x}:{bus:02x}/{segment:04x}:{bus:02x}:{device:02x}.{function}")
}

/// Linux capability-bitmap formatter: LE words, printed as space-separated
/// hex words in big-endian word order, trailing zero words dropped.
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
    use alloc::string::String;

    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn devnode_type_projects_devname_and_number() {
        let t = DevNodeType::new("input", 13, 64, 32, |i| format!("input/event{i}"));
        assert_eq!(t.devname(3), String::from("input/event3"));
        assert_eq!(t.device_number(3).major(), 13);
        assert_eq!(t.device_number(3).minor(), 64 + 3);
    }

    #[def_test]
    fn bitmap_format_keeps_linux_word_order() {
        // Anchor from the !796 tests: LE words, big-endian word order,
        // trailing zero words dropped.
        assert_eq!(bitmap_value(&[1, 0, 0, 0, 2]), String::from("200000001"));
        assert_eq!(bitmap_value(&[0]), String::from("0"));
        assert_eq!(bitmap_value(&[]), String::from("0"));
    }

    #[def_test(serial)]
    fn publish_withdraw_lifecycle_orders_events() {
        let observed = Arc::new(kspin::SpinNoPreempt::new(
            alloc::vec::Vec::<&'static str>::new(),
        ));
        let sink = observed.clone();
        subscribe(Arc::new(move |event| match event {
            DevNodeEvent::Added(_) => sink.lock().push("add"),
            DevNodeEvent::Removing(_) => sink.lock().push("removing"),
            DevNodeEvent::Removed(_) => sink.lock().push("removed"),
        }));

        publish(
            0x9999,
            "test-projection",
            String::from("vdz"),
            DeviceNumber::new(254, 16),
            "pci0000:00/0000:00:01.0",
            Some(String::from("pci:v00001AF4d00001001")),
            Vec::new(),
            Some(String::from("virtio-blk")),
        );
        withdraw(0x9999);

        let events = observed.lock().clone();
        assert_eq!(events.as_slice(), ["add", "removing", "removed"]);
        // Boot-time publications may exist; ours must be gone.
        assert!(!nodes().iter().any(|n| n.id() == 0x9999));
    }
}
