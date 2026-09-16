// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network control-plane coordination and legacy socket ioctl helpers.

use core::net::Ipv4Addr;

use kerrno::LinuxError;
use ksync::{Mutex, MutexGuard, static_lock};

use crate::{
    SERVICE,
    device::{
        ARPHRD_ETHER, ARPHRD_LOOPBACK, LINK_FLAG_UP, LinkConfigUpdate, LinkKind, NeighborUpdate,
        NeighborUpdatePolicy, interfaces::Ipv4AddrEntry,
    },
    ip::{IpAddress, IpCidr, Ipv4Address, Ipv4Cidr},
    network::state::NetworkState,
    router::{
        ROUTE_PROTOCOL_BOOT, ROUTE_SCOPE_LINK, ROUTE_SCOPE_UNIVERSE, ROUTE_TABLE_MAIN,
        ROUTE_TYPE_UNICAST, RouteAttrs, Rule,
    },
    service::Service,
};

static_lock! {
    static NETWORK_CONFIG_MUTATION_LOCK: Mutex<()> = Mutex::new(());
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExistingIpv4AddrAction {
    Keep,
    Reject,
}
/// Serializes ioctl, rtnetlink, and device removal through lookup and projection updates.
pub(crate) struct NetworkConfig<'a> {
    service: &'a Service,
    _guard: MutexGuard<'static, ()>,
}
impl<'a> NetworkConfig<'a> {
    /// Acquires the configuration lock in task context; the service must be initialized.
    /// Callers must hold neither a spinlock nor another configuration guard.
    pub(crate) fn lock(service: &'a Service) -> Self {
        Self {
            service,
            _guard: NETWORK_CONFIG_MUTATION_LOCK.lock(),
        }
    }

    fn device_index_by_name(&self, name: &str) -> Result<usize, LinuxError> {
        self.service
            .network()
            .device_index_by_name(name)
            .ok_or(LinuxError::ENODEV)
    }

    /// Copies the first matching route under the routing read lock without allocating a snapshot.
    /// The predicate must use only the supplied route and its captured request values.
    pub(crate) fn find_route(&self, matches_fn: impl FnMut(&Rule) -> bool) -> Option<Rule> {
        self.service.network().find_rule(matches_fn)
    }

    pub(crate) fn add_ipv4_addr(
        &self,
        entry: Ipv4AddrEntry,
        existing_action: ExistingIpv4AddrAction,
    ) -> Result<(), LinuxError> {
        let is_existing = self.service.network().has_exact_ipv4_addr(entry);
        if is_existing {
            return match existing_action {
                ExistingIpv4AddrAction::Keep => Ok(()),
                ExistingIpv4AddrAction::Reject => Err(LinuxError::EEXIST),
            };
        }
        self.update_ipv4_state(|state| state.add_ipv4_addr(entry))
    }

    /// Removes an address and reports whether its last local owner was removed.
    pub(crate) fn remove_ipv4_addr(&self, entry: Ipv4AddrEntry) -> Result<bool, LinuxError> {
        self.update_ipv4_state(|state| {
            if !state.remove_ipv4_addr(entry) {
                return Err(LinuxError::EADDRNOTAVAIL);
            }
            Ok(!state.has_ipv4_address(entry.addr.address()))
        })
    }

    pub(crate) fn set_primary_ipv4_addr(
        &self,
        dev: usize,
        addr: Ipv4Cidr,
    ) -> Result<(), LinuxError> {
        self.update_ipv4_state(|state| state.set_primary_ipv4_addr(dev, addr))
    }

    pub(crate) fn remove_primary_ipv4_addr(&self, dev: usize) -> Result<(), LinuxError> {
        self.update_ipv4_state(|state| state.remove_primary_ipv4_addr(dev))
    }

    pub(crate) fn add_route(&self, rule: Rule) -> Result<(), LinuxError> {
        self.service.network().add_route_rule(rule)
    }

    pub(crate) fn replace_route(
        &self,
        existing: Rule,
        replacement: Rule,
    ) -> Result<(), LinuxError> {
        self.service
            .network()
            .replace_route_rule(existing, replacement)
    }

    pub(crate) fn remove_route(&self, route: Rule) {
        self.service.network().remove_exact_route_rule(route);
    }

    pub(crate) fn set_ipv4_broadcast(
        &self,
        dev: usize,
        broadcast: Ipv4Address,
    ) -> Result<(), LinuxError> {
        self.service.network().set_ipv4_broadcast(dev, broadcast)
    }

    pub(crate) fn set_ipv4_netmask(&self, dev: usize, prefix_len: u8) -> Result<(), LinuxError> {
        self.update_ipv4_state(|state| state.set_ipv4_netmask(dev, prefix_len))
    }

    pub(crate) fn update_link(
        &self,
        ifindex: i32,
        update: LinkConfigUpdate,
    ) -> Result<(), LinuxError> {
        if self.service.network().update_device_link(ifindex, update)? {
            self.service.rebuild_stack();
        }
        Ok(())
    }

