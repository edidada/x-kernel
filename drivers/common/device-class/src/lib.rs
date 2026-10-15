// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Typed runtime device classes.
//!
//! One generic registry implementation serves every class; each class is a
//! named static instance plus thin forwarding functions whose names stay
//! stable for consumers (`publish_char`, `net_devices`, ...). Class-specific
//! behavior — block disk registration, input/display devnode projection —
//! lives in the [`ClassRuntimeMetadata`] impls, the Linux `struct class`
//! function-pointer equivalent.
//!
//! Drivers publish runtime capabilities here after probe succeeds so
//! subsystems can enumerate existing devices and subscribe to later
//! availability without depending on probe order.

#![no_std]

extern crate alloc;

#[cfg(feature = "input")]
use alloc::collections::BTreeMap;
use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};

use kerrno::KResult;
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;

mod class;
mod removal;

pub use driver_base::class::{
    ClassAvailabilityCallback, ClassDevice, ClassDeviceId, ClassDeviceMetadata, ClassParent,
    ClassRegistry, ClassRemovalCallback, ClassUse, ClassWithdraw, DetachedParent,
};
pub use removal::remove_all_detached;
#[cfg(feature = "block")]
pub use removal::{block_withdraw, unpublish_block};
#[cfg(feature = "char")]
pub use removal::{char_withdraw, unpublish_char};
#[cfg(feature = "display")]
pub use removal::{display_withdraw, unpublish_display};
#[cfg(feature = "hwrng")]
pub use removal::{hwrng_withdraw, unpublish_hwrng};
#[cfg(feature = "input")]
pub use removal::{input_withdraw, unpublish_input};
#[cfg(feature = "net")]
pub use removal::{net_withdraw, unpublish_net};
#[cfg(feature = "virtio-9p")]
pub use removal::{unpublish_virtio_9p, virtio_9p_withdraw};
#[cfg(feature = "vsock")]
pub use removal::{unpublish_vsock, vsock_withdraw};

// ---------------------------------------------------------------------------
// Concrete class runtime types
// ---------------------------------------------------------------------------

#[cfg(feature = "net")]
pub type NetDeviceImpl = Box<dyn NetDevice + Send + Sync>;

#[cfg(feature = "block")]
pub type BlockDeviceImpl = Arc<block::Gendisk>;

#[cfg(feature = "char")]
pub type CharDeviceImpl = Box<dyn CharDevice + Send + Sync>;

/// Shared hardware entropy source, also retained by its host activation owner.
///
/// Publish only after the driver can receive completion IRQs and accept reads.
/// Registry removal does not stop the source or prove that DMA has stopped.
#[cfg(feature = "hwrng")]
pub type HwrngDeviceImpl = Arc<dyn HwrngDevice + Send + Sync>;

#[cfg(feature = "hwrng")]
impl ClassRuntimeMetadata for HwrngDeviceImpl {}

#[cfg(feature = "display")]
pub type DisplayDeviceImpl = Box<dyn DisplayDevice + Send + Sync>;

#[cfg(feature = "input")]
pub type InputDeviceImpl = Box<dyn InputDevice + Send + Sync>;

#[cfg(feature = "vsock")]
pub type VsockDeviceImpl = Box<dyn VsockDevice + Send + Sync>;

#[cfg(feature = "virtio-9p")]
pub type Virtio9pDeviceImpl = Box<dyn Virtio9pDevice + Send + Sync>;

#[cfg(feature = "block")]
pub use block::Gendisk;
#[cfg(feature = "char")]
pub use char_driver::CharDevice;
#[cfg(feature = "display")]
pub use display::{DisplayDevice, DisplayInfo};
#[cfg(feature = "hwrng")]
pub use hwrng::HwrngDevice;
#[cfg(feature = "input")]
pub use input::{Event, EventType, InputDevice, InputDeviceId, InputEventNotifier};
#[cfg(feature = "net")]
pub use net::{NetBufHandle, NetDevice, NetRxScheduler};
#[cfg(feature = "virtio-9p")]
pub use virtio_devices::Virtio9pDevice;
#[cfg(feature = "vsock")]
pub use vsock::{
    VsockAddr, VsockConnId, VsockConnectionInfo, VsockDevice, VsockTransportEvent,
    VsockTransportEventKind,
};

// ---------------------------------------------------------------------------
// Per-class behavior (the `struct class` function-pointer slots)
// ---------------------------------------------------------------------------

trait ClassRuntimeMetadata {
    fn class_metadata(&self) -> ClassDeviceMetadata {
        ClassDeviceMetadata::empty()
    }

