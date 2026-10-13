// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The consumer-facing trait surface: every class's device trait
//! implemented for `ClassDevice<T>` — admission-gated data-plane
//! forwarding (`with_available`/`is_available`) over the shared runtime.

use driver_base::{Device, DeviceKind};

use super::*;

// ---------------------------------------------------------------------------
// CharDevice impl for ClassDevice (console/serial consumers)
// ---------------------------------------------------------------------------

#[cfg(feature = "char")]
impl Device for ClassDevice<CharDeviceImpl> {
    fn name(&self) -> &str {
        self.name()
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }
}

// Legacy-contracts bridge (splits 4-6): a ClassDevice that forwards as a
// CharDevice must first be a Device. The kind is the static one the
// forwarding type already carries — the type system says it, no stored
// runtime tag. These impls retire with the contracts in split 7.

#[cfg(feature = "char")]
impl CharDevice for ClassDevice<CharDeviceImpl> {
    fn read(&self, buf: &mut [u8]) -> DriverResult<usize> {
        self.with_available(|device| device.read(buf))
    }

    fn write(&self, buf: &[u8]) -> DriverResult<usize> {
        self.with_available(|device| device.write(buf))
    }

    fn flush(&self) -> DriverResult {
        self.with_available(|device| device.flush())
    }
}

#[cfg(feature = "display")]
impl Device for ClassDevice<DisplayDeviceImpl> {
    fn name(&self) -> &str {
        self.name()
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Display
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
        self.with_available(|device| device.flush())
    }

    fn create_scanout_resource(
        &self,
        resource: display::ScanoutResource,
        paddr: u64,
        length: u32,
    ) -> DriverResult {
        self.with_available(|device| device.create_scanout_resource(resource, paddr, length))
    }

    fn destroy_scanout_resource(&self, resource_id: u32) -> DriverResult {
        self.with_available(|device| device.destroy_scanout_resource(resource_id))
    }

    fn present_scanout_resource(
        &self,
        resource_id: u32,
        rect: display::ScanoutRect,
    ) -> DriverResult {
        self.with_available(|device| device.present_scanout_resource(resource_id, rect))
    }
}

#[cfg(feature = "input")]
impl Device for ClassDevice<InputDeviceImpl> {
    fn name(&self) -> &str {
        self.name()
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Input
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
        self.with_available(|device| device.get_event_bits(ty, out))
    }

    fn read_event(&self) -> DriverResult<Event> {
        self.with_available(|device| device.read_event())
    }
}

#[cfg(feature = "net")]
impl Device for ClassDevice<NetDeviceImpl> {
    fn name(&self) -> &str {
        self.name()
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Net
    }
}

#[cfg(feature = "net")]
impl NetDevice for ClassDevice<NetDeviceImpl> {
    fn mac(&self) -> net::MacAddress {
        self.with(|device| device.mac())
    }

    fn can_tx(&self) -> bool {
        // Linux netif_tx_disable parity: after the offline flag flips the
        // queue reports stopped, so the network stack's poll loop stops
        // selecting this NIC instead of retrying `send` into `BadState`.
        self.is_available() && self.with(|device| device.can_tx())
    }

    fn can_rx(&self) -> bool {
        self.is_available() && self.with(|device| device.can_rx())
    }

    fn rx_queue_len(&self) -> usize {
        self.with(|device| device.rx_queue_len())
    }

    fn tx_queue_len(&self) -> usize {
        self.with(|device| device.tx_queue_len())
    }

    fn recycle_rx(&self, rx_buf: NetBufHandle) -> DriverResult {
        self.with_available(|device| device.recycle_rx(rx_buf))
    }

    fn recycle_tx(&self) -> DriverResult {
        self.with_available(|device| device.recycle_tx())
    }

    fn send(&self, tx_buf: NetBufHandle) -> DriverResult {
        self.with_available(|device| device.send(tx_buf))
    }

    fn recv(&self) -> DriverResult<NetBufHandle> {
        self.with_available(|device| device.recv())
    }

    fn alloc_tx_buf(&self, size: usize) -> DriverResult<NetBufHandle> {
        self.with_available(|device| device.alloc_tx_buf(size))
    }

    fn set_rx_scheduler(&self, scheduler: Option<Arc<dyn NetRxScheduler>>) -> DriverResult {
        self.with_available(|device| device.set_rx_scheduler(scheduler))
    }
}

#[cfg(feature = "vsock")]
impl Device for ClassDevice<VsockDeviceImpl> {
    fn name(&self) -> &str {
        self.name()
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Vsock
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
        self.with_available(|device| device.connect(info))
    }

    fn accept(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.with_available(|device| device.accept(info))
    }

    fn force_close(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.with_available(|device| device.force_close(info))
    }

    fn send(&self, info: &VsockConnectionInfo, buf: &[u8]) -> DriverResult<usize> {
        self.with_available(|device| device.send(info, buf))
    }

    fn shutdown(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.with_available(|device| device.shutdown(info))
    }

    fn credit_update(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.with_available(|device| device.credit_update(info))
    }

    fn poll_event(
        &self,
        handler: &mut dyn FnMut(VsockTransportEvent, &[u8]) -> DriverResult<()>,
    ) -> DriverResult<bool> {
        self.with_available(|device| device.poll_event(handler))
    }
}

#[cfg(feature = "virtio-9p")]
impl ClassDevice<Virtio9pDeviceImpl> {
    pub fn mount_tag(&self) -> String {
        self.with(|device| device.mount_tag())
    }
}

#[cfg(feature = "virtio-9p")]
impl Device for ClassDevice<Virtio9pDeviceImpl> {
    fn name(&self) -> &str {
        self.name()
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Fs9p
    }
}

#[cfg(feature = "virtio-9p")]
impl Virtio9pDevice for ClassDevice<Virtio9pDeviceImpl> {
    fn mount_tag(&self) -> String {
        self.mount_tag()
    }

    fn request(&self, req: &[u8], resp: &mut [u8]) -> DriverResult<usize> {
        self.with_available(|device| device.request(req, resp))
    }
}
