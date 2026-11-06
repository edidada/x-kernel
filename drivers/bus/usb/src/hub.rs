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
/// `USB_PORT_FEAT_LINK_STATE` (USB 3.x only): wIndex = (link state << 8) | port.
const FEAT_PORT_LINK_STATE: u16 = 5;
const FEAT_PORT_RESET: u16 = 4;
const FEAT_PORT_POWER: u16 = 8;
/// Warm reset of a SuperSpeed port (`USB_PORT_FEAT_BH_PORT_RESET`).
const FEAT_PORT_BH_RESET: u16 = 28;

/// `wPortStatus` bits (identical low half in both specs).
const PORT_ST_CONNECTION: u32 = 1 << 0;
const PORT_ST_ENABLE: u32 = 1 << 1;
const PORT_ST_LOW_SPEED: u32 = 1 << 9;
const PORT_ST_HIGH_SPEED: u32 = 1 << 10;
/// USB3 hubs report the link state in bits 8:5 (`USB_PORT_STAT_LINK_STATE`):
/// 0 = U0, 5 = Rx.Detect, 6 = SS.Inactive… There is no speed field on an SS
/// hub port status — speed comes from the hub itself, not this word.
const PORT_ST_SS_SHIFT: u32 = 5;
const PORT_ST_SS_MASK: u32 = 0xf;

/// `wPortChange` bits.
const PORT_CH_CONNECTION: u32 = 1 << 0;
const PORT_CH_RESET: u32 = 1 << 4;
/// SS-only change bits (`USB_PORT_STAT_C_BH_RESET`, `USB_PORT_STAT_C_LINK_STATE`).
const PORT_CH_BH_RESET: u32 = 1 << 5;
const PORT_CH_LINK_STATE: u32 = 1 << 6;

/// Port link states for `SetFeature(PORT_LINK_STATE)`, pre-shifted into
/// `wIndex`'s high byte (`USB_SS_PORT_LS_*` are `<< 5`, and
/// `hub_set_port_link_state()` ors `port | (link << 3)` on top).
pub const LINK_RX_DETECT: u8 = 5;
pub const LINK_SS_INACTIVE: u8 = 6;
pub const LINK_COMP_MOD: u8 = 7;
/// `USB_SS_PORT_LS_U3`: what `hub_port_disable()` sets on a SuperSpeed port.
pub const LINK_U3: u8 = 3;

/// How many times a port reset is followed before giving up.
const RESET_POLLS: usize = 100;
/// `PORT_RESET_TRIES` (`hub.c:2729`): how many times `hub_port_reset()` resets a
/// port that reports connected but never enabled.
const PORT_RESET_TRIES: usize = 5;
/// `HUB_LONG_RESET_TIME` (`hub.c:2734`): the delay between those resets.
const HUB_LONG_RESET_TIME_US: u64 = 200_000;

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
    // bPwrOn2PwrGood counts 2 ms units; `hub_power_on_good_delay()` makes an
    // external hub wait at least 100 ms for power to become stable.
    khal::time::busy_wait(ktime_types::TimeSpan::from_micros(
        (u64::from(info.power_on_2ms) * 2_000).max(100_000),
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
    let change = u32::from(buf[2]) | (u32::from(buf[3]) << 8);
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
fn clear_changes(hcd: &mut dyn Hcd, address: u8, port: u8, change: u32, info: HubInfo) {
    // USB 3.x hubs have three more latched change bits than USB 2.0 ones — the
    // reference's hub_port_reset() clears C_RESET, C_BH_PORT_RESET,
    // C_PORT_LINK_STATE and (for a device) C_CONNECTION. A port whose link-state
    // change is still unacknowledged reports a state the hub has already left.
    let ss = info.is_superspeed();
    for (bit, feature) in [
        (PORT_CH_CONNECTION, 16), // C_PORT_CONNECTION
        (PORT_CH_RESET, 20),      // C_PORT_RESET
        (PORT_CH_BH_RESET, 29),   // C_BH_PORT_RESET (SS only)
        (PORT_CH_LINK_STATE, 25), // C_PORT_LINK_STATE (SS only)
    ] {
        if feature >= 25 && !ss {
            continue;
        }
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
        clear_changes(hcd, address, port, change, info);
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
            clear_changes(hcd, address, port, change, info);
            let speed = speed_of(status, info);
            let link_state = (status >> PORT_ST_SS_SHIFT) & PORT_ST_SS_MASK;
            log::info!(
                "usb: hub {address} port {port} reset done, enabled {}, speed {}, status \
                 {status:#06x} (link state {link_state})",
                status & PORT_ST_ENABLE != 0,
                speed.as_str(),
            );
            // `hub_port_reset()`'s escalation: a port that migrates from reset
            // into SS.Inactive or Compliance Mode needs a warm (BH) reset — the
            // hot reset trained the link but did not reset the device.
            if info.is_superspeed()
                && (link_state == u32::from(LINK_SS_INACTIVE)
                    || link_state == u32::from(LINK_COMP_MOD))
            {
                log::warn!(
                    "usb: hub {address} port {port}: hot reset left link state {link_state}, warm \
                     reset as the reference does"
                );
                return reset_port_warm(hcd, address, port, info);
            }
            return (status & PORT_ST_ENABLE != 0).then_some(speed);
        }
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(10_000));
    }
    log::error!("usb: hub {address} port {port} never enabled after reset");
    None
}

