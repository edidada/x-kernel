// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network control-plane coordination and legacy socket ioctl helpers.

use alloc::vec::Vec;
use core::net::Ipv4Addr;

use kerrno::LinuxError;
use ksync::{Mutex, MutexGuard, static_lock};

use crate::{
    SERVICE,
    device::{ARPHRD_ETHER, ARPHRD_LOOPBACK, LINK_FLAG_UP, LinkConfigUpdate, LinkKind},
    ip::{IpAddress, IpCidr, Ipv4Address, Ipv4Cidr},
    router::{
        Ipv4AddrEntry, ROUTE_PROTOCOL_BOOT, ROUTE_SCOPE_LINK, ROUTE_SCOPE_UNIVERSE,
        ROUTE_TABLE_MAIN, ROUTE_TYPE_UNICAST, RouteAttrs, Rule,
    },
};

static_lock! {
    static NETWORK_CONFIG_MUTATION_LOCK: Mutex<()> = Mutex::new(());
}

/// Protects network configuration changes and device removal.
///
/// This lock may sleep and can only be used in task context.
/// Hold it while updating network objects to avoid conflicts between
/// socket ioctls, rtnetlink requests, and device removal.
pub(crate) fn network_config_lock() -> MutexGuard<'static, ()> {
    NETWORK_CONFIG_MUTATION_LOCK.lock()
}

/// Snapshot of interface state required by network socket ioctls.
#[derive(Clone, Debug)]
pub struct NetInterfaceInfo {
    /// 1-based interface index.
    pub ifindex: i32,
    /// Flags bitmask (`IFF_UP`, `IFF_BROADCAST`, ...).
    pub flags: u32,
    /// Hardware address (MAC for Ethernet).
    pub hardware_addr: [u8; 6],
    /// Device type (`ARPHRD_ETHER`, `ARPHRD_LOOPBACK`, ...).
    pub arphrd: u16,
}

/// Finds a network interface by name.
pub fn find_interface(name: &str) -> Option<NetInterfaceInfo> {
    if !SERVICE.is_inited() {
        return None;
    }
    let link = SERVICE
        .link_snapshots()
        .into_iter()
        .find(|link| link.name == name)?;
    let arphrd = match link.kind {
        LinkKind::Loopback => ARPHRD_LOOPBACK,
        LinkKind::Ethernet => ARPHRD_ETHER,
    };
    Some(NetInterfaceInfo {
        ifindex: link.ifindex,
        flags: link.flags,
        hardware_addr: link.hardware_addr,
        arphrd,
    })
}

/// Returns the primary IPv4 address of the specified interface.
///
/// Device resolution and address lookup run under a single Router lock.
///
/// # Errors
///
/// Returns `ENODEV` when `name` is unknown and `EADDRNOTAVAIL` when the
/// interface has no IPv4 address.
pub fn interface_ipv4_addr(name: &str) -> Result<Ipv4Addr, LinuxError> {
    if !SERVICE.is_inited() {
        return Err(LinuxError::ENODEV);
    }
    SERVICE
        .interface_ipv4_addr(name)
        .map(|cidr| Ipv4Addr::from(cidr.address().octets()))
}

/// Updates the primary IPv4 address on `name`.
///
/// Other IPv4 addresses assigned to the interface remain unchanged.
pub fn set_interface_ipv4_addr(
    name: &str,
    addr: Ipv4Addr,
    prefix_len: u8, // Helps routing determine which addresses are on the same subnet.
) -> Result<(), LinuxError> {
    let _network_config_guard = network_config_lock();
    let dev = device_index_by_name(name)?;
    if addr.is_unspecified() {
        return SERVICE.remove_primary_ipv4_addr(dev);
    }
    let cidr = Ipv4Cidr::new(Ipv4Address::from_octets(addr.octets()), prefix_len)
        .ok_or(LinuxError::EINVAL)?;
    SERVICE.set_primary_ipv4_addr(dev, cidr)
}

/// Updates the prefix length of the first IPv4 address assigned to `name`.
///
/// Other IPv4 addresses, the address scope, and a custom broadcast address
/// remain unchanged.
///
/// # Errors
///
/// Returns `ENODEV` when `name` is unknown, `EADDRNOTAVAIL` when the
/// interface has no IPv4 address, or `EINVAL` when `prefix_len` is invalid.
pub fn set_interface_ipv4_netmask(name: &str, prefix_len: u8) -> Result<(), LinuxError> {
    let _network_config_guard = network_config_lock();
    let dev = device_index_by_name(name)?;
    SERVICE.set_ipv4_netmask(dev, prefix_len)
}

/// Sets the IPv4 broadcast address stored on the first address of `name`.
///
/// # Errors
///
/// Returns `ENODEV` when `name` is unknown, or `EADDRNOTAVAIL` when the
/// interface has no IPv4 address.
pub fn set_interface_ipv4_broadcast(name: &str, broadcast: Ipv4Addr) -> Result<(), LinuxError> {
    let _network_config_guard = network_config_lock();
    let dev = device_index_by_name(name)?;
    SERVICE.set_ipv4_broadcast(dev, Ipv4Address::from_octets(broadcast.octets()))
}

/// Applies the `IFF_UP` state requested through `SIOCSIFFLAGS`.
///
/// # Errors
///
/// Returns `ENODEV` when `name` is unknown.
pub fn set_interface_flags(name: &str, flags: u16) -> Result<(), LinuxError> {
    let _network_config_guard = network_config_lock();
    let ifindex = (device_index_by_name(name)? + 1) as i32;
    SERVICE.update_device_link(
        ifindex,
        LinkConfigUpdate {
            name: None,
            mtu: None,
            is_up: Some(flags as u32 & LINK_FLAG_UP != 0),
        },
    )
}

