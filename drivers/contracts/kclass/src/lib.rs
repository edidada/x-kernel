// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Typed runtime device classes built on top of the device core.
//!
//! The generic class-device handle and registry mechanics live in the internal
//! `generic` module. This crate root wires those mechanics to concrete
//! block/net/char/hwrng/display/input/vsock/9P operation traits.
//! Drivers publish runtime capabilities here after probe succeeds so subsystems
//! can enumerate existing devices and subscribe to later availability without
//! depending on probe order.
//! Input and display classes also project their runtime metadata into
//! devnode publications (devtmpfs/sysfs/uevent) on publish.

#![no_std]

extern crate alloc;

#[cfg(feature = "input")]
use alloc::collections::BTreeMap;
use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};

use driver_base::{Device, DeviceKind, DriverError, DriverResult};
pub use generic::{ClassDeviceId, ClassParent, DetachedParent};
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;

mod generic;

/// Re-export generic class-device primitives that are part of the public API.
pub use self::generic::{ClassAvailabilityCallback, ClassDevice};
use self::generic::{ClassDeviceMetadata, ClassRegistry, ClassRemovalCallback};

macro_rules! class_registry {
    (
        $feature:literal,
        $ty:ty,
        $kind:expr,
        $registry:ident,
        $registry_fn:ident,
        $publish_fn:ident,
        $devices_fn:ident,
        $find_fn:ident,
        $subscribe_fn:ident,
        $removed_subscriber_fn:ident,
        $notify_fn:ident,
        $remove_fn:ident
    ) => {
        #[cfg(feature = $feature)]
        static $registry: LazyInit<SpinNoPreempt<ClassRegistry<$ty>>> = LazyInit::new();

        #[cfg(feature = $feature)]
        fn $registry_fn() -> &'static SpinNoPreempt<ClassRegistry<$ty>> {
            $registry.call_once(|| SpinNoPreempt::new(ClassRegistry::new()));
            $registry.get().expect("class registry call_once succeeded")
        }

        /// Publish a runtime capability into its typed class registry.
        #[cfg(feature = $feature)]
        pub fn $publish_fn(
            parent: impl Into<ClassParent>,
            runtime: $ty,
        ) -> DriverResult<ClassDevice<$ty>> {
            let name = runtime.name().into();
            let device_kind = runtime.device_kind();
            let irq = runtime.irq();
            if device_kind != $kind {
                log::warn!(
                    "device class '{}': runtime kind {:?} does not match registry kind {:?}",
                    $kind.as_str(),
                    device_kind,
                    $kind,
                );
                return Err(DriverError::InvalidInput);
            }
            let metadata = runtime.class_metadata();
            let device = ClassDevice::try_new_with_class_metadata(
                parent, runtime, name, $kind, irq, metadata,
            )?;
            device.with(ClassRuntimeMetadata::publish_class)?;
            let id = device.id();
            let is_active = device.is_available();
            if let Err(error) = $registry_fn().lock().publish(device.clone()) {
                device.with(ClassRuntimeMetadata::unpublish_class);
                return Err(error);
            }
            log::debug!(
                "device class '{}': published {:?} (driver={})",
                $kind.as_str(),
                id,
                device.driver_name(),
            );
            if is_active {
                notify_class_available($kind, id);
            }
            Ok(device)
        }

        /// Return every currently available runtime capability for this class.
        #[cfg(feature = $feature)]
        pub fn $devices_fn() -> Vec<ClassDevice<$ty>> {
            $registry_fn().lock().devices()
        }

        /// Find one currently available runtime capability by device ID.
        #[cfg(feature = $feature)]
        pub fn $find_fn(id: ClassDeviceId) -> Option<ClassDevice<$ty>> {
            $registry_fn().lock().find(id)
        }

        /// Subscribe to future runtime capabilities for this class.
        #[cfg(feature = $feature)]
        pub fn $subscribe_fn(callback: ClassAvailabilityCallback<$ty>) {
            $registry_fn().lock().subscribe_available(callback);
        }

        #[cfg(feature = $feature)]
        pub fn $removed_subscriber_fn(callback: ClassRemovalCallback<$ty>) {
            $registry_fn().lock().subscribe_removed(callback);
        }

        #[cfg(feature = $feature)]
        fn $notify_fn(id: ClassDeviceId) {
            let (device, callbacks) = {
                let registry = $registry_fn().lock();
                let Some(device) = registry.find(id) else {
                    return;
                };
                (device, registry.availability_callbacks())
            };

            if let Some(node_class) = device.with(ClassRuntimeMetadata::device_node_class) {
                let node_id = device.parent_ref().id().raw();
                let legacy = device.location();
                match node_class {
                    NodeClass::Input(metadata) => {
                        crate::node_compat::input_node(
                            node_id,
                            metadata,
                            &device.driver_name(),
                            legacy,
                        );
                    }
                    NodeClass::Drm => {
                        crate::node_compat::drm_node(node_id, &device.driver_name(), legacy);
                    }
                }
            }

            for callback in callbacks {
                callback(device.clone());
            }
        }

        #[cfg(feature = $feature)]
        fn $remove_fn(id: ClassDeviceId) {
            let (removed, callbacks) = {
                let mut registry = $registry_fn().lock();
                let removed = registry.remove(id);
                let callbacks = registry.removal_callbacks();
                (removed, callbacks)
            };
            if let Some(device) = removed {
                for callback in callbacks {
                    callback(device.clone());
                }
                let node_id = device.parent_ref().id().raw();
                devnode::withdraw(node_id);
                device.with(ClassRuntimeMetadata::unpublish_class);
            }
        }
    };
}

