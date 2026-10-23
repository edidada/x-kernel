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
    DeviceRef,
    bus::{BusDeviceData, BusInstanceId},
    firmware::FirmwareNodeInfo,
    resource::{DmaInfo, ResourceMap},
};

/// The product of bus enumeration: everything the domain knows about a
/// device before any driver binds.
///
/// `bus_data` is the owning bus's vocabulary (`PlatformBusData` for the
/// platform bus); the core reads it ONLY through the
/// [`BusDeviceData`] vtable. Ids are assigned by the
/// [`DeviceDomain`](crate::DeviceDomain) on insertion.
/// Construct through [`Self::builder`]: `bus_instance` and `bus_data` are
/// required; resources default to empty and parent, firmware and DMA to `None`.
/// Optional setters accept a value or an `Option` via their `_opt` counterpart.
/// Domain insertion remains responsible for validating topology and bus data.
#[derive(Debug, typed_builder::TypedBuilder)]
pub struct DeviceDescriptor {
    /// The existing bus instance on which the enumerator discovered this device.
    /// Its registered type owns `bus_data`'s vocabulary; insertion validates that
    /// the data agrees with that type. Root devices also require an instance.
    pub bus_instance: BusInstanceId,
    /// Bus-specific device data (identity, discovery source) — the
    /// bus's vocabulary, opaque to the core. The firmware property
    /// snapshot is the descriptor's own `fw` field, not bus vocabulary.
    pub bus_data: Box<dyn BusDeviceData>,
    /// The parent object, retained for topology inspection even after removal.
    /// Registration requires this exact object to belong to the same domain
    /// and still accept children. Removal cascades children-first; retaining
    /// the parent preserves metadata, not its binding or hardware resources.
    /// `None` (the default) marks root devices.
    #[builder(default, setter(strip_option(fallback = parent_opt)))]
    pub parent: Option<DeviceRef>,
    #[builder(default)]
    pub resources: ResourceMap,
    /// The firmware node snapshot (`of_property_read_*` surface). True of the
    /// DEVICE, not of the bus: reachable from every bus through
    /// `Device::fw()`.
    #[builder(default, setter(strip_option(fallback = fw_opt)))]
    pub fw: Option<FirmwareNodeInfo>,
    /// Optional DMA configuration (address mask, offset and coherency).
    /// Enumerators or platform code supply it before DMA can be used.
    /// `None` means unconfigured, not proof that the hardware lacks DMA;
    /// model DMA allocation fails without invoking the host service.
    #[builder(default, setter(strip_option(fallback = dma_opt)))]
    pub dma: Option<DmaInfo>,
}

impl Clone for DeviceDescriptor {
    fn clone(&self) -> Self {
        Self {
            bus_instance: self.bus_instance,
            bus_data: alloc::boxed::Box::from(self.bus_data.as_ref()).clone_erased(),
            parent: self.parent.clone(),
            resources: self.resources.clone(),
            fw: self.fw.clone(),
            dma: self.dma,
        }
    }
}

unittest::path_mod_test!(tests, "../ut-tests/desc.rs");