/// Adds an IPv4 route to `Router`.
///
/// Routes with the same destination are allowed if they use a different
/// gateway or output interface.
///
/// # Errors
///
/// Returns `ENODEV` for an unknown interface, `EINVAL` for invalid values,
/// `ENETUNREACH` when the gateway cannot be reached, or `EEXIST` for a duplicate route.
pub fn add_ipv4_route(
    dst: Option<Ipv4Addr>,
    dst_len: u8,
    gateway: Option<Ipv4Addr>,
    oif_name: Option<&str>,
) -> Result<(), LinuxError> {
    let _network_config_guard = network_config_lock();
    let rule = ipv4_configured_route_rule(dst, dst_len, gateway, oif_name)?;
    if SERVICE.route_snapshot().iter().any(|existing| {
        existing.is_configured()
            && existing.filter == rule.filter
            && existing.via == rule.via
            && existing.dev == rule.dev
    }) {
        return Err(LinuxError::EEXIST);
    }
    SERVICE.add_route_rule(rule)
}

/// Deletes an IPv4 route from `Router`.
///
/// If the gateway or output interface is not given, any value can match.
///
/// # Errors
///
/// Returns `ENODEV` for an unknown interface, `EINVAL` for an invalid prefix,
/// or `ESRCH` when no matching route is found.
pub fn del_ipv4_route(
    dst: Option<Ipv4Addr>,
    dst_len: u8,
    gateway: Option<Ipv4Addr>,
    oif_name: Option<&str>,
) -> Result<(), LinuxError> {
    let _network_config_guard = network_config_lock();
    let oif = match oif_name {
        Some(name) => Some(device_index_by_name(name)?),
        None => None,
    };
    let filter = ipv4_route_filter(dst, dst_len)?;
    let via = gateway.map(|addr| IpAddress::Ipv4(Ipv4Address::from_octets(addr.octets())));
    let existing = SERVICE
        .route_snapshot()
        .into_iter()
        .find(|existing| {
            existing.is_configured()
                && existing.filter == filter
                && oif.is_none_or(|oif| existing.dev == oif)
                && (via.is_none() || existing.via == via)
        })
        .ok_or(LinuxError::ESRCH)?;
    SERVICE.remove_route_rule(existing);
    Ok(())
}

/// Converts the destination IP and prefix length into a CIDR network,
/// e.g. `192.168.1.123/24` becomes `192.168.1.0/24`.
fn ipv4_route_filter(dst: Option<Ipv4Addr>, dst_len: u8) -> Result<IpCidr, LinuxError> {
    let dst_addr = dst
        .map(|addr| Ipv4Address::from_octets(addr.octets()))
        .unwrap_or(Ipv4Address::UNSPECIFIED);
    Ok(Ipv4Cidr::new(dst_addr, dst_len)
        .ok_or(LinuxError::EINVAL)?
        .network()
        .into())
}

/// Creates a `Rule` for a configured IPv4 route.
// For example, the following route rule:
// filter = 192.168.2.0/24   // destination IP -> CIDR network
// via    = 192.168.1.1      // gateway IP
// dev    = eth0             // output interface
// source = 192.168.1.10     // source IP (first address of the output interface)
//                           // falling back to the route destination address
fn ipv4_configured_route_rule(
    dst: Option<Ipv4Addr>,
    dst_len: u8,
    gateway: Option<Ipv4Addr>,
    oif_name: Option<&str>,
) -> Result<Rule, LinuxError> {
    let dev = device_index_by_name(oif_name.ok_or(LinuxError::ENETUNREACH)?)?;
    let filter = ipv4_route_filter(dst, dst_len)?;
    let via = gateway.map(|addr| IpAddress::Ipv4(Ipv4Address::from_octets(addr.octets())));
    let source = ipv4_entries_for_device(dev)
        .first()
        .map(|entry| IpAddress::Ipv4(entry.addr.address()))
        .unwrap_or(filter.address());
    Ok(Rule::with_route_attrs(
        filter,
        via,
        dev,
        source,
        RouteAttrs {
            table: ROUTE_TABLE_MAIN,
            protocol: ROUTE_PROTOCOL_BOOT,
            scope: if via.is_some() {
                ROUTE_SCOPE_UNIVERSE
            } else {
                ROUTE_SCOPE_LINK
            },
            route_type: ROUTE_TYPE_UNICAST,
            prefsrc: None,
        },
    ))
}

fn device_index_by_name(name: &str) -> Result<usize, LinuxError> {
    if !SERVICE.is_inited() {
        return Err(LinuxError::ENODEV);
    }
    SERVICE.device_index_by_name(name).ok_or(LinuxError::ENODEV)
}

/// Returns all IPv4 addresses assigned to the given device.
// Get all IPv4 address snapshots
//        ↓
// Convert them into an iterator
//        ↓
// Filter addresses belonging to the specified device
//        ↓
// Extract the entry from each snapshot
//        ↓
// Collect them into a Vec<Ipv4AddrEntry>
fn ipv4_entries_for_device(dev: usize) -> Vec<Ipv4AddrEntry> {
    SERVICE
        .ipv4_addr_snapshots()
        .into_iter()
        .filter(|snapshot| snapshot.entry.dev == dev)
        .map(|snapshot| snapshot.entry)
        .collect()
}