    /// Removes the device, repairs route indices and RX cursor, and synchronizes projections.
    pub(crate) fn remove_device(&self, id: kdevice::DeviceId) -> Option<()> {
        let previous_mtu = self.service.network().effective_mtu();
        let removed_index = self.service.network().remove_device(id)?;
        self.service
            .repair_rx_cursor_after_device_removed(removed_index);
        if self.service.network().effective_mtu() != previous_mtu {
            self.service.rebuild_stack();
        }
        let local_ipv4_addrs = self.service.network().local_ipv4_addrs();
        self.sync_local_ipv4_views(&local_ipv4_addrs);
        Some(())
    }

    pub(crate) fn update_neighbor(
        &self,
        update: NeighborUpdate,
        policy: NeighborUpdatePolicy,
    ) -> Result<(), LinuxError> {
        self.service
            .network()
            .apply_neighbor_update(update, policy)?;
        self.service.events.wake();
        Ok(())
    }

    fn update_ipv4_state<T>(
        &self,
        update_fn: impl FnOnce(&NetworkState) -> Result<T, LinuxError>,
    ) -> Result<T, LinuxError> {
        let result = update_fn(self.service.network())?;
        self.sync_local_ipv4_views(&self.service.network().local_ipv4_addrs());
        Ok(result)
    }

    fn sync_local_ipv4_views(&self, addrs: &[Ipv4Cidr]) {
        self.service.ipv4_ingress.set_ipv4_addrs(addrs);
        self.service.stack.set_ipv4_addrs(addrs);
    }

    fn ipv4_configured_route_rule(
        &self,
        dst: Option<Ipv4Addr>,
        dst_len: u8,
        gateway: Option<Ipv4Addr>,
        oif_name: &str,
    ) -> Result<Rule, LinuxError> {
        let dev = self.device_index_by_name(oif_name)?;
        let filter = ipv4_route_filter(dst, dst_len)?;
        let via = gateway.map(|addr| IpAddress::Ipv4(Ipv4Address::from_octets(addr.octets())));
        let source = self
            .service
            .network()
            .primary_ipv4_addr(dev)
            .map(|addr| IpAddress::Ipv4(addr.address()))
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
        .network()
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
/// Device resolution and address lookup copy a consistent interface snapshot.
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
        .network()
        .interface_ipv4_addr(name)
        .map(|cidr| Ipv4Addr::from(cidr.address().octets()))
}

/// Updates the primary IPv4 address on `name`.
///
/// Other IPv4 addresses assigned to the interface remain unchanged.
pub fn set_interface_ipv4_addr(
    name: &str,
    addr: Ipv4Addr,
    prefix_len: u8,
) -> Result<(), LinuxError> {
    let config = network_config()?;
    let dev = config.device_index_by_name(name)?;
    if addr.is_unspecified() {
        return config.remove_primary_ipv4_addr(dev);
    }
    let cidr = Ipv4Cidr::new(Ipv4Address::from_octets(addr.octets()), prefix_len)
        .ok_or(LinuxError::EINVAL)?;
    config.set_primary_ipv4_addr(dev, cidr)
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
    let config = network_config()?;
    let dev = config.device_index_by_name(name)?;
    config.set_ipv4_netmask(dev, prefix_len)
}

/// Sets the IPv4 broadcast address stored on the first address of `name`.
///
/// # Errors
///
/// Returns `ENODEV` when `name` is unknown, or `EADDRNOTAVAIL` when the
/// interface has no IPv4 address.
pub fn set_interface_ipv4_broadcast(name: &str, broadcast: Ipv4Addr) -> Result<(), LinuxError> {
    let config = network_config()?;
    let dev = config.device_index_by_name(name)?;
    config.set_ipv4_broadcast(dev, Ipv4Address::from_octets(broadcast.octets()))
}

/// Applies the `IFF_UP` state requested through `SIOCSIFFLAGS`.
///
/// # Errors
///
/// Returns `ENODEV` when `name` is unknown.
pub fn set_interface_flags(name: &str, flags: u16) -> Result<(), LinuxError> {
    let config = network_config()?;
    let ifindex = (config.device_index_by_name(name)? + 1) as i32;
    config.update_link(
        ifindex,
        LinkConfigUpdate {
            name: None,
            mtu: None,
            is_up: Some(flags as u32 & LINK_FLAG_UP != 0),
        },
    )
}

/// Adds an IPv4 route owned by `Router`.
///
/// Linux `SIOCADDRT` sets `NLM_F_CREATE` without `NLM_F_EXCL`, so a second
/// route to the same destination is rejected only when the nexthop also
/// matches (`oif` and gateway). A different gateway is a distinct route.
///
/// # Errors
///
/// Returns `ENODEV` when `oif_name` is unknown, `EINVAL` when the prefix or
/// gateway is illegal, `ENETUNREACH` when no output interface is supplied or
/// the gateway is outside its subnet, or `EEXIST` for a duplicate route.
pub fn add_ipv4_route(
    dst: Option<Ipv4Addr>,
    dst_len: u8,
    gateway: Option<Ipv4Addr>,
    oif_name: Option<&str>,
) -> Result<(), LinuxError> {
    let oif_name = oif_name.ok_or(LinuxError::ENETUNREACH)?;
    let config = network_config()?;
    let rule = config.ipv4_configured_route_rule(dst, dst_len, gateway, oif_name)?;
    if config
        .find_route(|existing| {
            existing.is_configured()
                && existing.filter == rule.filter
                && existing.via == rule.via
                && existing.dev == rule.dev
        })
        .is_some()
    {
        return Err(LinuxError::EEXIST);
    }
    config.add_route(rule)
}

/// Deletes an IPv4 route owned by `Router`. Missing entries return `ESRCH`.
///
/// A missing output interface and gateway are wildcards, matching Linux
/// `SIOCDELRT` / `fib_nh_match` when `rt_dev` or `RTF_GATEWAY` is omitted.
/// `route del default` therefore removes the first matching default route.
///
/// # Errors
///
/// Returns `ENODEV` when `oif_name` is unknown, `EINVAL` when the prefix is
/// illegal, or `ESRCH` when no matching configured route exists.
pub fn del_ipv4_route(
    dst: Option<Ipv4Addr>,
    dst_len: u8,
    gateway: Option<Ipv4Addr>,
    oif_name: Option<&str>,
) -> Result<(), LinuxError> {
    let config = network_config()?;
    let oif = match oif_name {
        Some(name) => Some(config.device_index_by_name(name)?),
        None => None,
    };
    let filter = ipv4_route_filter(dst, dst_len)?;
    let via = gateway.map(|addr| IpAddress::Ipv4(Ipv4Address::from_octets(addr.octets())));
    let existing = config
        .find_route(|existing| {
            existing.is_configured()
                && existing.filter == filter
                && oif.is_none_or(|oif| existing.dev == oif)
                && (via.is_none() || existing.via == via)
        })
        .ok_or(LinuxError::ESRCH)?;
    config.remove_route(existing);
    Ok(())
}

fn ipv4_route_filter(dst: Option<Ipv4Addr>, dst_len: u8) -> Result<IpCidr, LinuxError> {
    let dst_addr = dst
        .map(|addr| Ipv4Address::from_octets(addr.octets()))
        .unwrap_or(Ipv4Address::UNSPECIFIED);
    Ok(Ipv4Cidr::new(dst_addr, dst_len)
        .ok_or(LinuxError::EINVAL)?
        .network()
        .into())
}

/// Checks lazy initialization before borrowing the global Service.
fn network_config() -> Result<NetworkConfig<'static>, LinuxError> {
    if !SERVICE.is_inited() {
        return Err(LinuxError::ENODEV);
    }
    Ok(NetworkConfig::lock(&SERVICE))
}

