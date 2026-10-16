// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The node classes a device class can project into devtmpfs/sysfs/
//! uevent (the Linux class→devnode vocabulary: input's eventN, DRM's
//! cardN). The node MODEL owns how these project — the families only
//! declare which they are and supply the metadata.

use alloc::{format, string::String, vec::Vec};

use crate::{DevNodeType, PublishError, bitmap_value};

/// The devnode publication shape for class devices that project into
/// devtmpfs/sysfs/uevent.
pub enum NodeClass {
    Input(InputNodeMetadata),
    Drm,
}

/// Input capability metadata carried into the node's uevent properties.
#[derive(Clone)]
pub struct InputNodeMetadata {
    pub name: String,
    pub physical_location: String,
    pub unique_id: String,
    pub bus_type: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
    pub event_types: Vec<u8>,
    pub event_codes: Vec<(u8, Vec<u8>)>,
}

static INPUT_TYPE: DevNodeType = DevNodeType::new(
    "input",
    13,
    64,
    32,
    |index| format!("input/event{index}"),
    // physical_path here already ends with the `input` segment; the
    // canonical layout nests inputN under it and eventN under inputN.
    |physical, index| format!("{physical}/input{index}/event{index}"),
);
static DRM_TYPE: DevNodeType = DevNodeType::new(
    "drm",
    226,
    0,
    64,
    |index| format!("dri/card{index}"),
    |physical, index| format!("{physical}/drm/card{index}"),
);

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn input_property_name(event_type: u8) -> Option<&'static str> {
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

impl NodeClass {
    /// Project this node class: publish the devnode and its uevent
    /// properties. Called by the class layer when a device becomes
    /// available — the node model decides the shape, the caller supplies
    /// identity.
    pub fn project(&self, id: u64, driver_name: &str, parent_path: &str) {
        match self {
            Self::Input(metadata) => input_node(id, (*metadata).clone(), driver_name, parent_path),
            Self::Drm => drm_node(id, driver_name, parent_path),
        }
    }
}

fn input_node(id: u64, metadata: InputNodeMetadata, driver_name: &str, parent_path: &str) {
    let physical = String::from(parent_path);
    let mut properties = Vec::new();
    properties.push((
        String::from("PRODUCT"),
        format!(
            "{:x}/{:x}/{:x}/{:x}",
            metadata.bus_type, metadata.vendor, metadata.product, metadata.version
        ),
    ));
    properties.push((String::from("NAME"), sanitize(&metadata.name)));
    properties.push((String::from("PHYS"), sanitize(&metadata.physical_location)));
    if !metadata.unique_id.is_empty() {
        properties.push((String::from("UNIQ"), sanitize(&metadata.unique_id)));
    }
    properties.push((String::from("PROP"), String::from("0")));
    properties.push((String::from("EV"), bitmap_value(&metadata.event_types)));
    for (event_type, bits) in &metadata.event_codes {
        if let Some(key) = input_property_name(*event_type) {
            properties.push((String::from(key), bitmap_value(bits)));
        }
    }
    match crate::publish_auto_indexed(
        id,
        &INPUT_TYPE,
        &format!("{physical}/input"),
        None,
        properties,
        Some(String::from(driver_name)),
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
                "input: event index space exhausted ({}); device '{}' gets no /dev node and no \
                 uevent",
                INPUT_TYPE.index_count,
                metadata.name
            );
        }
        Err(PublishError::AlreadyExists) => {
            log::error!("input: devnode id {id} already published; skipping");
        }
    }
}

/// Build the drm node publication (cardN).
fn drm_node(id: u64, driver_name: &str, parent_path: &str) {
    let physical = String::from(parent_path);
    // udev rules match DRM nodes on DEVTYPE=drm_minor (permissions,
    // device ownership); losing it changes /dev/dri/cardN handling.
    let properties = alloc::vec![(String::from("DEVTYPE"), String::from("drm_minor"))];
    match crate::publish_auto_indexed(
        id,
        &DRM_TYPE,
        &physical,
        None,
        properties,
        Some(String::from(driver_name)),
    ) {
        Ok(_) => {}
        Err(PublishError::ResourceBusy) => {
            log::error!(
                "drm: card index space exhausted ({}); device gets no /dev node and no uevent",
                DRM_TYPE.index_count
            );
        }
        Err(PublishError::AlreadyExists) => {
            log::error!("drm: devnode id {id} already published; skipping");
        }
    }
}
