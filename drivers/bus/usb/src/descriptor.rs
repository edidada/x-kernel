// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! USB descriptor wire formats (USB 2.0 §9.6, USB 3.x §9.6).
//!
//! Everything here is little-endian on the wire, so the parsers read bytes
//! explicitly instead of casting the buffer to a `#[repr(C)]` struct: a
//! misaligned or truncated buffer must be an error, never a panic.

use alloc::vec::Vec;

use kerrno::{KError, KResult};

/// Descriptor type values (`bDescriptorType`).
pub mod ty {
    pub const DEVICE: u8 = 1;
    pub const CONFIG: u8 = 2;
    pub const STRING: u8 = 3;
    pub const INTERFACE: u8 = 4;
    pub const ENDPOINT: u8 = 5;
    pub const DEVICE_QUALIFIER: u8 = 6;
    pub const BOS: u8 = 15;
}

/// Standard request codes (`bRequest`).
pub mod request {
    pub const GET_STATUS: u8 = 0;
    pub const GET_DESCRIPTOR: u8 = 6;
    pub const SET_ADDRESS: u8 = 5;
    pub const SET_CONFIGURATION: u8 = 9;
}

/// Descriptor type, as a named value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescriptorType {
    Device,
    Config,
    String,
    Interface,
    Endpoint,
    Other(u8),
}

impl DescriptorType {
    pub const fn from_u8(value: u8) -> Self {
        match value {
            ty::DEVICE => Self::Device,
            ty::CONFIG => Self::Config,
            ty::STRING => Self::String,
            ty::INTERFACE => Self::Interface,
            ty::ENDPOINT => Self::Endpoint,
            other => Self::Other(other),
        }
    }
}

fn read_u16(buf: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([buf[offset], buf[offset + 1]])
}

/// Device descriptor (`tDevice`), 18 bytes.
#[derive(Clone, Copy, Debug)]
pub struct DeviceDescriptor {
    pub bcd_usb: u16,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub max_packet_size_0: u8,
    pub vendor: u16,
    pub product: u16,
    pub bcd_device: u16,
    pub manufacturer_index: u8,
    pub product_index: u8,
    pub serial_index: u8,
    pub num_configurations: u8,
}

impl DeviceDescriptor {
    pub const WIRE_LEN: usize = 18;

    /// Parses the first 18 bytes of a device descriptor.
    pub fn parse(buf: &[u8]) -> KResult<Self> {
        if buf.len() < Self::WIRE_LEN {
            return Err(KError::InvalidInput);
        }
        if buf[1] != ty::DEVICE {
            return Err(KError::InvalidInput);
        }
        Ok(Self {
            bcd_usb: read_u16(buf, 2),
            class: buf[4],
            subclass: buf[5],
            protocol: buf[6],
            max_packet_size_0: buf[7],
            vendor: read_u16(buf, 8),
            product: read_u16(buf, 10),
            bcd_device: read_u16(buf, 12),
            manufacturer_index: buf[14],
            product_index: buf[15],
            serial_index: buf[16],
            num_configurations: buf[17],
        })
    }
}

/// One endpoint of an interface.
#[derive(Clone, Copy, Debug)]
pub struct EndpointDescriptor {
    pub address: u8,
    pub attributes: u8,
    pub max_packet_size: u16,
    pub interval: u8,
}

impl EndpointDescriptor {
    /// Endpoint number, bits [3:0].
    pub const fn number(self) -> u8 {
        self.address & 0x0f
    }

    /// Direction: `true` for IN (device to host), bit 7.
    pub const fn is_in(self) -> bool {
        self.address & 0x80 != 0
    }

    /// Transfer type (`bmAttributes` bits [1:0]): 0 control, 1 isochronous,
    /// 2 bulk, 3 interrupt.
    pub const fn transfer_type(self) -> u8 {
        self.attributes & 0x03
    }
}

/// One interface of a configuration.
#[derive(Clone, Debug)]
pub struct InterfaceDescriptor {
    pub number: u8,
    pub alternate_setting: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub endpoints: Vec<EndpointDescriptor>,
}

/// A whole configuration: its descriptor header plus the interfaces under it.
#[derive(Clone, Debug)]
pub struct ConfigDescriptor {
    pub configuration_value: u8,
    pub attributes: u8,
    pub max_power: u8,
    pub total_length: u16,
    pub interfaces: Vec<InterfaceDescriptor>,
}

/// Parses a `GET_DESCRIPTOR(CONFIGURATION)` reply into every configuration it
/// carries (`wTotalLength` covers all of them, each with its own header).
pub fn parse_config_descriptors(buf: &[u8]) -> KResult<Vec<ConfigDescriptor>> {
    let mut configurations = Vec::new();
    let mut offset = 0;
    while offset + 2 <= buf.len() {
        let length = buf[offset] as usize;
        let kind = buf[offset + 1];
        if length < 2 || offset + length > buf.len() {
            return Err(KError::InvalidInput);
        }
        if kind != ty::CONFIG {
            // Interfaces and endpoints are consumed below; anything else in
            // between (class-specific descriptors) is skipped by length.
            offset += length;
            continue;
        }
        if length < 9 {
            return Err(KError::InvalidInput);
        }
        let mut configuration = ConfigDescriptor {
            total_length: read_u16(buf, offset + 2),
            interfaces: Vec::new(),
            configuration_value: buf[offset + 5],
            attributes: buf[offset + 7],
            max_power: buf[offset + 8],
        };
        // Walk this configuration's own descriptors.
        let mut cursor = offset + length;
        while cursor + 2 <= buf.len() && cursor < offset + configuration.total_length as usize {
            let item_len = buf[cursor] as usize;
            if item_len < 2 || cursor + item_len > buf.len() {
                break;
            }
            match buf[cursor + 1] {
                ty::INTERFACE if item_len >= 9 => {
                    configuration.interfaces.push(InterfaceDescriptor {
                        number: buf[cursor + 2],
                        alternate_setting: buf[cursor + 3],
                        endpoints: Vec::new(),
                        class: buf[cursor + 5],
                        subclass: buf[cursor + 6],
                        protocol: buf[cursor + 7],
                    });
                }
                ty::ENDPOINT if item_len >= 7 => {
                    if let Some(interface) = configuration.interfaces.last_mut() {
                        interface.endpoints.push(EndpointDescriptor {
                            address: buf[cursor + 2],
                            attributes: buf[cursor + 3],
                            max_packet_size: read_u16(buf, cursor + 4),
                            interval: buf[cursor + 6],
                        });
                    }
                }
                _ => {}
            }
            cursor += item_len;
        }
        offset += length.max(configuration.total_length as usize);
        configurations.push(configuration);
    }
    if configurations.is_empty() {
        return Err(KError::InvalidInput);
    }
    Ok(configurations)
}