#[cfg(unittest)]
mod tests {
    use alloc::boxed::Box;

    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::device::test_device::TestDevice;

    fn service_with_devices() -> Service {
        let state = NetworkState::new();
        state.add_device(Box::new(TestDevice::ethernet(1, 1500)));
        state.add_device(Box::new(TestDevice::ethernet(2, 1500)));
        Service::new(state)
    }

    #[def_test]
    fn address_transaction_preserves_views_until_last_owner_is_removed() {
        let service = service_with_devices();
        let config = NetworkConfig::lock(&service);
        let entry = Ipv4AddrEntry {
            dev: 0,
            addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 1), 24),
            scope: ROUTE_SCOPE_UNIVERSE,
            broadcast: Some(Ipv4Address::new(192, 0, 2, 254)),
        };
        let shared = Ipv4AddrEntry { dev: 1, ..entry };
        config
            .add_ipv4_addr(entry, ExistingIpv4AddrAction::Reject)
            .unwrap();
        config
            .add_ipv4_addr(shared, ExistingIpv4AddrAction::Reject)
            .unwrap();
        let views_before = service.stack.ipv4_addrs_for_tests();
        let addresses_before = service.network().ipv4_addr_entries();
        let changed_attributes = Ipv4AddrEntry {
            scope: ROUTE_SCOPE_LINK,
            broadcast: None,
            ..entry
        };

        config
            .add_ipv4_addr(changed_attributes, ExistingIpv4AddrAction::Keep)
            .unwrap();
        assert_eq!(
            config.add_ipv4_addr(entry, ExistingIpv4AddrAction::Reject),
            Err(LinuxError::EEXIST)
        );
        assert_eq!(config.set_ipv4_netmask(0, 33), Err(LinuxError::EINVAL));
        assert_eq!(service.network().ipv4_addr_entries(), addresses_before);
        assert_eq!(service.stack.ipv4_addrs_for_tests(), views_before);

        assert_eq!(config.remove_ipv4_addr(entry), Ok(false));
        let views_after = service.stack.ipv4_addrs_for_tests();
        assert_eq!(views_after.as_slice(), &views_before[..1]);
        assert_eq!(config.remove_ipv4_addr(shared), Ok(true));
        assert!(service.stack.ipv4_addrs_for_tests().is_empty());
        assert!(
            config
                .find_route(|rule| rule.src == IpAddress::Ipv4(entry.addr.address()))
                .is_none()
        );
        assert_eq!(
            config.remove_ipv4_addr(shared),
            Err(LinuxError::EADDRNOTAVAIL)
        );
    }
}