    fn publish_class(&self) -> KResult<()> {
        Ok(())
    }

    fn unpublish_class(&self) {}

    fn device_node_class(&self) -> Option<NodeClass> {
        None
    }
}

/// The devnode publication shape for class devices that project into
/// devtmpfs/sysfs/uevent.
pub enum NodeClass {
    Input(InputNodeMetadata),
    Drm,
}

/// Input capability metadata carried into the node's uevent properties.
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

#[cfg(feature = "block")]
impl ClassRuntimeMetadata for BlockDeviceImpl {
    fn publish_class(&self) -> KResult<()> {
        block::add_disk(self.clone()).map(drop)
    }

    fn unpublish_class(&self) {
        block::del_gendisk(self.device_number());
    }
}

#[cfg(feature = "char")]
impl ClassRuntimeMetadata for CharDeviceImpl {}

#[cfg(feature = "display")]
impl ClassRuntimeMetadata for DisplayDeviceImpl {
    fn device_node_class(&self) -> Option<NodeClass> {
        Some(NodeClass::Drm)
    }
}

#[cfg(feature = "input")]
impl ClassRuntimeMetadata for InputDeviceImpl {
    fn class_metadata(&self) -> ClassDeviceMetadata {
        ClassDeviceMetadata::input(self.physical_location().into(), self.unique_id().into())
    }

    fn device_node_class(&self) -> Option<NodeClass> {
        let mut event_types = alloc::vec![0; (EventType::COUNT as usize).div_ceil(8)];
        let mut event_codes = BTreeMap::new();
        for raw in 0..EventType::COUNT {
            let Some(event_type) = EventType::from_repr(raw) else {
                continue;
            };
            let mut codes = alloc::vec![0; event_type.bits_count().div_ceil(8)];
            if self
                .get_event_bits(event_type, &mut codes)
                .is_ok_and(|is_supported| is_supported)
            {
                event_types[raw as usize / 8] |= 1 << (raw as usize % 8);
                event_codes.insert(raw, codes);
            }
        }
        let id = self.device_id();
        Some(NodeClass::Input(InputNodeMetadata {
            name: String::from(self.name()),
            physical_location: String::from(self.physical_location()),
            unique_id: String::from(self.unique_id()),
            bus_type: id.bus_type,
            vendor: id.vendor,
            product: id.product,
            version: id.version,
            event_types,
            event_codes: event_codes.into_iter().collect(),
        }))
    }
}

#[cfg(feature = "net")]
impl ClassRuntimeMetadata for NetDeviceImpl {}

#[cfg(feature = "vsock")]
impl ClassRuntimeMetadata for VsockDeviceImpl {}

#[cfg(feature = "virtio-9p")]
impl ClassRuntimeMetadata for Virtio9pDeviceImpl {}

// ---------------------------------------------------------------------------
// Public API: per-class forwarding functions (names are the stable surface)
// ---------------------------------------------------------------------------

macro_rules! class_api {
    (
        $feature:literal,
        $ty:ty,
        $registry:ident,
        $registry_fn:ident,
        $publish_fn:ident,
        $devices_fn:ident,
        $find_fn:ident,
        $subscribe_fn:ident,
        $removed_subscriber_fn:ident
    ) => {
        #[cfg(feature = $feature)]
        static $registry: LazyInit<SpinNoPreempt<ClassRegistry<$ty>>> = LazyInit::new();

        #[cfg(feature = $feature)]
        fn $registry_fn() -> &'static SpinNoPreempt<ClassRegistry<$ty>> {
            $registry.call_once(|| SpinNoPreempt::new(ClassRegistry::new()));
            $registry.get().expect("class registry call_once succeeded")
        }

        #[cfg(feature = $feature)]
        #[doc = concat!("Publish a runtime capability into the `", $feature, "` class.")]
        pub fn $publish_fn(
            parent: impl Into<ClassParent>,
            runtime: $ty,
            name: impl Into<String>,
        ) -> KResult<ClassDevice<$ty>> {
            // The name arrives explicitly: the driver built it, it owns it.
            // No runtime-tag extraction — the registry's type is the class.
            let name = name.into();
            let metadata = runtime.class_metadata();
            let device = ClassDevice::try_new_with_class_metadata(parent, runtime, name, metadata)?;
            device.with(ClassRuntimeMetadata::publish_class)?;
            let id = device.id();
            let is_active = device.is_available();
            if let Err(error) = $registry_fn().lock().publish(device.clone()) {
                device.with(ClassRuntimeMetadata::unpublish_class);
                return Err(error);
            }
            log::debug!(
                "device class '{}': published {:?} (driver={})",
                $feature,
                id,
                device.driver_name(),
            );
            if is_active {
                class_notify::<$ty>($registry_fn, id);
            }
            Ok(device)
        }

        #[cfg(feature = $feature)]
        #[doc = concat!("Return every currently available `", $feature, "` device.")]
        pub fn $devices_fn() -> Vec<ClassDevice<$ty>> {
            $registry_fn().lock().devices()
        }

        #[cfg(feature = $feature)]
        pub fn $find_fn(id: ClassDeviceId) -> Option<ClassDevice<$ty>> {
            $registry_fn().lock().find(id)
        }

        #[cfg(feature = $feature)]
        pub fn $subscribe_fn(callback: ClassAvailabilityCallback<$ty>) {
            $registry_fn().lock().subscribe_available(callback);
        }

        #[cfg(feature = $feature)]
        pub fn $removed_subscriber_fn(callback: ClassRemovalCallback<$ty>) {
            $registry_fn().lock().subscribe_removed(callback);
        }
    };
}

