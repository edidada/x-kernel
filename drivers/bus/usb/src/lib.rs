// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! USB host bus core.
//!
//! The bus owns everything that is the same for every host controller: the
//! descriptor wire formats, the transfer contract [`Hcd`] a controller
//! implements, and the enumeration state machine that turns a newly connected
//! port into a configured device.
//!
//! A controller driver supplies [`Hcd`]; it never decides what to ask a device.
//! Enumeration, address assignment and the descriptor requests live here, so a
//! second controller (EHCI/OHCI, or a virtual one) reuses them unchanged.

#![no_std]

extern crate alloc;

pub mod bot;
pub mod bot_fs;
pub mod bus;

#[cfg(unittest)]
unittest::path_mod_test!(tests_bus, "../ut-tests/bus.rs");
pub mod descriptor;
pub mod enumerate;
pub mod hub;

use alloc::{string::String, vec::Vec};

pub use descriptor::{
    ConfigDescriptor, DescriptorType, DeviceDescriptor, EndpointDescriptor, InterfaceDescriptor,
    parse_config_descriptors,
};
use kerrno::KResult;

/// Bus speed of a connected device, as reported by the host controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speed {
    Low,
    Full,
    High,
    Super,
    SuperPlus,
    Unknown,
}

impl Speed {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Full => "full",
            Self::High => "high",
            Self::Super => "super",
            Self::SuperPlus => "super+",
            Self::Unknown => "unknown",
        }
    }
}

/// Setup packet (USB 2.0 §9.3). Always little-endian on the wire.
#[derive(Clone, Copy, Debug)]
#[repr(C, packed)]
pub struct SetupPacket {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl SetupPacket {
    pub const fn new(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> Self {
        Self {
            request_type,
            request,
            value,
            index,
            length,
        }
    }

    /// A standard IN request to the device itself (`bmRequestType` 0x80).
    pub const fn get_descriptor(descriptor_type: u8, index: u8, length: u16) -> Self {
        Self::new(
            0x80,
            descriptor::request::GET_DESCRIPTOR,
            ((descriptor_type as u16) << 8) | index as u16,
            0,
            length,
        )
    }

    /// A standard OUT request to the device itself (`bmRequestType` 0x00).
    pub const fn set_address(address: u8) -> Self {
        Self::new(0x00, descriptor::request::SET_ADDRESS, address as u16, 0, 0)
    }

    /// A standard OUT request to the device itself (`bmRequestType` 0x00).
    pub const fn set_configuration(configuration: u8) -> Self {
        Self::new(
            0x00,
            descriptor::request::SET_CONFIGURATION,
            configuration as u16,
            0,
            0,
        )
    }
}

/// Port state as reported by the host controller's root hub.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortStatus {
    pub connected: bool,
    pub enabled: bool,
    pub speed: Speed,
    /// Set by the controller when the port changed state and the change has
    /// not been acknowledged yet.
    pub changed: bool,
}

/// The transfer contract between the bus core and a host controller.
///
/// Address 0 is the default control pipe the controller must provide before
/// any device has been addressed; every other address is one enumeration has
/// already assigned.
pub trait Hcd: Send + Sync {
    /// Number of root hub ports.
    fn port_count(&self) -> usize;

    /// Reads one root hub port without acknowledging its change bits.
    fn port_status(&self, port: usize) -> KResult<PortStatus>;

    /// Resets a port and returns the speed the device negotiated.
    fn port_reset(&mut self, port: usize) -> KResult<Speed>;

    /// The address the controller already gave this port's device, when the
    /// controller owns addressing.
    ///
    /// USB's default-address phase (`address 0` → `SET_ADDRESS`) is the bus
    /// core's job on host controllers whose address is a bus-level property
    /// (EHCI, UHCI). xHCI is different: `Address Device` makes the controller
    /// assign the address, and that address *is* the device slot id — there is
    /// no usable address 0 afterwards. A controller that has already addressed
    /// the port reports it here, and the bus core then starts from that address
    /// instead of telling the device to move to a new one.
    ///
    /// Default: `None` (the controller has not addressed anything; the bus core
    /// drives the default-address phase itself).
    fn port_address(&self, _port: usize) -> Option<u8> {
        None
    }

    /// One control transfer: `setup` stage, then `data` in the direction the
    /// setup packet asks for, then the status stage.
    ///
    /// Returns the number of data bytes actually transferred. A device that
    /// answers with fewer bytes than requested is not an error: descriptors are
    /// routinely read in two steps.
    fn control_transfer(
        &mut self,
        address: u8,
        setup: &SetupPacket,
        data: &mut [u8],
    ) -> KResult<usize>;

    /// One bulk transfer returning the number of bytes moved.
    fn bulk_transfer(
        &mut self,
        address: u8,
        endpoint: u8,
        data: &mut [u8],
        is_in: bool,
    ) -> KResult<usize>;

    /// `usb_hcd_reset_endpoint()`: client software cleared the endpoint's halt,
    /// so the controller's own copy of the data toggle has to return to DATA0.
    ///
    /// For bulk and interrupt endpoints the toggle lives in the *hardware*, not
    /// in the device record: `ehci_endpoint_reset()` spells this out -- "for Bulk
    /// and Interrupt endpoints we maintain the toggle state in the hardware; the
    /// toggle bits in udev aren't used at all. When an endpoint is reset by
    /// usb_clear_halt() we must reset the toggle bit in the QH."
    ///
    /// Skipping it is not a cosmetic matter: the device resets its toggle on
    /// `CLEAR_FEATURE(ENDPOINT_HALT)`, and if the host keeps the value the
    /// aborted transfer stopped at, every later packet is a toggle mismatch that
    /// the device discards without handshaking. On the host side that looks like
    /// a descriptor that stays Active in `hw_current` and never completes, and no
    /// amount of retrying helps.
    ///
    /// Default: the controller keeps no toggle state that software can reset.
    /// Tells the controller the EP0 max packet size the device's descriptor
    /// declared. `Address Device` can only carry a guess for it, and controllers
    /// that keep the guess (xHCI does) then split later control transfers into
    /// packets the device cannot receive (`xhci_check_maxpacket()`). Controllers
    /// whose EP0 size is not in a context ignore this.
    fn set_ep0_max_packet(&mut self, _address: u8, _max_packet: u16) {}

    fn clear_endpoint_halt(&mut self, address: u8, endpoint: u8, is_out: bool) {
        let _ = (address, endpoint, is_out);
    }

    /// The controller name used in logs.
    fn name(&self) -> &'static str;
}

/// A device that finished enumeration.
#[derive(Clone)]
pub struct UsbDevice {
    pub address: u8,
    pub port: usize,
    /// The device this one hangs off: `None` for a root-port device, `Some((hub
    /// address, hub port))` for a child. A port scan uses it to decide which
    /// devices a root-port change can have removed.
    pub parent: Option<(u8, u8)>,
    pub speed: Speed,
    pub device: DeviceDescriptor,
    pub configurations: Vec<ConfigDescriptor>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial: Option<String>,
}