macro_rules! class_registries {
    (
        $(
            $feature:literal,
            $ty:ty,
            $kind:path,
            $registry:ident,
            $registry_fn:ident,
            $publish_fn:ident,
            $devices_fn:ident,
            $find_fn:ident,
            $subscribe_fn:ident,
            $removed_subscriber_fn:ident,
            $notify_fn:ident,
            $remove_fn:ident
        );+ $(;)?
    ) => {
        fn notify_class_available(kind: DeviceKind, id: ClassDeviceId) {
            match kind {
                $(
                    #[cfg(feature = $feature)]
                    $kind => $notify_fn(id),
                )+
                _ => {}
            }
        }

        fn remove_class_device(id: ClassDeviceId) {
            $(
                #[cfg(feature = $feature)]
                $remove_fn(id);
            )+
        }

        $(
            class_registry!(
                $feature,
                $ty,
                $kind,
                $registry,
                $registry_fn,
                $publish_fn,
                $devices_fn,
                $find_fn,
                $subscribe_fn,
                $removed_subscriber_fn,
                $notify_fn,
                $remove_fn
            );
        )+
    };
}

/// Concrete runtime type for network devices.
///
/// Drivers construct this via `Box::new(their_net_device)` and publish it
/// through [`publish_net`].
#[cfg(feature = "net")]
pub type NetDeviceImpl = Box<dyn NetDevice + Send + Sync>;
/// Concrete runtime type for registered generic disks.
#[cfg(feature = "block")]
pub type BlockDeviceImpl = block::GendiskRef;
/// Concrete runtime type for character devices.
#[cfg(feature = "char")]
pub type CharDeviceImpl = Box<dyn CharDevice + Send + Sync>;
/// Shared hardware entropy source, also retained by its host activation owner.
///
/// Publish only after the driver can receive completion IRQs and accept reads.
/// Registry removal does not stop the source or prove that DMA has stopped.
#[cfg(feature = "hwrng")]
pub type HwrngDeviceImpl = Arc<dyn HwrngDevice + Send + Sync>;
/// Concrete runtime type for display devices.
#[cfg(feature = "display")]
pub type DisplayDeviceImpl = Box<dyn DisplayDevice + Send + Sync>;
/// Concrete runtime type for input devices.
#[cfg(feature = "input")]
pub type InputDeviceImpl = Box<dyn InputDevice + Send + Sync>;
/// Concrete runtime type for VSOCK devices.
#[cfg(feature = "vsock")]
pub type VsockDeviceImpl = Box<dyn VsockDevice + Send + Sync>;
/// Concrete runtime type for VirtIO 9P devices.
#[cfg(feature = "virtio-9p")]
pub type Virtio9pDeviceImpl = Box<dyn Virtio9pDevice + Send + Sync>;

/// Re-export the generic disk type used by block-class publishers.
#[cfg(feature = "block")]
pub use block::Gendisk;
/// Re-export character device trait.
#[cfg(feature = "char")]
pub use char_driver::CharDevice;
/// Re-export display device trait and framebuffer metadata.
#[cfg(feature = "display")]
pub use display::{DisplayDevice, DisplayInfo};
/// Re-export the hardware entropy source contract.
#[cfg(feature = "hwrng")]
pub use hwrng::HwrngDevice;
/// Re-export input device trait, event types, and identity types.
#[cfg(feature = "input")]
pub use input::{Event, EventType, InputDevice, InputDeviceId, InputEventNotifier};
/// Re-export network device trait, buffer handle, and RX scheduler capability.
#[cfg(feature = "net")]
pub use net::{NetBufHandle, NetDevice, NetRxScheduler};
/// Re-export VirtIO 9P device trait.
#[cfg(feature = "virtio-9p")]
pub use virtio::Virtio9pDevice;
/// Re-export VSOCK device trait and address/connection types.
#[cfg(feature = "vsock")]
pub use vsock::{
    VsockAddr, VsockConnId, VsockConnectionInfo, VsockDevice, VsockTransportEvent,
    VsockTransportEventKind,
};

