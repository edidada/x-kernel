// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! USB hub class (USB 2.0 §11.14 / USB 3.x §10.15), the subset a host needs to
//! reach the devices behind a hub.
//!
//! This board's USB3.0 connectors do not reach the controllers directly: each
//! one has a pair of Genesys Logic companion hubs (a USB2.1 one on the D+/D-
//! lines and a USB3.2 one on the SuperSpeed pairs), so every device the user
//! plugs in sits one tier below a hub. Until that tier is walked, a working
//! controller and a working SuperSpeed link still show nothing.
//!
//! Requests are class requests addressed to the hub's own control endpoint:
//! `GET_DESCRIPTOR(HUB)` for the descriptor, then per port
//! `SetPortFeature(PORT_POWER)` / `SetPortFeature(PORT_RESET)` with
//! `GetPortStatus` polling. The port status encodings differ between USB2 and
//! USB3 hubs (the speed field moved), so the hub's `bDeviceProtocol` selects
//! the decoder.

use crate::{Hcd, SetupPacket};

/// Class-specific request codes (`bRequest`).
const REQ_GET_STATUS: u8 = 0;
const REQ_CLEAR_FEATURE: u8 = 1;
const REQ_SET_FEATURE: u8 = 3;
const REQ_GET_DESCRIPTOR: u8 = 6;

/// `bmRequestType`: IN, class, device-to-host, recipient = **device**.
/// `GET_DESCRIPTOR(HUB)` is addressed to the hub device itself (USB 2.0
/// §11.21.1: 10100000b), unlike the per-port requests below.
const RT_IN_CLASS_DEVICE: u8 = 0xA0;
/// `bmRequestType`: IN, class, device-to-host, recipient = other (a port).
const RT_IN_CLASS_OTHER: u8 = 0xA3;
/// `bmRequestType`: OUT, class, host-to-device, recipient = other.
const RT_OUT_CLASS_OTHER: u8 = 0x23;

/// Port feature selectors (USB 2.0 §11.24.2, USB 3.x §10.16.1).
const FEAT_PORT_ENABLE: u16 = 1;
const FEAT_PORT_RESET: u16 = 4;
const FEAT_PORT_POWER: u16 = 8;

/// `wPortStatus` bits (identical low half in both specs).
const PORT_ST_CONNECTION: u32 = 1 << 0;
const PORT_ST_ENABLE: u32 = 1 << 1;
const PORT_ST_LOW_SPEED: u32 = 1 << 9;
const PORT_ST_HIGH_SPEED: u32 = 1 << 10;
/// USB3 hubs report the link speed in bits 6:3 instead, with 4 = SuperSpeed.
const PORT_ST_SS_SHIFT: u32 = 3;
const PORT_ST_SS_MASK: u32 = 0xf;

/// `wPortChange` bits.
const PORT_CH_CONNECTION: u32 = 1 << 0;
const PORT_CH_RESET: u32 = 1 << 4;

/// How many times a port reset is followed before giving up.
const RESET_POLLS: usize = 100;

/// What a hub told us about itself.
#[derive(Debug, Clone, Copy)]
pub struct HubInfo {
    /// `bNbrPorts`.
    pub ports: u8,
    /// `bDeviceProtocol`: 0 full/low-speed, 1 high-speed single-TT,
    /// 2 high-speed multi-TT, 3 SuperSpeed.
    pub protocol: u8,
    /// `bPwrOn2PwrGood`, in 2 ms units: how long a port needs after power-on.
    pub power_on_2ms: u8,
}

impl HubInfo {
    /// Whether the hub is a SuperSpeed hub (its port status has the SS layout).
    pub const fn is_superspeed(self) -> bool {
        self.protocol == 3
    }
}

/// One downstream port's state, decoded for logging and for the caller's
/// decision to enumerate what is attached.
#[derive(Debug, Clone, Copy)]
pub struct HubPort {
    /// 1-based port number, as the hub reports it.
    pub port: u8,
    pub connected: bool,
    pub enabled: bool,
    pub speed: Speed,
}

/// The speed a hub port reports for its attached device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speed {
    Low,
    Full,
    High,
    Super,
    Unknown,
}

impl Speed {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Full => "full",
            Self::High => "high",
            Self::Super => "super",
            Self::Unknown => "unknown",
        }
    }
}