/// Warm (BH) reset for a SuperSpeed port: `SetFeature(BH_PORT_RESET)` re-runs
/// the full LFPS reset sequence and puts the device back into the Default
/// state even when the hot reset only retrained the link. Completion is
/// observed as `C_BH_PORT_RESET` (`hub_port_reset()`'s `warm` branch).
pub fn reset_port_warm(hcd: &mut dyn Hcd, address: u8, port: u8, info: HubInfo) -> Option<Speed> {
    let ok = control_no_data(
        hcd,
        address,
        SetupPacket::new(
            RT_OUT_CLASS_OTHER,
            REQ_SET_FEATURE,
            FEAT_PORT_BH_RESET,
            u16::from(port),
            0,
        ),
    );
    if !ok {
        log::error!("usb: hub {address} port {port} BH_PORT_RESET rejected");
        return None;
    }
    // A warm reset re-runs link training; it takes longer than a hot reset.
    khal::time::busy_wait(ktime_types::TimeSpan::from_micros(100_000));
    for _ in 0..RESET_POLLS {
        let (status, change) = port_status(hcd, address, port)?;
        if change & PORT_CH_BH_RESET != 0 {
            clear_changes(hcd, address, port, change, info);
            let speed = speed_of(status, info);
            log::info!(
                "usb: hub {address} port {port} warm reset done, enabled {}, speed {}, status \
                 {status:#06x} (link state {})",
                status & PORT_ST_ENABLE != 0,
                speed.as_str(),
                (status >> PORT_ST_SS_SHIFT) & PORT_ST_SS_MASK,
            );
            return (status & PORT_ST_ENABLE != 0).then_some(speed);
        }
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(10_000));
    }
    log::error!("usb: hub {address} port {port} never enabled after warm reset");
    None
}

/// `hub_port_disable()` for a SuperSpeed port: drive the link to Rx.Detect.
/// The device sees the link go away and re-trains from scratch, so the next
/// reset+address cycle runs against a freshly detected device instead of one
/// stuck in a half-configured state. Returns once the port reports the device
/// again (`C_PORT_CONNECTION`) or the poll budget runs out.
pub fn redetect_link(hcd: &mut dyn Hcd, address: u8, port: u8) -> bool {
    let ok = control_no_data(
        hcd,
        address,
        SetupPacket::new(
            RT_OUT_CLASS_OTHER,
            REQ_SET_FEATURE,
            FEAT_PORT_LINK_STATE,
            (u16::from(LINK_RX_DETECT) << 8) | u16::from(port),
            0,
        ),
    );
    if !ok {
        log::error!("usb: hub {address} port {port} link state Rx.Detect rejected");
        return false;
    }
    log::info!("usb: hub {address} port {port}: link driven to Rx.Detect, waiting for reconnect");
    for _ in 0..200 {
        // A hub that stops answering GetPortStatus will not report a reconnect
        // either, and every failed control transfer costs its full timeout: one
        // dead hub turned this poll into tens of seconds of empty waiting.
        // `hub_port_wait_reset()` treats an unreadable status the same way.
        let Some((_, change)) = port_status(hcd, address, port) else {
            log::error!("usb: hub {address} port {port}: port status unreadable, giving up");
            return false;
        };
        if change & PORT_CH_CONNECTION != 0 {
            log::info!("usb: hub {address} port {port}: device reconnected");
            return true;
        }
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(10_000));
    }
    log::error!("usb: hub {address} port {port}: no reconnect after Rx.Detect");
    false
}

