// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network inventory and routing under independent read-mostly locks.
//!
//! Lock order when a path needs both: `interfaces` then `routing`.
//! Callers that need both must not acquire `routing` first. Packet I/O
//! copies a [`ResolvedRoute`] and releases these locks before queue or
//! device work.
use alloc::{boxed::Box, vec::Vec};

use kerrno::{KError, KResult, LinuxError};
use ksync::{RwLock, RwLockWriteGuard};

use crate::{
    device::{
        LinkConfigUpdate, LinkKind, LinkSendSnapshot, LinkSnapshot, NeighborUpdate,
        NeighborUpdatePolicy, NetDevice,
        interfaces::{InterfaceSet, Ipv4AddrEntry},
    },
    ip::{IpAddress, Ipv4Address, Ipv4Cidr},
    network::router::{Router, Rule},
};

/// Copied forwarding decision; the routing and interface read locks are
/// already released when this value is returned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedRoute {
    pub device: usize,
    device_id: Option<kdevice::DeviceId>,
    pub next_hop: IpAddress,
    /// Egress-device MTU copied at resolve time. TX fragmentation uses this
    /// instead of [`InterfaceSet::effective_mtu`] or a later inventory lookup.
    pub mtu: usize,
    pub source: IpAddress,
    pub is_loopback: bool,
}

/// Read-mostly inventory and route table. Configuration writers take the
/// corresponding write side; the data path copies snapshots and drops the
/// read guards before protocol or queue work.
pub(crate) struct NetworkState {
    interfaces: RwLock<InterfaceSet>,
    routing: RwLock<Router>,
}

impl NetworkState {
    pub(crate) fn new() -> Self {
        Self {
            interfaces: RwLock::new(InterfaceSet::new()),
            routing: RwLock::new(Router::new()),
        }
    }

    pub(crate) fn with_interfaces<R>(&self, use_fn: impl FnOnce(&InterfaceSet) -> R) -> R {
        use_fn(&self.interfaces.read())
    }

    pub(crate) fn with_interfaces_mut<R>(&self, use_fn: impl FnOnce(&mut InterfaceSet) -> R) -> R {
        use_fn(&mut self.interfaces.write())
    }