/// Shared notify-with-devnode logic (replaces the macro's generated body).
fn class_notify<T: ClassRuntimeMetadata>(
    registry_fn: fn() -> &'static SpinNoPreempt<ClassRegistry<T>>,
    id: ClassDeviceId,
) {
    let (device, callbacks) = {
        let registry = registry_fn().lock();
        let Some(device) = registry.find(id) else {
            return;
        };
        (device, registry.availability_callbacks())
    };

    if let Some(node_class) = device.with(ClassRuntimeMetadata::device_node_class) {
        let node_id = device.parent_ref().id().raw();
        let parent_location = device.location();
        match node_class {
            NodeClass::Input(metadata) => {
                node_compat::input_node(node_id, metadata, device.driver_name(), parent_location);
            }
            NodeClass::Drm => {
                node_compat::drm_node(node_id, device.driver_name(), parent_location);
            }
        }
    }

    for callback in callbacks {
        callback(device.clone());
    }
}

/// Shared removal logic.
fn class_remove<T: ClassRuntimeMetadata>(
    registry_fn: fn() -> &'static SpinNoPreempt<ClassRegistry<T>>,
    id: ClassDeviceId,
) {
    let (removed, callbacks) = {
        let mut registry = registry_fn().lock();
        let removed = registry.remove(id);
        let callbacks = registry.removal_callbacks();
        (removed, callbacks)
    };
    if let Some(device) = removed {
        // Usage gate FIRST: new acquire_use calls fail from here; the
        // in-flight count at close time is what a draining subsystem
        // waits on. Then the driver's remove callbacks still see an
        // available device (their own final operations run under the
        // gate they already hold). Only then flip the shared offline
        // flag every consumer clone can observe, and withdraw.
        device.begin_drain();
        for callback in callbacks {
            callback(device.clone());
        }
        device.parent_ref().mark_offline();
        let node_id = device.parent_ref().id().raw();
        devnode::withdraw(node_id);
        device.with(ClassRuntimeMetadata::unpublish_class);
    }
}

class_api!(
    "net",
    NetDeviceImpl,
    NET_DEVICES,
    net_class_registry,
    publish_net,
    net_devices,
    find_net_device,
    subscribe_net_available,
    subscribe_net_removed
);

class_api!(
    "hwrng",
    HwrngDeviceImpl,
    HWRNG_DEVICES,
    hwrng_class_registry,
    publish_hwrng,
    hwrng_devices,
    find_hwrng_device,
    subscribe_hwrng_available,
    subscribe_hwrng_removed
);

class_api!(
    "block",
    BlockDeviceImpl,
    BLOCK_DEVICES,
    block_class_registry,
    publish_block,
    block_devices,
    find_block_device,
    subscribe_block_available,
    subscribe_block_removed
);

class_api!(
    "char",
    CharDeviceImpl,
    CHAR_DEVICES,
    char_class_registry,
    publish_char,
    char_devices,
    find_char_device,
    subscribe_char_available,
    subscribe_char_removed
);

class_api!(
    "display",
    DisplayDeviceImpl,
    DISPLAY_DEVICES,
    display_class_registry,
    publish_display,
    display_devices,
    find_display_device,
    subscribe_display_available,
    subscribe_display_removed
);

class_api!(
    "input",
    InputDeviceImpl,
    INPUT_DEVICES,
    input_class_registry,
    publish_input,
    input_devices,
    find_input_device,
    subscribe_input_available,
    subscribe_input_removed
);