trait ClassRuntimeMetadata {
    fn class_metadata(&self) -> ClassDeviceMetadata {
        ClassDeviceMetadata::empty()
    }

    fn publish_class(&self) -> DriverResult<()> {
        Ok(())
    }

    fn unpublish_class(&self) {}

    fn device_node_class(&self) -> Option<NodeClass> {
        None
    }
}

#[cfg(feature = "block")]
impl ClassRuntimeMetadata for BlockDeviceImpl {
    fn publish_class(&self) -> DriverResult<()> {
        block::add_disk(self.clone()).map(drop)
    }

    fn unpublish_class(&self) {
        block::del_gendisk(self.device_number());
    }
}

#[cfg(feature = "char")]
impl ClassRuntimeMetadata for CharDeviceImpl {}

#[cfg(feature = "hwrng")]
impl ClassRuntimeMetadata for HwrngDeviceImpl {}

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

#[cfg(feature = "char")]
impl CharDevice for ClassDevice<CharDeviceImpl> {
    fn read(&self, buf: &mut [u8]) -> DriverResult<usize> {
        self.with(|device| device.read(buf))
    }

    fn write(&self, buf: &[u8]) -> DriverResult<usize> {
        self.with(|device| device.write(buf))
    }

    fn flush(&self) -> DriverResult {
        self.with(|device| device.flush())
    }
}

#[cfg(feature = "hwrng")]
impl HwrngDevice for ClassDevice<HwrngDeviceImpl> {
    fn read(&self, buf: &mut [u8]) -> DriverResult<usize> {
        self.with(|device| device.read(buf))
    }

    fn is_failed(&self) -> bool {
        self.with(|device| device.is_failed())
    }
}

#[cfg(feature = "display")]
impl DisplayDevice for ClassDevice<DisplayDeviceImpl> {
    fn info(&self) -> DisplayInfo {
        self.with(|device| device.info())
    }

    fn need_flush(&self) -> bool {
        self.with(|device| device.need_flush())
    }

    fn flush(&self) -> DriverResult {
        self.with(|device| device.flush())
    }

    fn create_scanout_resource(
        &self,
        resource: display::ScanoutResource,
        paddr: u64,
        length: u32,
    ) -> DriverResult {
        self.with(|device| device.create_scanout_resource(resource, paddr, length))
    }

    fn destroy_scanout_resource(&self, resource_id: u32) -> DriverResult {
        self.with(|device| device.destroy_scanout_resource(resource_id))
    }

    fn present_scanout_resource(
        &self,
        resource_id: u32,
        rect: display::ScanoutRect,
    ) -> DriverResult {
        self.with(|device| device.present_scanout_resource(resource_id, rect))
    }
}

#[cfg(feature = "input")]
impl InputDevice for ClassDevice<InputDeviceImpl> {
    fn device_id(&self) -> InputDeviceId {
        self.with(|device| device.device_id())
    }

    fn physical_location(&self) -> &str {
        self.input_physical_location()
    }

    fn unique_id(&self) -> &str {
        self.input_unique_id()
    }

    fn get_event_bits(&self, ty: EventType, out: &mut [u8]) -> DriverResult<bool> {
        self.with(|device| device.get_event_bits(ty, out))
    }

    fn read_event(&self) -> DriverResult<Event> {
        self.with(|device| device.read_event())
    }

    fn set_event_notifier(&self, notifier: Option<Arc<dyn InputEventNotifier>>) -> DriverResult {
        self.with(|device| device.set_event_notifier(notifier))
    }
}

#[cfg(feature = "net")]
impl NetDevice for ClassDevice<NetDeviceImpl> {
    fn mac(&self) -> net::MacAddress {
        self.with(|device| device.mac())
    }

    fn can_tx(&self) -> bool {
        self.with(|device| device.can_tx())
    }

    fn can_rx(&self) -> bool {
        self.with(|device| device.can_rx())
    }

    fn rx_queue_len(&self) -> usize {
        self.with(|device| device.rx_queue_len())
    }