    /// Nonblocking interface guard for poller device RX and broadcast TX.
    /// RX callers that also take `NetIo` must acquire this guard first.
    pub(crate) fn try_write_interfaces(&self) -> Option<RwLockWriteGuard<'_, InterfaceSet>> {
        self.interfaces.try_write()
    }

    /// Runs `use_fn` with both write guards. Acquires `interfaces` then `routing`.
    fn with_inventory_mut<R>(&self, use_fn: impl FnOnce(&mut InterfaceSet, &mut Router) -> R) -> R {
        let mut interfaces = self.interfaces.write();
        let mut routing = self.routing.write();
        use_fn(&mut interfaces, &mut routing)
    }

    fn with_inventory<R>(&self, use_fn: impl FnOnce(&InterfaceSet, &Router) -> R) -> R {
        let interfaces = self.interfaces.read();
        let routing = self.routing.read();
        use_fn(&interfaces, &routing)
    }

    #[cfg(unittest)]
    pub(crate) fn replace_for_tests(&self, other: NetworkState) {
        self.with_inventory_mut(|interfaces, routing| {
            core::mem::swap(interfaces, &mut other.interfaces.write());
            core::mem::swap(routing, &mut other.routing.write());
        });
    }

    pub(crate) fn add_device(&self, device: Box<dyn NetDevice>) -> usize {
        self.with_interfaces_mut(|interfaces| interfaces.add_device(device))
    }

    /// Looks up the forwarding path and copies the fields later stages need.
    #[cfg(unittest)]
    pub(crate) fn resolve(&self, dst: &IpAddress) -> Option<ResolvedRoute> {
        self.with_inventory(|interfaces, routing| resolve_route(interfaces, routing, dst))
    }

    /// Returns `None` on lock contention, or the copied lookup result otherwise.
    pub(crate) fn try_resolve(&self, dst: &IpAddress) -> Option<Option<ResolvedRoute>> {
        let interfaces = self.interfaces.try_read()?;
        let routing = self.routing.try_read()?;
        Some(resolve_route(&interfaces, &routing, dst))
    }

    /// Copies an interface query without waiting for a configuration writer.
    pub(crate) fn try_with_interfaces<R>(
        &self,
        use_fn: impl FnOnce(&InterfaceSet) -> R,
    ) -> Option<R> {
        let interfaces = self.interfaces.try_read()?;
        Some(use_fn(&interfaces))
    }

    pub(crate) fn resolve_output(&self, dst: &IpAddress, oif: i32) -> KResult<ResolvedRoute> {
        self.with_inventory(|interfaces, routing| {
            let source = routing.output_route_source(interfaces, dst, oif)?;
            if dst.is_broadcast() && oif > 0 {
                let device_index = (oif - 1) as usize;
                let device = interfaces
                    .devices()
                    .get(device_index)
                    .ok_or(KError::from(LinuxError::ENETUNREACH))?;
                return Ok(ResolvedRoute {
                    device: device_index,
                    device_id: device.device_id(),
                    next_hop: *dst,
                    mtu: device.mtu().min(u16::MAX as usize),
                    source,
                    is_loopback: device.link_kind() == LinkKind::Loopback,
                });
            }
            let rule = routing
                .lookup(dst)
                .ok_or(KError::from(LinuxError::ENETUNREACH))?;
            let device = interfaces
                .devices()
                .get(rule.dev)
                .ok_or(KError::from(LinuxError::ENETUNREACH))?;
            Ok(ResolvedRoute {
                device: rule.dev,
                device_id: device.device_id(),
                next_hop: rule.via.unwrap_or(*dst),
                mtu: device.mtu().min(u16::MAX as usize),
                source,
                is_loopback: device.link_kind() == LinkKind::Loopback,
            })
        })
    }

    pub(crate) fn select_source_address(&self, dst: &IpAddress) -> KResult<IpAddress> {
        self.with_inventory(|interfaces, routing| routing.output_route_source(interfaces, dst, 0))
    }

    #[cfg(unittest)]
    pub(crate) fn route_mtu(&self, dst: &IpAddress) -> Option<usize> {
        self.resolve(dst).map(|route| route.mtu)
    }

    pub(crate) fn ipv4_dest_requires_broadcast(&self, address: &IpAddress) -> bool {
        self.with_interfaces(|interfaces| interfaces.ipv4_dest_requires_broadcast(address))
    }

    pub(crate) fn has_exact_ipv4_addr(&self, entry: Ipv4AddrEntry) -> bool {
        self.with_interfaces(|interfaces| {
            interfaces
                .ipv4_addr_entries()
                .iter()
                .any(|existing| existing.dev == entry.dev && existing.addr == entry.addr)
        })
    }

    pub(crate) fn has_ipv4_address(&self, address: Ipv4Address) -> bool {
        self.with_interfaces(|interfaces| interfaces.has_ipv4_address(address))
    }

    pub(crate) fn effective_mtu(&self) -> usize {
        self.with_interfaces(|interfaces| interfaces.effective_mtu())
    }

    pub(crate) fn local_ipv4_addrs(&self) -> Vec<Ipv4Cidr> {
        self.with_interfaces(|interfaces| interfaces.local_ipv4_addrs())
    }

    /// Returns the primary address for a zero-based device slot without allocating a snapshot.
    pub(crate) fn primary_ipv4_addr(&self, dev: usize) -> Option<Ipv4Cidr> {
        self.with_interfaces(|interfaces| {
            interfaces
                .ipv4_addr_entries()
                .iter()
                .find(|entry| entry.dev == dev)
                .map(|entry| entry.addr)
        })
    }

    pub(crate) fn find_rule(&self, matches_fn: impl FnMut(&Rule) -> bool) -> Option<Rule> {
        self.routing.read().find_rule(matches_fn)
    }

    pub(crate) fn add_rule(&self, rule: Rule) {
        self.routing.write().add_rule(rule);
    }

    pub(crate) fn add_route_rule(&self, rule: Rule) -> Result<(), LinuxError> {
        let interfaces = self.interfaces.read();
        self.routing.write().add_route_rule(&interfaces, rule)
    }

    pub(crate) fn replace_route_rule(
        &self,
        existing: Rule,
        replacement: Rule,
    ) -> Result<(), LinuxError> {
        let interfaces = self.interfaces.read();
        self.routing
            .write()
            .replace_route_rule(&interfaces, existing, replacement)
    }

    pub(crate) fn remove_exact_route_rule(&self, route: Rule) {
        self.routing.write().remove_exact_route_rule(route);
    }

    pub(crate) fn set_ipv4_broadcast(
        &self,
        dev: usize,
        broadcast: Ipv4Address,
    ) -> Result<(), LinuxError> {
        self.with_interfaces_mut(|interfaces| interfaces.set_ipv4_broadcast(dev, broadcast))
    }

    pub(crate) fn update_device_link(
        &self,
        ifindex: i32,
        update: LinkConfigUpdate,
    ) -> Result<bool, LinuxError> {
        self.with_interfaces_mut(|interfaces| interfaces.update_device_link(ifindex, update))
    }

    pub(crate) fn apply_neighbor_update(
        &self,
        update: NeighborUpdate,
        policy: NeighborUpdatePolicy,
    ) -> Result<(), LinuxError> {
        self.with_interfaces_mut(|interfaces| interfaces.apply_neighbor_update(update, policy))
    }

    pub(crate) fn remove_device(&self, id: kdevice::DeviceId) -> Option<usize> {
        self.with_inventory_mut(|interfaces, routing| {
            let previous_addrs = interfaces.local_ipv4_addrs();
            let index = interfaces.remove_device_by_model_id(id)?;
            routing.remove_device_routes(index);
            refresh_address_routes(interfaces, routing, &previous_addrs);
            Some(index)
        })
    }

    fn update_primary_address<T>(
        &self,
        dev: usize,
        update: impl FnOnce(&mut InterfaceSet) -> Result<T, LinuxError>,
    ) -> Result<T, LinuxError> {
        self.with_inventory_mut(|interfaces, routing| {
            let previous_addr = interfaces
                .ipv4_addr_entries()
                .iter()
                .find(|entry| entry.dev == dev)
                .map(|entry| entry.addr);
            let result = update(interfaces)?;
            refresh_address_routes(interfaces, routing, previous_addr.as_slice());
            Ok(result)
        })
    }

    pub(crate) fn add_ipv4_addr(&self, entry: Ipv4AddrEntry) -> Result<(), LinuxError> {
        self.with_inventory_mut(|interfaces, routing| {
            interfaces.add_ipv4_addr(entry)?;
            routing.rebuild_ipv4_address_routes(interfaces);
            Ok(())
        })
    }

    pub(crate) fn set_primary_ipv4_addr(
        &self,
        dev: usize,
        addr: Ipv4Cidr,
    ) -> Result<(), LinuxError> {
        self.update_primary_address(dev, |interfaces| {
            interfaces.set_primary_ipv4_addr(dev, addr)
        })
    }

    pub(crate) fn remove_primary_ipv4_addr(&self, dev: usize) -> Result<(), LinuxError> {
        self.update_primary_address(dev, |interfaces| interfaces.remove_primary_ipv4_addr(dev))
    }

    /// Removes an address and its derived routes, reporting whether it existed.
    pub(crate) fn remove_ipv4_addr(&self, entry: Ipv4AddrEntry) -> bool {
        self.with_inventory_mut(|interfaces, routing| {
            let removed = interfaces.remove_ipv4_addr(entry);
            if removed {
                refresh_address_routes(interfaces, routing, &[entry.addr]);
            }
            removed
        })
    }

    pub(crate) fn set_ipv4_netmask(&self, dev: usize, prefix_len: u8) -> Result<(), LinuxError> {
        self.with_inventory_mut(|interfaces, routing| {
            interfaces.set_ipv4_netmask(dev, prefix_len)?;
            routing.rebuild_ipv4_address_routes(interfaces);
            Ok(())
        })
    }

    /// Revalidates the device identity and IPv4 source while submitting a
    /// copied route. Device removal can renumber slots after resolution.
    pub(crate) fn submit_ip_packet(
        &self,
        route: ResolvedRoute,
        source: IpAddress,
        packet: crate::buf::PacketBuf,
        timestamp: ktime_types::MonotonicInstant,
    ) -> bool {
        self.with_interfaces_mut(|interfaces| {
            submit_ip_packet(interfaces, route, source, packet, timestamp)
        })
    }

    /// Returns ownership on interface contention, before any device submission.
    pub(crate) fn try_submit_ip_packet(
        &self,
        route: ResolvedRoute,
        source: IpAddress,
        packet: crate::buf::PacketBuf,
        timestamp: ktime_types::MonotonicInstant,
    ) -> Result<bool, crate::buf::PacketBuf> {
        let Some(mut interfaces) = self.interfaces.try_write() else {
            return Err(packet);
        };
        Ok(submit_ip_packet(
            &mut interfaces,
            route,
            source,
            packet,
            timestamp,
        ))
    }
}