fn control_in(
    hcd: &mut dyn Hcd,
    address: u8,
    setup: SetupPacket,
    data: &mut [u8],
) -> Option<usize> {
    match hcd.control_transfer(address, &setup, data) {
        Ok(read) => Some(read),
        Err(err) => {
            // SetupPacket is `repr(packed)`: copy the fields out instead of
            // taking references to them.
            let (request_type, request, value, index) =
                (setup.request_type, setup.request, setup.value, setup.index);
            log::error!(
                "usb: control IN {request_type:#04x}/{request:#04x} value {value:#06x} index \
                 {index:#06x} on {address} failed: {err:?}"
            );
            None
        }
    }
}

fn control_no_data(hcd: &mut dyn Hcd, address: u8, setup: SetupPacket) -> bool {
    hcd.control_transfer(address, &setup, &mut []).is_ok()
}

/// `GET_DESCRIPTOR(HUB)` — `bNbrPorts`, `bDeviceProtocol` and the power-on delay.
///
/// The descriptor is 9 bytes for USB 2.0 hubs and 12 for SuperSpeed ones; a
/// short buffer is fine because every field this driver needs lives in those
/// first bytes.
pub fn info(hcd: &mut dyn Hcd, address: u8, protocol: u8) -> Option<HubInfo> {
    // SuperSpeed hubs answer descriptor type 0x2A (`USB_DT_SS_HUB`); asking a
    // SuperSpeed hub for the USB 2.0 hub descriptor (0x29) gets a Stall, which
    // is exactly what this port did — and why the SuperSpeed hub's downstream
    // ports (where a USB3 stick appears) were never scanned at all.
    let descriptor_value: u16 = if protocol == 3 { 0x2a00 } else { 0x2900 };
    let mut buf = [0u8; 16];
    let read = control_in(
        hcd,
        address,
        SetupPacket::new(
            RT_IN_CLASS_DEVICE,
            REQ_GET_DESCRIPTOR,
            descriptor_value,
            0,
            16,
        ),
        &mut buf,
    )?;
    if read < 7 {
        log::error!("usb: hub {address} hub descriptor short ({read} bytes)");
        return None;
    }
    Some(HubInfo {
        ports: buf[2],
        // From the *device* descriptor (bDeviceProtocol), not the hub descriptor:
        // 0 FS, 1 HS single-TT, 2 HS multi-TT, 3 SuperSpeed.
        protocol,
        power_on_2ms: buf[5],
    })
}

/// `SetPortFeature(PORT_POWER)` for every port.
///
/// Ports come up unpowered by default (USB 2.0 §11.11): a device behind an
/// unpowered port is invisible no matter how well the controller works.
pub fn power_ports(hcd: &mut dyn Hcd, address: u8, info: HubInfo) {
    for port in 1..=u32::from(info.ports) {
        let ok = control_no_data(
            hcd,
            address,
            SetupPacket::new(
                RT_OUT_CLASS_OTHER,
                REQ_SET_FEATURE,
                FEAT_PORT_POWER,
                port as u16,
                0,
            ),
        );
        log::info!(
            "usb: hub {address} port {port} power {} ",
            if ok { "on" } else { "FAILED" }
        );
    }
    // bPwrOn2PwrGood counts 2 ms units; devices need it before they answer.
    khal::time::busy_wait(ktime_types::TimeSpan::from_micros(
        u64::from(info.power_on_2ms) * 2_000 + 10_000,
    ));
}

/// `GetPortStatus` on one port: `(wPortStatus, wPortChange)`.
pub fn port_status(hcd: &mut dyn Hcd, address: u8, port: u8) -> Option<(u32, u32)> {
    let mut buf = [0u8; 4];
    let read = control_in(
        hcd,
        address,
        SetupPacket::new(RT_IN_CLASS_OTHER, REQ_GET_STATUS, 0, u16::from(port), 4),
        &mut buf,
    )?;
    if read < 4 {
        return None;
    }
    let status = u32::from(buf[0]) | (u32::from(buf[1]) << 8);
    let change = u32::from(buf[2]) | (u32::from(buf[3]) << 16);
    Some((status, change))
}

fn speed_of(status: u32, info: HubInfo) -> Speed {
    if info.is_superspeed() {
        // Every downstream port of a SuperSpeed hub is SuperSpeed: the USB2 half
        // of the same physical connector is wired to a separate companion hub.
        // Decoding a speed field here is not just unnecessary, it is wrong — the
        // port status of an enabled SuperSpeed port reads 0x0203 on this board,
        // which a USB2-style decode turns into "low" and then addresses a 5 Gbps
        // device with low-speed signalling.
        let _ = status;
        return Speed::Super;
    }
    if status & PORT_ST_LOW_SPEED != 0 {
        Speed::Low
    } else if status & PORT_ST_HIGH_SPEED != 0 {
        Speed::High
    } else {
        Speed::Full
    }
}