/// `hub_port_reset()`'s retry loop: reset, and if the port still reports
/// connected-but-not-enabled, reset again -- escalating to a warm (BH) reset for
/// a SuperSpeed port after the first failure, which is what the reference does
/// ("hot reset failed, warm reset", `hub.c:4863`).
///
/// A hub port reset is not a formality. The USB2 companion port of a SuperSpeed
/// stick on this board reports `connected true, enabled false` after one reset
/// and enables on a later one; giving up after the first attempt is how a present
/// device ends up with no descriptors read at all.
pub fn reset_port_retries(
    hcd: &mut dyn Hcd,
    address: u8,
    port: u8,
    info: HubInfo,
) -> Option<Speed> {
    let mut warm = false;
    for attempt in 0..PORT_RESET_TRIES {
        let result = if warm {
            reset_port_warm(hcd, address, port, info)
        } else {
            reset_port(hcd, address, port, info)
        };
        if result.is_some() {
            return result;
        }
        // `hub_port_reset()`: "If the port is in SS.Inactive or Compliance Mode,
        // the hot or warm reset failed. Try another warm reset."
        if info.is_superspeed() && !warm {
            warm = true;
        }
        log::warn!(
            "usb: hub {address} port {port}: not enabled, trying {}reset again ({}/{})",
            if warm { "warm " } else { "" },
            attempt + 1,
            PORT_RESET_TRIES,
        );
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(HUB_LONG_RESET_TIME_US));
    }
    log::error!("usb: hub {address} port {port}: cannot enable after {PORT_RESET_TRIES} resets");
    None
}

/// `hub_port_disable()` (`hub.c:4419`): take a port away from a device that
/// failed to enumerate, so the port does not stay claimed by a child that never
/// reached the Addressed state.
///
/// The two halves are different features, not one with a flag: a USB 2.0 port is
/// disabled by clearing `PORT_ENABLE`, while a SuperSpeed port has no such
/// feature and is disabled by driving the link to U3
/// (`hub_set_port_link_state(hub, port, USB_SS_PORT_LS_U3)`). Sending the USB 2.0
/// request to a SuperSpeed hub stalls.
pub fn port_disable(hcd: &mut dyn Hcd, address: u8, port: u8, is_superspeed: bool) -> bool {
    if is_superspeed {
        let ok = control_no_data(
            hcd,
            address,
            SetupPacket::new(
                RT_OUT_CLASS_OTHER,
                REQ_SET_FEATURE,
                FEAT_PORT_LINK_STATE,
                (u16::from(LINK_U3) << 8) | u16::from(port),
                0,
            ),
        );
        if !ok {
            log::error!("usb: hub {address} port {port}: U3 (disable) rejected");
        }
        ok
    } else {
        let ok = control_no_data(
            hcd,
            address,
            SetupPacket::new(
                RT_OUT_CLASS_OTHER,
                REQ_CLEAR_FEATURE,
                FEAT_PORT_ENABLE,
                u16::from(port),
                0,
            ),
        );
        if !ok {
            log::error!("usb: hub {address} port {port}: clear PORT_ENABLE rejected");
        }
        ok
    }
}

/// Whether the port-enable feature is exposed (used by the scan diagnostic).
pub const fn feature_port_enable() -> u16 {
    FEAT_PORT_ENABLE
}
