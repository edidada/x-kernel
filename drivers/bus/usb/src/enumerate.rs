// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Enumeration: the requests every USB device is asked, in the order the
//! specification requires (USB 2.0 §9.1.1.5 "Bus enumeration").
//!
//! The order is not a style choice: a device answers on address 0 with an
//! 8-byte packet size until `SET_ADDRESS`, and `SET_ADDRESS` takes effect only
//! after its status stage, so reading the full descriptor first would truncate.

use alloc::{
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};

use kerrno::{KError, KResult};
use log::info;

use crate::{
    ConfigDescriptor, DeviceDescriptor, Hcd, PortStatus, SetupPacket, Speed, UsbDevice, descriptor,
};

/// First address handed out by enumeration. Address 0 is the default pipe.
const FIRST_DEVICE_ADDRESS: u8 = 1;

/// Result of looking at the root hub once.
pub struct Enumeration {
    pub devices: Vec<UsbDevice>,
    /// Ports that reported a connection but could not be brought up.
    pub failed_ports: Vec<usize>,
}

/// Walks every root hub port, bringing up each connected device it finds.
///
/// Ports are re-read after each device so a change bit set during the walk is
/// not silently consumed by the next port's read.
pub fn enumerate_root_ports(hcd: &mut dyn Hcd) -> KResult<Enumeration> {
    let ports = hcd.port_count();
    let mut devices = Vec::new();
    let mut failed_ports = Vec::new();
    let mut next_address = FIRST_DEVICE_ADDRESS;

    for port in 0..ports {
        let status = hcd.port_status(port)?;
        if !status.connected {
            continue;
        }
        match enumerate_port(hcd, port, next_address) {
            Ok(device) => {
                next_address = next_address.saturating_add(1);
                devices.push(device);
            }
            Err(err) => {
                info!("usb: port {port} connected but enumeration failed: {err:?}");
                failed_ports.push(port);
            }
        }
    }

    Ok(Enumeration {
        devices,
        failed_ports,
    })
}

/// Brings up the device on one port: reset, address it, read its descriptors,
/// and configure it.
pub fn enumerate_port(hcd: &mut dyn Hcd, port: usize, address: u8) -> KResult<UsbDevice> {
    let PortStatus { connected, .. } = hcd.port_status(port)?;
    if !connected {
        return Err(KError::NotFound);
    }
    let speed = hcd.port_reset(port)?;
    info!(
        "usb: port {port} reset, speed {} (status said {})",
        speed.as_str(),
        speed.as_str(),
    );

    // A controller that assigns addresses itself (xHCI's `Address Device`) has
    // already put this device at a known address; there is no default-address
    // pipe to talk to afterwards, so start from that address and never issue
    // SET_ADDRESS. Host controllers whose address is a bus-level property
    // answer `None` and get the standard `address 0` phase below.
    let pre_addressed = hcd.port_address(port);
    let default_address = pre_addressed.unwrap_or(0);
    if let Some(address) = pre_addressed {
        info!("usb: port {port} already addressed as {address} by the controller");
    }

    // 1. Eight bytes of the device descriptor on the default pipe: enough for
    //    `bMaxPacketSize0`, which the rest of enumeration needs.
    let mut buf = [0u8; DeviceDescriptor::WIRE_LEN];
    // BRING-UP PROBE (length discrimination): the same GET_DESCRIPTOR through the
    // same pipe with three wLength values, *before* anything depends on the first
    // read succeeding. A device that answers one length and not another points at
    // the transfer's length handling; one that answers none points at the
    // transaction itself. Bounded to three requests, removed once the data-stage
    // failure is understood.
    let mut read = 0usize;
    for probe_len in [8usize, 18, 64] {
        let mut probe = [0u8; 64];
        let result = control_in(
            hcd,
            default_address,
            SetupPacket::get_descriptor(descriptor::ty::DEVICE, 0, probe_len as u16),
            &mut probe[..probe_len],
        );
        match result {
            Ok(got) => {
                info!("usb: port {port} probe wLength {probe_len} -> {got} bytes");
                if probe_len == 8 {
                    buf[..8].copy_from_slice(&probe[..8]);
                    read = got.min(8);
                }
            }
            Err(err) => log::error!("usb: port {port} probe wLength {probe_len} -> {err:?}"),
        }
    }
    if read == 0 {
        return Err(KError::NotFound);
    }
    info!("usb: port {port} first descriptor read {read} bytes");
    let short = DeviceDescriptor::parse(&zero_padded(&buf, read))?;
    info!(
        "usb: port {port} ep0 max packet {}",
        short.max_packet_size_0
    );

    // 2. Address it. From here on the device only answers to `address`.
    let address = match pre_addressed {
        Some(address) => address,
        None => {
            control_no_data(hcd, 0, SetupPacket::set_address(address))?;
            info!("usb: port {port} addressed as {address}");
            address
        }
    };

    // 3. The full device descriptor.
    let read = control_in(
        hcd,
        address,
        SetupPacket::get_descriptor(descriptor::ty::DEVICE, 0, DeviceDescriptor::WIRE_LEN as u16),
        &mut buf,
    )?;
    if read < DeviceDescriptor::WIRE_LEN {
        return Err(KError::InvalidInput);
    }
    let device = DeviceDescriptor::parse(&buf)?;
    info!(
        "usb: {address:03}:{port} id {:04x}:{:04x} usb {}.{} device {}.{} class \
         {:#04x}/{:02x}/{:02x} ep0 {} configs {}",
        device.vendor,
        device.product,
        (device.bcd_usb >> 8) & 0xff,
        (device.bcd_usb >> 4) & 0x0f,
        (device.bcd_device >> 8) & 0xff,
        (device.bcd_device >> 4) & 0x0f,
        device.class,
        device.subclass,
        device.protocol,
        device.max_packet_size_0,
        device.num_configurations,
    );

    let configurations = read_configurations(hcd, address)?;
    log_configurations(address, port, &configurations);

    // 5. Strings, best effort: a device may return none.
    let manufacturer = read_string(hcd, address, device.manufacturer_index);
    let product = read_string(hcd, address, device.product_index);
    let serial = read_string(hcd, address, device.serial_index);
    if let Some(product) = &product {
        info!("usb: {address:03}:{port} product {product}");
    }
    if let Some(manufacturer) = &manufacturer {
        info!("usb: {address:03}:{port} manufacturer {manufacturer}");
    }

    // 6. Put it in the first configuration, which is what every class driver
    //    assumes when it claims an interface.
    let first_configuration = configurations
        .first()
        .map(|configuration| configuration.configuration_value)
        .unwrap_or(0);
    if first_configuration != 0 {
        control_no_data(
            hcd,
            address,
            SetupPacket::set_configuration(first_configuration),
        )?;
        info!("usb: {address:03}:{port} configured with {first_configuration}");
    }

    Ok(UsbDevice {
        address,
        port,
        speed,
        device,
        configurations,
        manufacturer,
        product,
        serial,
    })
}