/// Clears the change bits this driver consumed, so the next read is fresh.
fn clear_changes(hcd: &mut dyn Hcd, address: u8, port: u8, change: u32) {
    for (bit, feature) in [
        (PORT_CH_CONNECTION, 16), // C_PORT_CONNECTION
        (PORT_CH_RESET, 20),      // C_PORT_RESET
    ] {
        if change & bit != 0 {
            let _ = control_no_data(
                hcd,
                address,
                SetupPacket::new(
                    RT_OUT_CLASS_OTHER,
                    REQ_CLEAR_FEATURE,
                    feature,
                    u16::from(port),
                    0,
                ),
            );
        }
    }
}

/// Scans every downstream port of the hub at `address`.
pub fn scan(hcd: &mut dyn Hcd, address: u8, protocol: u8) -> alloc::vec::Vec<HubPort> {
    let Some(info) = info(hcd, address, protocol) else {
        log::error!("usb: hub {address} did not answer GET_DESCRIPTOR(HUB)");
        return alloc::vec::Vec::new();
    };
    log::info!(
        "usb: hub {address} reports {} port(s), protocol {} ({})",
        info.ports,
        info.protocol,
        if info.is_superspeed() {
            "SuperSpeed"
        } else {
            "USB2"
        },
    );
    power_ports(hcd, address, info);
    let mut ports = alloc::vec::Vec::new();
    for port in 1..=info.ports {
        let Some((status, change)) = port_status(hcd, address, port) else {
            log::warn!("usb: hub {address} port {port} status unreadable");
            continue;
        };
        clear_changes(hcd, address, port, change);
        let connected = status & PORT_ST_CONNECTION != 0;
        let enabled = status & PORT_ST_ENABLE != 0;
        let speed = speed_of(status, info);
        log::info!(
            "usb: hub {address} port {port}: connected {connected}, enabled {enabled}, speed {}, \
             status {status:#06x} change {change:#04x}",
            speed.as_str(),
        );
        ports.push(HubPort {
            port,
            connected,
            enabled,
            speed,
        });
    }
    ports
}

/// `SetPortFeature(PORT_RESET)` then poll until the port is enabled.
///
/// USB 2.0 §11.5.1.5: the host sets PORT_RESET, the hub holds it for 10 ms and
/// drives the enable only for a device it recognises, so "reset finished" is
/// observed as `C_PORT_RESET` together with `PORT_ENABLE` — not as a timeout.
pub fn reset_port(hcd: &mut dyn Hcd, address: u8, port: u8, info: HubInfo) -> Option<Speed> {
    // Always reset. `PORT_ENABLE` says the hub has a link, not that the device is
    // in the Default state a host needs it in: a SuperSpeed port can already be
    // enabled (the hub brought its link up on attach) while the device has never
    // been reset by us, and such a device ignores the SET_ADDRESS that Address
    // Device issues — measured as completion code 4 on exactly the port whose
    // status read `enabled true` before the reset.
    let ok = control_no_data(
        hcd,
        address,
        SetupPacket::new(
            RT_OUT_CLASS_OTHER,
            REQ_SET_FEATURE,
            FEAT_PORT_RESET,
            u16::from(port),
            0,
        ),
    );
    if !ok {
        log::error!("usb: hub {address} port {port} PORT_RESET rejected");
        return None;
    }
    // The hub needs a few ms to finish the reset before it clears the bit.
    khal::time::busy_wait(ktime_types::TimeSpan::from_micros(20_000));
    for _ in 0..RESET_POLLS {
        let (status, change) = port_status(hcd, address, port)?;
        // Wait for C_PORT_RESET specifically. `PORT_ENABLE` is already set on a
        // SuperSpeed port before the reset is requested (the hub brings its link
        // up on attach), so treating it as "reset done" returns immediately and
        // the Address Device that follows races a reset that is still running —
        // the device answers nothing and the command ends in a Transaction Error.
        if change & PORT_CH_RESET != 0 {
            clear_changes(hcd, address, port, change);
            let speed = speed_of(status, info);
            log::info!(
                "usb: hub {address} port {port} reset done, enabled {}, speed {}, status \
                 {status:#06x} (link state {})",
                status & PORT_ST_ENABLE != 0,
                speed.as_str(),
                (status >> 5) & 0xf,
            );
            return (status & PORT_ST_ENABLE != 0).then_some(speed);
        }
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(10_000));
    }
    log::error!("usb: hub {address} port {port} never enabled after reset");
    None
}

/// Whether the port-enable feature is exposed (used by the scan diagnostic).
pub const fn feature_port_enable() -> u16 {
    FEAT_PORT_ENABLE
}