    fn tx_queue_len(&self) -> usize {
        self.with(|device| device.tx_queue_len())
    }

    fn recycle_rx(&self, rx_buf: NetBufHandle) -> DriverResult {
        self.with(|device| device.recycle_rx(rx_buf))
    }

    fn recycle_tx(&self) -> DriverResult {
        self.with(|device| device.recycle_tx())
    }

    fn send(&self, tx_buf: NetBufHandle) -> DriverResult {
        self.with(|device| device.send(tx_buf))
    }

    fn recv(&self) -> DriverResult<NetBufHandle> {
        self.with(|device| device.recv())
    }

    fn alloc_tx_buf(&self, size: usize) -> DriverResult<NetBufHandle> {
        self.with(|device| device.alloc_tx_buf(size))
    }

    fn set_rx_scheduler(&self, scheduler: Option<Arc<dyn NetRxScheduler>>) -> DriverResult {
        self.with(|device| device.set_rx_scheduler(scheduler))
    }
}

#[cfg(feature = "vsock")]
impl VsockDevice for ClassDevice<VsockDeviceImpl> {
    fn guest_cid(&self) -> u64 {
        self.with(|device| device.guest_cid())
    }

    fn listen(&self, port: u32) {
        self.with(|device| device.listen(port));
    }

    fn unlisten(&self, port: u32) {
        self.with(|device| device.unlisten(port));
    }

    fn connect(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.with(|device| device.connect(info))
    }

    fn accept(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.with(|device| device.accept(info))
    }

    fn force_close(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.with(|device| device.force_close(info))
    }

    fn send(&self, info: &VsockConnectionInfo, buf: &[u8]) -> DriverResult<usize> {
        self.with(|device| device.send(info, buf))
    }

    fn shutdown(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.with(|device| device.shutdown(info))
    }

    fn credit_update(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.with(|device| device.credit_update(info))
    }

    fn poll_event(
        &self,
        handler: &mut dyn FnMut(VsockTransportEvent, &[u8]) -> DriverResult<()>,
    ) -> DriverResult<bool> {
        self.with(|device| device.poll_event(handler))
    }
}

#[cfg(feature = "virtio-9p")]
impl ClassDevice<Virtio9pDeviceImpl> {
    pub fn mount_tag(&self) -> String {
        self.with(|device| device.mount_tag())
    }
}

#[cfg(feature = "virtio-9p")]
impl Virtio9pDevice for ClassDevice<Virtio9pDeviceImpl> {
    fn mount_tag(&self) -> String {
        self.mount_tag()
    }