class_api!(
    "vsock",
    VsockDeviceImpl,
    VSOCK_DEVICES,
    vsock_class_registry,
    publish_vsock,
    vsock_devices,
    find_vsock_device,
    subscribe_vsock_available,
    subscribe_vsock_removed
);

class_api!(
    "virtio-9p",
    Virtio9pDeviceImpl,
    VIRTIO_9P_DEVICES,
    virtio_9p_class_registry,
    publish_virtio_9p,
    virtio_9p_devices,
    find_virtio_9p_device,
    subscribe_virtio_9p_available,
    subscribe_virtio_9p_removed
);

// ---------------------------------------------------------------------------
#[cfg(feature = "char")]
pub fn remove_char_device(id: ClassDeviceId) {
    class_remove(char_class_registry, id);
}

#[cfg(feature = "net")]
pub fn remove_net_device(id: ClassDeviceId) {
    class_remove(net_class_registry, id);
}

// ---------------------------------------------------------------------------
// devnode projection for input/drm classes
// ---------------------------------------------------------------------------

mod node_compat {
    use alloc::{format, string::String, vec::Vec};

    use devnode::{DevNodeType, bitmap_value};

    use super::InputNodeMetadata;

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

    /// Build the input node publication (eventN with parent-path properties).
    pub(super) fn input_node(
        id: u64,
        metadata: InputNodeMetadata,
        driver_name: &str,
        parent_path: &str,
    ) {
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
        match devnode::publish_auto_indexed(
            id,
            &INPUT_TYPE,
            &format!("{physical}/input"),
            None,
            properties,
            Some(String::from(driver_name)),
        ) {
            Ok(_) => {}
            Err(devnode::PublishError::ResourceBusy) => {
                // Index space exhausted: NO fallback node. Any synthesized
                // dev_t either aliases a live device (devfs cdev_add would
                // overwrite it; removing either device tears down both) or
                // fabricates a node userspace cannot distinguish. The class
                // device itself keeps working; only the /dev and uevent
                // projections are absent, loudly.
                log::error!(
                    "input: event index space exhausted ({}); device '{}' gets no /dev node and \
                     no uevent",
                    INPUT_TYPE.index_count,
                    metadata.name
                );
            }
            Err(devnode::PublishError::AlreadyExists) => {
                log::error!("input: devnode id {id} already published; skipping");
            }
        }
    }

    /// Build the drm node publication (cardN).
    pub(super) fn drm_node(id: u64, driver_name: &str, parent_path: &str) {
        let physical = String::from(parent_path);
        // udev rules match DRM nodes on DEVTYPE=drm_minor (permissions,
        // device ownership); losing it changes /dev/dri/cardN handling.
        let properties = alloc::vec![(String::from("DEVTYPE"), String::from("drm_minor"))];
        match devnode::publish_auto_indexed(
            id,
            &DRM_TYPE,
            &physical,
            None,
            properties,
            Some(String::from(driver_name)),
        ) {
            Ok(_) => {}
            Err(devnode::PublishError::ResourceBusy) => {
                log::error!(
                    "drm: card index space exhausted ({}); device gets no /dev node and no uevent",
                    DRM_TYPE.index_count
                );
            }
            Err(devnode::PublishError::AlreadyExists) => {
                log::error!("drm: devnode id {id} already published; skipping");
            }
        }
    }
}

pub mod prelude {
    #[cfg(feature = "block")]
    pub use block::BlockDeviceOperations;
    pub use kerrno::{KError, KResult};

    #[cfg(feature = "block")]
    pub use crate::BlockDeviceImpl;
    #[cfg(feature = "char")]
    pub use crate::{CharDevice, CharDeviceImpl, subscribe_char_removed};
    #[cfg(feature = "display")]
    pub use crate::{DisplayDevice, DisplayDeviceImpl, DisplayInfo};
    #[cfg(feature = "input")]
    pub use crate::{Event, EventType, InputDevice, InputDeviceId, InputDeviceImpl};
    #[cfg(feature = "net")]
    pub use crate::{
        NetBufHandle, NetDevice, NetDeviceImpl, NetRxScheduler, subscribe_net_removed,
    };
    #[cfg(feature = "virtio-9p")]
    pub use crate::{Virtio9pDevice, Virtio9pDeviceImpl};
    #[cfg(feature = "vsock")]
    pub use crate::{
        VsockAddr, VsockConnId, VsockConnectionInfo, VsockDevice, VsockDeviceImpl,
        VsockTransportEvent, VsockTransportEventKind,
    };
}