impl NetworkState {
    pub fn link_snapshots(&self) -> Vec<LinkSnapshot> {
        self.with_interfaces(|interfaces| interfaces.link_snapshots())
    }

    pub fn link_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSnapshot> {
        self.with_interfaces(|interfaces| interfaces.link_snapshot_for_ifindex(ifindex))
    }

    pub fn has_device(&self, ifindex: i32) -> bool {
        self.with_interfaces(|interfaces| interfaces.has_device(ifindex))
    }

    pub fn device_index_by_name(&self, name: &str) -> Option<usize> {
        self.with_interfaces(|interfaces| interfaces.device_index_by_name(name))
    }

    pub fn link_send_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSendSnapshot> {
        self.with_interfaces(|interfaces| interfaces.link_send_snapshot_for_ifindex(ifindex))
    }

    pub(crate) fn ipv4_addr_snapshots(&self) -> Vec<crate::device::interfaces::Ipv4AddrSnapshot> {
        self.with_interfaces(|interfaces| interfaces.ipv4_addr_snapshots())
    }

    #[cfg(unittest)]
    pub(crate) fn ipv4_addr_entries(&self) -> Vec<crate::device::interfaces::Ipv4AddrEntry> {
        self.with_interfaces(|interfaces| interfaces.ipv4_addr_entries().to_vec())
    }