    fn request(&self, req: &[u8], resp: &mut [u8]) -> DriverResult<usize> {
        self.with(|device| device.request(req, resp))
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

mod node_compat {
    use alloc::{format, string::String, vec::Vec};

    use devnode::{DevNode, DevNodeType, DeviceLocation, bitmap_value};

    use super::InputNodeMetadata;

    static INPUT_TYPE: DevNodeType =
        DevNodeType::new("input", 13, 64, 32, |index| format!("input/event{index}"));
    static DRM_TYPE: DevNodeType =
        DevNodeType::new("drm", 226, 0, 64, |index| format!("dri/card{index}"));

    fn physical_path(location: DeviceLocation) -> String {
        match location {
            DeviceLocation::Pci {
                segment,
                bus,
                device,
                function,
            } => devnode::physical_path_pci(segment, bus, device, function),
            DeviceLocation::Mmio { base, .. } => format!("platform/{base:x}.mmio"),
            DeviceLocation::FirmwareNode { id } => format!("platform/firmware{id}"),
            DeviceLocation::PlatformStatic { id } => format!("platform/static{id}"),
            DeviceLocation::Bridge { domain } => format!("pci{domain:04x}:00"),
        }
    }

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
        location: DeviceLocation,
    ) -> DevNode {
        let physical = physical_path(location);
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
        devnode::publish(
            id,
            "input",
            INPUT_TYPE.devname(0),
            INPUT_TYPE.device_number(0),
            &format!("{physical}/input"),
            None,
            properties,
            Some(String::from(driver_name)),
        )
    }

    /// Build the drm node publication (cardN).
    pub(super) fn drm_node(id: u64, driver_name: &str, location: DeviceLocation) -> DevNode {
        let physical = physical_path(location);
        devnode::publish(
            id,
            "drm",
            DRM_TYPE.devname(0),
            DRM_TYPE.device_number(0),
            &physical,
            None,
            Vec::new(),
            Some(String::from(driver_name)),
        )
    }
}

class_registries! {
    "net",
    NetDeviceImpl,
    DeviceKind::Net,
    NET_DEVICES,
    net_class_registry,
    publish_net,
    net_devices,
    find_net_device,
    subscribe_net_available,
    subscribe_net_removed,
    notify_net_available,
    remove_net_device;
    "block",
    BlockDeviceImpl,
    DeviceKind::Block,
    BLOCK_DEVICES,
    block_class_registry,
    publish_block,
    block_devices,
    find_block_device,
    subscribe_block_available,
    subscribe_block_removed,
    notify_block_available,
    remove_block_device;
    "char",
    CharDeviceImpl,
    DeviceKind::Char,
    CHAR_DEVICES,
    char_class_registry,
    publish_char,
    char_devices,
    find_char_device,
    subscribe_char_available,
    subscribe_char_removed,
    notify_char_available,
    remove_char_device;
    "hwrng",
    HwrngDeviceImpl,
    DeviceKind::Hwrng,
    HWRNG_DEVICES,
    hwrng_class_registry,
    publish_hwrng,
    hwrng_devices,
    find_hwrng_device,
    subscribe_hwrng_available,
    notify_hwrng_available,
    remove_hwrng_device;
    "display",
    DisplayDeviceImpl,
    DeviceKind::Display,
    DISPLAY_DEVICES,
    display_class_registry,
    publish_display,
    display_devices,
    find_display_device,
    subscribe_display_available,
    subscribe_display_removed,
    notify_display_available,
    remove_display_device;
    "input",
    InputDeviceImpl,
    DeviceKind::Input,
    INPUT_DEVICES,
    input_class_registry,
    publish_input,
    input_devices,
    find_input_device,
    subscribe_input_available,
    subscribe_input_removed,
    notify_input_available,
    remove_input_device;
    "vsock",
    VsockDeviceImpl,
    DeviceKind::Vsock,
    VSOCK_DEVICES,
    vsock_class_registry,
    publish_vsock,
    vsock_devices,
    find_vsock_device,
    subscribe_vsock_available,
    subscribe_vsock_removed,
    notify_vsock_available,
    remove_vsock_device;
    "virtio-9p",
    Virtio9pDeviceImpl,
    DeviceKind::Fs9p,
    VIRTIO_9P_DEVICES,
    virtio_9p_class_registry,
    publish_virtio_9p,
    virtio_9p_devices,
    find_virtio_9p_device,
    subscribe_virtio_9p_available,
    subscribe_virtio_9p_removed,
    notify_virtio_9p_available,
    remove_virtio_9p_device;
}

/// Shared class prelude for runtime publishers and consumers.
pub mod prelude {
    #[cfg(feature = "block")]
    pub use block::BlockDeviceOperations;
    pub use driver_base::{Device, DeviceKind, DriverError, DriverResult};

    #[cfg(feature = "block")]
    pub use crate::BlockDeviceImpl;
    #[cfg(feature = "char")]
    pub use crate::{CharDevice, CharDeviceImpl, subscribe_char_removed};
    #[cfg(feature = "display")]
    pub use crate::{DisplayDevice, DisplayDeviceImpl, DisplayInfo};
    #[cfg(feature = "input")]
    pub use crate::{Event, EventType, InputDevice, InputDeviceId, InputDeviceImpl};
    #[cfg(feature = "hwrng")]
    pub use crate::{HwrngDevice, HwrngDeviceImpl};
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

/// Drop a detached (driver-core owned) char publication by its owning device
/// id. Called by the composition root's remove notifier.
#[cfg(feature = "char")]
pub fn remove_detached_char(owner: u64) -> Option<ClassDevice<CharDeviceImpl>> {
    char_class_registry().lock().remove_detached(owner)
}

/// Drop a detached (driver-core owned) net publication by its owning device id.
#[cfg(feature = "net")]
pub fn remove_detached_net(owner: u64) -> Option<ClassDevice<NetDeviceImpl>> {
    net_class_registry().lock().remove_detached(owner)
}

#[cfg(feature = "input")]
pub fn remove_detached_input(owner: u64) -> Option<ClassDevice<InputDeviceImpl>> {
    input_class_registry().lock().remove_detached(owner)
}

#[cfg(feature = "display")]
pub fn remove_detached_display(owner: u64) -> Option<ClassDevice<DisplayDeviceImpl>> {
    display_class_registry().lock().remove_detached(owner)
}

#[cfg(feature = "vsock")]
pub fn remove_detached_vsock(owner: u64) -> Option<ClassDevice<VsockDeviceImpl>> {
    vsock_class_registry().lock().remove_detached(owner)
}