fn zero_padded(
    buf: &[u8; DeviceDescriptor::WIRE_LEN],
    read: usize,
) -> [u8; DeviceDescriptor::WIRE_LEN] {
    let mut padded = [0u8; DeviceDescriptor::WIRE_LEN];
    padded[..read.min(DeviceDescriptor::WIRE_LEN)]
        .copy_from_slice(&buf[..read.min(DeviceDescriptor::WIRE_LEN)]);
    padded
}

/// Reads and parses every configuration descriptor of a device.
///
/// Two requests, as the specification requires: the header carries
/// `wTotalLength`, which is what the full read asks for.
fn read_configurations(hcd: &mut dyn Hcd, address: u8) -> KResult<Vec<ConfigDescriptor>> {
    let mut header = [0u8; 9];
    control_in(
        hcd,
        address,
        SetupPacket::get_descriptor(descriptor::ty::CONFIG, 0, header.len() as u16),
        &mut header,
    )?;
    let total_length = u16::from_le_bytes([header[2], header[3]]) as usize;
    if total_length < header.len() || total_length > MAX_CONFIG_LENGTH {
        info!("usb: {address:03} implausible configuration length {total_length}");
        return Err(KError::InvalidInput);
    }
    let mut buffer = vec![0u8; total_length];
    let read = control_in(
        hcd,
        address,
        SetupPacket::get_descriptor(descriptor::ty::CONFIG, 0, total_length as u16),
        &mut buffer,
    )?;
    if read < header.len() {
        return Err(KError::InvalidInput);
    }
    descriptor::parse_config_descriptors(&buffer[..read])
}

/// Logs the configuration tree: what a class driver would claim.
fn log_configurations(address: u8, port: usize, configurations: &[ConfigDescriptor]) {
    for configuration in configurations {
        info!(
            "usb: {address:03}:{port} config {} total {} interfaces {} attributes {:#04x} power {}",
            configuration.configuration_value,
            configuration.total_length,
            configuration.interfaces.len(),
            configuration.attributes,
            configuration.max_power,
        );
        for interface in &configuration.interfaces {
            info!(
                "usb: {address:03}:{port}   interface {} alt {} class {:#04x}/{:02x}/{:02x} \
                 endpoints {}",
                interface.number,
                interface.alternate_setting,
                interface.class,
                interface.subclass,
                interface.protocol,
                interface.endpoints.len(),
            );
            for endpoint in &interface.endpoints {
                info!(
                    "usb: {address:03}:{port}     endpoint {:#04x} type {} max {} interval {}",
                    endpoint.address,
                    endpoint.transfer_type(),
                    endpoint.max_packet_size,
                    endpoint.interval,
                );
            }
        }
    }
}

/// Upper bound on `wTotalLength`: a malformed header must not make the driver
/// allocate its own way out of memory.
const MAX_CONFIG_LENGTH: usize = 4096;