    pub(crate) fn interface_ipv4_addr(
        &self,
        name: &str,
    ) -> Result<crate::ip::Ipv4Cidr, LinuxError> {
        self.with_interfaces(|interfaces| interfaces.interface_ipv4_addr(name))
    }

    pub(crate) fn route_snapshot(&self) -> Vec<Rule> {
        self.routing.read().route_snapshot()
    }

    #[cfg(unittest)]
    pub(crate) fn has_neighbor(&self, dev: usize, dst: IpAddress) -> bool {
        self.with_interfaces(|interfaces| interfaces.has_neighbor(dev, dst))
    }

    #[cfg(unittest)]
    pub(crate) fn lookup_route(&self, dst: &IpAddress) -> Option<Rule> {
        self.routing.read().lookup(dst).copied()
    }
}

fn resolve_route(
    interfaces: &InterfaceSet,
    routing: &Router,
    dst: &IpAddress,
) -> Option<ResolvedRoute> {
    let rule = routing.lookup(dst)?;
    let device = interfaces.devices().get(rule.dev)?;
    Some(ResolvedRoute {
        device: rule.dev,
        device_id: device.device_id(),
        next_hop: rule.via.unwrap_or(*dst),
        mtu: device.mtu().min(u16::MAX as usize),
        source: rule.src,
        is_loopback: device.link_kind() == LinkKind::Loopback,
    })
}

