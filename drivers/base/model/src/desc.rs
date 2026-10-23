// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The device descriptor: the bus-neutral shell every enumeration product
//! fills in.
//!
//! What stays here is exactly what is true of EVERY device regardless of
//! its bus: which bus owns it, its framework identity, its named
//! resources, its DMA view, its topology link. Everything bus-specific —
//! identity for matching, firmware facts, discovery source — is the
//! bus's own vocabulary carried as [`BusDeviceData`].

use alloc::boxed::Box;

use crate::{
    bus::{BusDeviceData, BusInstanceId},
    device::DeviceId,
    firmware::FirmwareNodeInfo,
    resource::{DmaInfo, ResourceMap},
};

/// The product of bus enumeration: everything the domain knows about a
/// device before any driver binds.
///
/// `data` is the owning bus's vocabulary (`PlatformBusData` for the
/// platform bus); the core reads it ONLY through the
/// [`BusDeviceData`] vtable. Ids are assigned by the
/// [`DeviceDomain`](crate::DeviceDomain) on insertion.
#[derive(Debug)]
pub struct DeviceDescriptor {
    /// The existing bus instance on which the enumerator discovered this device.
    /// Its registered type owns `data`'s vocabulary; insertion validates that
    /// the data agrees with that type. Root devices also require an instance.
    pub bus_instance: BusInstanceId,
    /// Bus-specific device data (identity, discovery source) — the
    /// bus's vocabulary, opaque to the core. The firmware property
    /// snapshot is the descriptor's own `fw` field, not bus vocabulary.
    pub data: Box<dyn BusDeviceData>,
    /// The parent device (the topology link). The domain validates the
    /// parent exists at `add_device` time and cascades removals through
    /// children first — a parent never disappears while a child
    /// references it. `None` (the default) marks root devices.
    pub parent: Option<DeviceId>,
    pub resources: ResourceMap,
    /// The firmware node snapshot (`of_property_read_*` surface). True of the
    /// DEVICE, not of the bus: reachable from every bus through
    /// `Device::fw()`.
    pub fw: Option<FirmwareNodeInfo>,
    /// Optional DMA configuration; None rejects DMA allocation before invoking the host.
    pub dma: Option<DmaInfo>,
}

impl Clone for DeviceDescriptor {
    fn clone(&self) -> Self {
        Self {
            bus_instance: self.bus_instance,
            data: alloc::boxed::Box::from(self.data.as_ref()).clone_erased(),
            parent: self.parent,
            resources: self.resources.clone(),
            fw: self.fw.clone(),
            dma: self.dma,
        }
    }
}

impl DeviceDescriptor {
    /// Describe a device discovered on an existing bus instance.
    ///
    /// The platform or controller creates the instance before enumeration.
    /// The enumerator passes its instance here, rather than choosing by bus type.
    pub fn new(bus_instance: BusInstanceId, data: Box<dyn BusDeviceData>) -> Self {
        Self {
            bus_instance,
            parent: None,
            data,
            resources: ResourceMap::new(),
            fw: None,
            dma: None,
        }
    }

    pub fn with_resources(mut self, resources: ResourceMap) -> Self {
        self.resources = resources;
        self
    }

    /// Attach the firmware node snapshot read at enumeration.
    pub fn with_firmware(mut self, fw: Option<FirmwareNodeInfo>) -> Self {
        self.fw = fw;
        self
    }

    /// Configure DMA using constraints established by the enumerator or platform.
    /// Missing firmware facts do not implicitly enable DMA.
    pub fn with_dma(mut self, dma: DmaInfo) -> Self {
        self.dma = Some(dma);
        self
    }

    /// Set the parent device (the topology link).
    pub fn with_parent(mut self, parent: Option<DeviceId>) -> Self {
        self.parent = parent;
        self
    }
}
