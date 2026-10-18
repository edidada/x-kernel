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
    bus::{BusDeviceData, BusId, BusInstanceId},
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
    /// The bus that owns `data`'s vocabulary.
    pub bus: BusId,
    /// The CONCRETE bus instance this device sits on (`pci_dev->bus`),
    /// when the bus has instances. Validated at `add_device`: the
    /// instance must exist and its type must equal `bus`.
    pub bus_instance: Option<BusInstanceId>,
    /// Bus-specific device data (identity, firmware facts, discovery
    /// source) — the bus's vocabulary, opaque to the core.
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
    /// The device's DMA view (mask/offset/coherency), derived from
    /// firmware (`dma-ranges`, `dma-coherent`, PCI bridge windows) at
    /// enumeration. Defaults to [`DmaInfo::DEFAULT`] for board-static
    /// devices; consulted by every DMA allocation — the
    /// `dev->dma_mask`/`dev->dma_pfn_offset` analogue, snapshotted
    /// instead of living in the host allocator.
    pub dma: DmaInfo,
}

impl Clone for DeviceDescriptor {
    fn clone(&self) -> Self {
        Self {
            bus: self.bus,
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
    /// A descriptor for a device on `bus`, carrying the bus's own data.
    pub fn new(bus: BusId, data: Box<dyn BusDeviceData>) -> Self {
        Self {
            bus,
            bus_instance: None,
            parent: None,
            data,
            resources: ResourceMap::new(),
            fw: None,
            dma: DmaInfo::DEFAULT,
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

    /// Set the device's DMA view (the `dma-ranges`/`dma-coherent` derived
    /// mask/offset/coherency). Enumerators that found no firmware DMA
    /// facts leave the [`DmaInfo::DEFAULT`] static-device view in place —
    /// but a firmware-DESCRIBED device with no DMA facts is the
    /// enumerator's call (DT nodes get strict facts: non-coherent,
    /// identity, full mask — per what firmware did NOT say).
    pub fn with_dma(mut self, dma: DmaInfo) -> Self {
        self.dma = dma;
        self
    }

    /// Attach to a concrete bus instance (`pci_set_dev` on the bus
    /// object): the instance must have been created with
    /// [`DeviceDomain::add_bus_instance`](crate::DeviceDomain::add_bus_instance)
    /// and its bus type must equal this descriptor's.
    pub fn with_bus_instance(mut self, instance: BusInstanceId) -> Self {
        self.bus_instance = Some(instance);
        self
    }

    /// Set the parent device (the topology link).
    pub fn with_parent(mut self, parent: Option<DeviceId>) -> Self {
        self.parent = parent;
        self
    }
}