fn submit_ip_packet(
    interfaces: &mut InterfaceSet,
    route: ResolvedRoute,
    source: IpAddress,
    packet: crate::buf::PacketBuf,
    timestamp: ktime_types::MonotonicInstant,
) -> bool {
    if let IpAddress::Ipv4(source) = source
        && !interfaces.has_ipv4_address(source)
    {
        return false;
    }
    let Some(device) = interfaces.devices_mut().get_mut(route.device) else {
        return false;
    };
    // Removable devices have a model ID; the built-in loopback has
    // none and remains registered for the service lifetime.
    if device.device_id() != route.device_id {
        return false;
    }
    device.send_ip_packet(
        route.device as i32 + 1,
        route.next_hop,
        source,
        packet,
        timestamp,
    )
}

fn refresh_address_routes(
    interfaces: &InterfaceSet,
    routing: &mut Router,
    previous_addrs: &[Ipv4Cidr],
) {
    for addr in previous_addrs {
        if !interfaces.has_ipv4_address(addr.address()) {
            routing.remove_routes_with_source(addr.address());
        }
    }
    routing.rebuild_ipv4_address_routes(interfaces);
}

#[cfg(unittest)]
mod configuration_tests {
    use alloc::{boxed::Box, sync::Arc, vec};
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::{
        device::LoopbackDevice,
        ip::Ipv4Address,
        router::{ROUTE_PROTOCOL_KERNEL, ROUTE_SCOPE_HOST, ROUTE_SCOPE_UNIVERSE, Rule},
    };