/// Reads a string descriptor and converts the UTF-16LE payload to ASCII,
/// replacing everything outside ASCII with `?`.
pub fn read_string(hcd: &mut dyn Hcd, address: u8, index: u8) -> Option<String> {
    if index == 0 {
        return None;
    }
    let mut buf = [0u8; 255];
    let read = control_in(
        hcd,
        address,
        SetupPacket::get_descriptor(descriptor::ty::STRING, index, buf.len() as u16),
        &mut buf,
    )
    .ok()?;
    if read < 2 || buf[1] != descriptor::ty::STRING {
        return None;
    }
    let chars = (read - 2) / 2;
    let mut out = String::new();
    for char_index in 0..chars {
        let unit = u16::from_le_bytes([buf[2 + char_index * 2], buf[3 + char_index * 2]]);
        let decoded = char::from_u32(unit as u32).unwrap_or('?');
        out.push(if decoded.is_ascii() { decoded } else { '?' });
    }
    Some(out)
}

fn control_in(
    hcd: &mut dyn Hcd,
    address: u8,
    setup: SetupPacket,
    data: &mut [u8],
) -> KResult<usize> {
    hcd.control_transfer(address, &setup, data)
}

fn control_no_data(hcd: &mut dyn Hcd, address: u8, setup: SetupPacket) -> KResult<usize> {
    hcd.control_transfer(address, &setup, &mut [])
}

/// Enumerates a device the controller has already addressed.
///
/// This is the downstream half of [`enumerate_port`]: a device behind a hub is
/// reached by resetting the hub's port and letting the controller assign the
/// address, so the descriptor work starts at the device's own address with no
/// default-address phase and no root-port reset.
pub fn enumerate_addressed(
    hcd: &mut dyn Hcd,
    address: u8,
    port: usize,
    speed: Speed,
) -> KResult<UsbDevice> {
    let mut buf = [0u8; DeviceDescriptor::WIRE_LEN];
    let read = control_in(
        hcd,
        address,
        SetupPacket::get_descriptor(descriptor::ty::DEVICE, 0, DeviceDescriptor::WIRE_LEN as u16),
        &mut buf,
    )?;
    if read < DeviceDescriptor::WIRE_LEN {
        return Err(KError::InvalidInput);
    }
    let device = DeviceDescriptor::parse(&buf)?;
    info!(
        "usb: {address:03}:{port} id {:04x}:{:04x} usb {}.{} class {:#04x}/{:02x}/{:02x} ep0 {} \
         configs {}",
        device.vendor,
        device.product,
        (device.bcd_usb >> 8) & 0xff,
        (device.bcd_usb >> 4) & 0x0f,
        device.class,
        device.subclass,
        device.protocol,
        device.max_packet_size_0,
        device.num_configurations,
    );
    let configurations = read_configurations(hcd, address)?;
    let manufacturer = read_string(hcd, address, device.manufacturer_index);
    let product = read_string(hcd, address, device.product_index);
    let serial = read_string(hcd, address, device.serial_index);
    if let Some(product) = &product {
        info!("usb: {address:03}:{port} product {product}");
    }
    if let Some(manufacturer) = &manufacturer {
        info!("usb: {address:03}:{port} manufacturer {manufacturer}");
    }
    let first_configuration = configurations
        .first()
        .map(|configuration| configuration.configuration_value)
        .unwrap_or(0);
    if first_configuration != 0 {
        control_no_data(
            hcd,
            address,
            SetupPacket::set_configuration(first_configuration),
        )?;
        info!("usb: {address:03}:{port} configured with {first_configuration}");
    }
    Ok(UsbDevice {
        address,
        port,
        speed,
        device,
        configurations,
        manufacturer,
        product,
        serial,
    })
}

/// One-line description of a device, for drivers that log their claim.
pub fn describe(device: &UsbDevice) -> String {
    format!(
        "{:04x}:{:04x} at {} ({})",
        device.device.vendor,
        device.device.product,
        device.address,
        device.speed.as_str(),
    )
}

/// Convenience for class drivers: the first interface matching a class triple.
pub fn find_interface(
    configurations: &[ConfigDescriptor],
    class: u8,
    subclass: u8,
    protocol: u8,
) -> Option<&crate::InterfaceDescriptor> {
    configurations
        .iter()
        .flat_map(|configuration| configuration.interfaces.iter())
        .find(|interface| {
            interface.class == class
                && interface.subclass == subclass
                && interface.protocol == protocol
        })
}

impl core::fmt::Display for Speed {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<&str> for Speed {
    fn from(value: &str) -> Self {
        match value {
            "low" => Self::Low,
            "full" => Self::Full,
            "high" => Self::High,
            "super" => Self::Super,
            "super+" => Self::SuperPlus,
            _ => Self::Unknown,
        }
    }
}

impl UsbDevice {
    /// Human-readable identity, used by logs and by `usb` class drivers.
    pub fn summary(&self) -> String {
        let mut text = describe(self);
        if let Some(product) = &self.product {
            text.push_str(" \"");
            text.push_str(product);
            text.push('"');
        }
        text.to_string()
    }
}