    #[def_test]
    fn poller_queries_defer_inventory_and_routing_contention() {
        let state = NetworkState::new();
        let dst = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 2));
        {
            let _interfaces = state.interfaces.write();
            assert!(
                state
                    .try_with_interfaces(|interfaces| interfaces.effective_mtu())
                    .is_none()
            );
            assert!(state.try_resolve(&dst).is_none());
        }
        {
            let _routing = state.routing.write();
            assert!(state.try_resolve(&dst).is_none());
            assert!(state.interfaces.try_write().is_some());
        }
        assert_eq!(state.try_resolve(&dst), Some(None));
    }

    #[def_test]
    fn stale_route_does_not_transmit_on_a_reused_device_slot() {
        use crate::{
            buf::{PacketBuf, PacketOwner},
            device::test_device::TestDevice,
        };

        let state = NetworkState::new();
        let sends = Arc::new(AtomicUsize::new(0));
        let first = state.add_device(Box::new(TestDevice::ethernet(1, 1500)));
        let second = state.add_device(Box::new(TestDevice::ethernet(2, 1500).with_send_hook({
            let sends = sends.clone();
            move || {
                sends.fetch_add(1, Ordering::Relaxed);
            }
        })));
        let src = Ipv4Address::new(192, 0, 2, 1);
        for dev in [first, second] {
            state
                .add_ipv4_addr(Ipv4AddrEntry {
                    dev,
                    addr: Ipv4Cidr::new(src, 24),
                    scope: ROUTE_SCOPE_UNIVERSE,
                    broadcast: None,
                })
                .unwrap();
        }
        let dst = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 2));
        let stale = state.resolve(&dst).unwrap();
        assert_eq!(stale.device, first);
        state.remove_device(kdevice::DeviceId::new(1)).unwrap();
        let packet_fn = || PacketBuf::from_ip_packet_vec(0, vec![0; 20], PacketOwner::Ipv4Stack);
        state.submit_ip_packet(
            stale,
            IpAddress::Ipv4(src),
            packet_fn(),
            ktime_types::MonotonicInstant::ORIGIN,
        );
        assert_eq!(sends.load(Ordering::Relaxed), 0);

        let current = state.resolve(&dst).unwrap();
        assert_eq!(current.device, stale.device);
        state.submit_ip_packet(
            current,
            IpAddress::Ipv4(src),
            packet_fn(),
            ktime_types::MonotonicInstant::ORIGIN,
        );
        assert_eq!(sends.load(Ordering::Relaxed), 1);
    }

    #[def_test]
    fn removed_source_is_rejected_after_route_resolution() {
        use crate::{
            buf::{PacketBuf, PacketOwner},
            device::test_device::TestDevice,
        };

        let state = NetworkState::new();
        let sends = Arc::new(AtomicUsize::new(0));
        let dev = state.add_device(Box::new(TestDevice::ethernet(1, 1500).with_send_hook({
            let sends = sends.clone();
            move || {
                sends.fetch_add(1, Ordering::Relaxed);
            }
        })));
        let src = Ipv4Address::new(192, 0, 2, 1);
        let entry = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(src, 24),
            scope: ROUTE_SCOPE_UNIVERSE,
            broadcast: None,
        };
        state.add_ipv4_addr(entry).unwrap();
        let route = state
            .resolve(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 2)))
            .unwrap();
        assert!(state.remove_ipv4_addr(entry));
        let packet = PacketBuf::from_ip_packet_vec(0, vec![0; 20], PacketOwner::Ipv4Stack);
        state.submit_ip_packet(
            route,
            IpAddress::Ipv4(src),
            packet,
            ktime_types::MonotonicInstant::ORIGIN,
        );
        assert_eq!(sends.load(Ordering::Relaxed), 0);
    }

    #[def_test]
    fn ipv4_address_installs_local_and_connected_routes() {
        let router = NetworkState::new();
        let dev = router.add_device(Box::new(LoopbackDevice::new()));
        let entry = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        };

        router.add_ipv4_addr(entry).unwrap();

        let local = router
            .lookup_route(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 7)))
            .unwrap();
        assert_eq!(local.filter.prefix_len(), 32);
        assert_eq!(local.dev, dev);
        assert_eq!(local.scope, ROUTE_SCOPE_HOST);
        assert_eq!(local.protocol, ROUTE_PROTOCOL_KERNEL);

        let connected = router
            .lookup_route(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 8)))
            .unwrap();
        assert_eq!(connected.filter.prefix_len(), 24);
        assert_eq!(connected.dev, dev);
        assert_eq!(connected.scope, ROUTE_SCOPE_HOST);
        assert_eq!(connected.protocol, ROUTE_PROTOCOL_KERNEL);
    }

    #[def_test]
    fn ipv4_broadcast_detection_covers_limited_and_directed_addresses() {
        let router = NetworkState::new();
        let dev = router.add_device(Box::new(LoopbackDevice::new()));
        router
            .add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 7), 24),
                scope: ROUTE_SCOPE_HOST,
                broadcast: None,
            })
            .unwrap();

        assert!(router.ipv4_dest_requires_broadcast(&IpAddress::Ipv4(Ipv4Address::BROADCAST)));
        assert!(
            router.ipv4_dest_requires_broadcast(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 255)))
        );
        assert!(
            !router.ipv4_dest_requires_broadcast(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 8)))
        );
    }

    #[def_test]
    fn ipv4_address_capacity_matches_smoltcp_interface_limit() {
        let router = NetworkState::new();
        let dev = router.add_device(Box::new(LoopbackDevice::new()));

        for index in 0..smoltcp::config::IFACE_MAX_ADDR_COUNT {
            router
                .add_ipv4_addr(Ipv4AddrEntry {
                    dev,
                    addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, index as u8 + 1), 32),
                    scope: ROUTE_SCOPE_HOST,
                    broadcast: None,
                })
                .unwrap();
        }

        assert_eq!(
            router.add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 1), 32),
                scope: ROUTE_SCOPE_HOST,
                broadcast: None,
            }),
            Err(LinuxError::ENOBUFS)
        );
    }

    #[def_test]
    fn setting_primary_ipv4_address_keeps_secondary_addresses() {
        let router = NetworkState::new();
        let dev = router.add_device(Box::new(LoopbackDevice::new()));
        let primary = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: Some(Ipv4Address::new(192, 0, 2, 111)),
        };
        let secondary = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        };
        router.add_ipv4_addr(primary).unwrap();
        router.add_ipv4_addr(secondary).unwrap();

        let replacement = Ipv4Cidr::new(Ipv4Address::new(203, 0, 113, 7), 24);
        router.set_primary_ipv4_addr(dev, replacement).unwrap();

        assert_eq!(router.ipv4_addr_entries().len(), 2);
        assert_eq!(router.ipv4_addr_entries()[0].addr, replacement);
        assert_eq!(router.ipv4_addr_entries()[0].scope, ROUTE_SCOPE_UNIVERSE);
        assert_eq!(router.ipv4_addr_entries()[0].broadcast, None);
        assert_eq!(router.ipv4_addr_entries()[1], secondary);
    }

    #[def_test]
    fn removing_primary_ipv4_address_keeps_secondary_addresses() {
        let router = NetworkState::new();
        let dev = router.add_device(Box::new(LoopbackDevice::new()));
        let primary = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        };
        let secondary = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        };
        router.add_ipv4_addr(primary).unwrap();
        router.add_ipv4_addr(secondary).unwrap();

        router.remove_primary_ipv4_addr(dev).unwrap();

        assert_eq!(router.ipv4_addr_entries(), vec![secondary]);
    }

    #[def_test]
    fn setting_ipv4_netmask_keeps_secondary_scope_and_custom_broadcast() {
        let router = NetworkState::new();
        let dev = router.add_device(Box::new(LoopbackDevice::new()));
        let custom_broadcast = Ipv4Address::new(192, 0, 2, 111);
        let secondary = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 7), 24),
            scope: ROUTE_SCOPE_UNIVERSE,
            broadcast: None,
        };
        router
            .add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 7), 24),
                scope: ROUTE_SCOPE_HOST,
                broadcast: Some(custom_broadcast),
            })
            .unwrap();
        router.add_ipv4_addr(secondary).unwrap();

        router.set_ipv4_netmask(dev, 16).unwrap();

        assert_eq!(router.ipv4_addr_entries().len(), 2);
        assert_eq!(router.ipv4_addr_entries()[0].addr.prefix_len(), 16);
        assert_eq!(router.ipv4_addr_entries()[0].scope, ROUTE_SCOPE_HOST);
        assert_eq!(
            router.ipv4_addr_entries()[0].broadcast,
            Some(custom_broadcast)
        );
        assert_eq!(router.ipv4_addr_entries()[1], secondary);
    }

    #[def_test]
    fn deleting_ipv4_address_removes_automatic_routes() {
        let router = NetworkState::new();
        let dev = router.add_device(Box::new(LoopbackDevice::new()));
        let entry = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        };

        router.add_ipv4_addr(entry).unwrap();
        assert!(
            router
                .lookup_route(&IpAddress::Ipv4(Ipv4Address::new(198, 51, 100, 7)))
                .is_some()
        );

        assert!(router.remove_ipv4_addr(entry));
        assert!(
            router
                .lookup_route(&IpAddress::Ipv4(Ipv4Address::new(198, 51, 100, 7)))
                .is_none()
        );
    }

    #[def_test]
    fn configured_gateway_must_be_on_device_subnet() {
        let router = NetworkState::new();
        let dev = router.add_device(Box::new(LoopbackDevice::new()));
        router
            .add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 7), 24),
                scope: ROUTE_SCOPE_HOST,
                broadcast: None,
            })
            .unwrap();

        let unreachable = Rule::new(
            Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0).into(),
            Some(IpAddress::Ipv4(Ipv4Address::new(198, 51, 100, 1))),
            dev,
            IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 7)),
        );
        assert_eq!(
            router.add_route_rule(unreachable),
            Err(LinuxError::ENETUNREACH)
        );

        let default = Rule::new(
            Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0).into(),
            Some(IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 1))),
            dev,
            IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 7)),
        );
        router.add_route_rule(default).unwrap();
        router.remove_exact_route_rule(default);
        assert!(
            router
                .route_snapshot()
                .iter()
                .all(|route| !route.is_configured())
        );
    }
}
