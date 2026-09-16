// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network inventory and routing under independent read-mostly locks.
//!
//! Lock order when a path needs both: `interfaces` then `routing`.
//! Callers that need both must not acquire `routing` first. TX batches retain
//! these read guards through device submission; RX retains inventory read
//! access. Configuration therefore cannot invalidate addresses or ifindices
//! during a handoff. Queue preparation uses a copied [`ResolvedRoute`].
use alloc::{boxed::Box, vec::Vec};

use kerrno::{KError, KResult, LinuxError};
#[cfg(unittest)]
use ksync::RwLockWriteGuard;
use ksync::{MutexGuard, RwLock};
use ktime_types::MonotonicInstant;

use crate::{
    buf::PacketBuf,
    device::{
        DeviceHandle, LinkConfigUpdate, LinkKind, LinkSendSnapshot, LinkSnapshot, NeighborUpdate,
        NeighborUpdatePolicy, NetDevice,
        interfaces::{InterfaceSet, Ipv4AddrEntry},
    },
    ip::{IpAddress, Ipv4Address, Ipv4Cidr},
    network::router::{Router, Rule},
};

/// Copied forwarding decision; the routing and interface read locks are
/// already released when this value is returned. Submission revalidates the
/// handle's slot and the source under inventory read access through handoff.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedRoute {
    pub device: usize,
    handle: DeviceHandle,
    pub next_hop: IpAddress,
    /// Egress-device MTU copied at resolve time. TX fragmentation uses this
    /// instead of [`InterfaceSet::effective_mtu`] or a later inventory lookup.
    pub mtu: usize,
    pub source: IpAddress,
    pub is_loopback: bool,
}

impl ResolvedRoute {
    #[cfg(unittest)]
    pub(crate) fn lock_egress(
        &self,
    ) -> ksync::MutexGuard<'_, alloc::boxed::Box<dyn crate::device::NetDevice>> {
        self.handle.lock()
    }
}

/// Read-mostly inventory and route table. Configuration writers take the
/// corresponding write side; bounded device batches borrow read views.
/// Protocol processing and TX queue access run outside these guards.
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

    /// Nonblocking interface write guard for configuration tests that hold
    /// inventory exclusive access. Packet TX/RX no longer take this guard.
    #[cfg(unittest)]
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

    /// Borrows a consistent forwarding view for one bounded TX chunk.
    /// Device guards cached by the batch are released before its read views.
    pub(crate) fn try_with_tx_batch<R>(
        &self,
        use_fn: impl FnOnce(&mut TxBatch<'_>) -> R,
    ) -> Option<R> {
        let interfaces = self.interfaces.try_read()?;
        let routing = self.routing.try_read()?;
        Some(use_fn(&mut TxBatch {
            interfaces: &interfaces,
            routing: &routing,
            egress: None,
        }))
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
                return Ok(resolved_route(device_index, device, *dst, source));
            }
            let rule = routing
                .lookup(dst)
                .ok_or(KError::from(LinuxError::ENETUNREACH))?;
            let device = interfaces
                .devices()
                .get(rule.dev)
                .ok_or(KError::from(LinuxError::ENETUNREACH))?;
            Ok(resolved_route(
                rule.dev,
                device,
                rule.via.unwrap_or(*dst),
                source,
            ))
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

    /// Revalidates a copied slot, identity, and source through device handoff.
    /// The inventory read guard excludes address deletion and slot renumbering.
    pub(crate) fn submit_ip_packet(
        &self,
        route: ResolvedRoute,
        source: IpAddress,
        packet: crate::buf::PacketBuf,
        timestamp: ktime_types::MonotonicInstant,
    ) -> bool {
        self.with_interfaces(|interfaces| {
            if !has_local_source(interfaces, source) {
                return false;
            }
            let Some(device) = interfaces.devices().get(route.device) else {
                return false;
            };
            if device != &route.handle {
                return false;
            }
            device.send_ip_packet(
                route.device as i32 + 1,
                route.next_hop,
                source,
                packet,
                timestamp,
            )
        })
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

#[cfg(unittest)]
fn resolve_route(
    interfaces: &InterfaceSet,
    routing: &Router,
    dst: &IpAddress,
) -> Option<ResolvedRoute> {
    let rule = routing.lookup(dst)?;
    let device = interfaces.devices().get(rule.dev)?;
    Some(resolved_route(
        rule.dev,
        device,
        rule.via.unwrap_or(*dst),
        rule.src,
    ))
}

fn resolved_route(
    device_index: usize,
    device: &DeviceHandle,
    next_hop: IpAddress,
    source: IpAddress,
) -> ResolvedRoute {
    ResolvedRoute {
        device: device_index,
        handle: device.clone(),
        next_hop,
        mtu: device.mtu().min(u16::MAX as usize),
        source,
        is_loopback: device.link_kind() == LinkKind::Loopback,
    }
}

fn has_local_source(interfaces: &InterfaceSet, source: IpAddress) -> bool {
    match source {
        IpAddress::Ipv4(source) => interfaces.has_ipv4_address(source),
        IpAddress::Ipv6(_) => true,
    }
}

/// A bounded TX chunk's forwarding view and most recently used device guard.
///
/// The owning inventory guard keeps borrowed devices, indices, and addresses
/// stable. Consecutive unicast packets reuse one device guard. Switching egress
/// releases it before acquiring another; fan-out releases it before preflight.
pub(crate) struct TxBatch<'a> {
    interfaces: &'a InterfaceSet,
    routing: &'a Router,
    egress: Option<(usize, MutexGuard<'a, Box<dyn NetDevice>>)>,
}

impl TxBatch<'_> {
    pub(crate) fn resolve(&self, dst: &IpAddress) -> Option<Rule> {
        let rule = self.routing.lookup(dst)?;
        self.interfaces.devices().get(rule.dev)?;
        Some(*rule)
    }

    /// Returns the untouched packet when the selected device is busy.
    pub(crate) fn submit_ip_packet(
        &mut self,
        device_index: usize,
        next_hop: IpAddress,
        source: IpAddress,
        packet: PacketBuf,
        timestamp: MonotonicInstant,
    ) -> Result<bool, PacketBuf> {
        if !has_local_source(self.interfaces, source) {
            return Ok(false);
        }
        if self
            .egress
            .as_ref()
            .is_none_or(|(index, _)| *index != device_index)
        {
            self.egress = None;
            let Some(handle) = self.interfaces.devices().get(device_index) else {
                return Ok(false);
            };
            let Some(device) = handle.try_lock() else {
                return Err(packet);
            };
            self.egress = Some((device_index, device));
        }
        let (_, device) = self.egress.as_mut().unwrap();
        Ok(device.send_ip_packet(device_index as i32 + 1, next_hop, source, packet, timestamp))
    }

    /// Acquires the complete fan-out before sending so retries cannot duplicate
    /// a prefix. IPv4 broadcast permits an unspecified source for DHCP.
    pub(crate) fn broadcast(
        &mut self,
        bound_if: i32,
        source: IpAddress,
        destination: IpAddress,
        packet: PacketBuf,
        timestamp: MonotonicInstant,
    ) -> Result<bool, PacketBuf> {
        self.egress = None;
        if !source.is_unspecified() && !has_local_source(self.interfaces, source) {
            return Ok(false);
        }
        let mut guards = Vec::new();
        for (index, handle) in self.interfaces.devices().iter().enumerate() {
            let ifindex = index as i32 + 1;
            if bound_if != 0 && bound_if != ifindex {
                continue;
            }
            let Some(device) = handle.try_lock() else {
                return Err(packet);
            };
            guards.push((ifindex, device));
        }
        let Some((last, prefix)) = guards.split_last_mut() else {
            return Ok(false);
        };
        let mut has_rx_work = false;
        for (ifindex, device) in prefix {
            has_rx_work |=
                device.send_ip_packet(*ifindex, destination, source, packet.clone(), timestamp);
        }
        has_rx_work |= last
            .1
            .send_ip_packet(last.0, destination, source, packet, timestamp);
        Ok(has_rx_work)
    }
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
            assert!(
                state
                    .try_with_tx_batch(|batch| batch.resolve(&dst))
                    .is_none()
            );
        }
        {
            let _routing = state.routing.write();
            assert!(
                state
                    .try_with_tx_batch(|batch| batch.resolve(&dst))
                    .is_none()
            );
            assert!(state.interfaces.try_write().is_some());
        }
        assert!(matches!(
            state.try_with_tx_batch(|batch| batch.resolve(&dst)),
            Some(None)
        ));
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
            stale.clone(),
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
    fn stale_route_is_rejected_after_preceding_device_removal() {
        use crate::{buf::PacketOwner, device::test_device::TestDevice};

        let state = NetworkState::new();
        state.add_device(Box::new(TestDevice::ethernet(1, 1500)));
        let sends = Arc::new(AtomicUsize::new(0));
        let dev = state.add_device(Box::new(TestDevice::ethernet(2, 1500).with_send_hook({
            let sends = sends.clone();
            move || {
                sends.fetch_add(1, Ordering::Relaxed);
            }
        })));
        let source = Ipv4Address::new(192, 0, 2, 1);
        state
            .add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: Ipv4Cidr::new(source, 24),
                scope: ROUTE_SCOPE_UNIVERSE,
                broadcast: None,
            })
            .unwrap();
        let destination = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 2));
        let stale = state.resolve(&destination).unwrap();
        state.remove_device(kdevice::DeviceId::new(1)).unwrap();
        let packet_fn = || PacketBuf::from_ip_packet_vec(0, vec![0; 20], PacketOwner::Ipv4Stack);

        state.submit_ip_packet(
            stale,
            IpAddress::Ipv4(source),
            packet_fn(),
            MonotonicInstant::ORIGIN,
        );
        assert_eq!(sends.load(Ordering::Relaxed), 0);
        let current = state.resolve(&destination).unwrap();
        assert_eq!(current.device, 0);
        state.submit_ip_packet(
            current,
            IpAddress::Ipv4(source),
            packet_fn(),
            MonotonicInstant::ORIGIN,
        );
        assert_eq!(sends.load(Ordering::Relaxed), 1);
    }

    #[def_test]
    fn device_handoff_excludes_address_and_inventory_mutation() {
        use crate::{buf::PacketOwner, device::test_device::TestDevice};

        let state = Arc::new(NetworkState::new());
        let excluded_mutations = Arc::new(AtomicUsize::new(0));
        let dev = state.add_device(Box::new(TestDevice::ethernet(1, 1500).with_send_hook({
            let state = Arc::downgrade(&state);
            let excluded_mutations = excluded_mutations.clone();
            move || {
                let state = state.upgrade().unwrap();
                if state.try_write_interfaces().is_none() {
                    excluded_mutations.fetch_add(1, Ordering::Relaxed);
                }
            }
        })));
        let source = Ipv4Address::new(192, 0, 2, 1);
        let entry = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(source, 24),
            scope: ROUTE_SCOPE_UNIVERSE,
            broadcast: None,
        };
        state.add_ipv4_addr(entry).unwrap();
        let destination = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 2));
        let route = state.resolve(&destination).unwrap();
        let packet_fn = || PacketBuf::from_ip_packet_vec(0, vec![0; 20], PacketOwner::Ipv4Stack);

        state.submit_ip_packet(
            route.clone(),
            IpAddress::Ipv4(source),
            packet_fn(),
            MonotonicInstant::ORIGIN,
        );
        state
            .try_with_tx_batch(|batch| {
                batch
                    .submit_ip_packet(
                        dev,
                        destination,
                        IpAddress::Ipv4(source),
                        packet_fn(),
                        MonotonicInstant::ORIGIN,
                    )
                    .unwrap();
                batch
                    .broadcast(
                        0,
                        IpAddress::Ipv4(source),
                        IpAddress::Ipv4(Ipv4Address::BROADCAST),
                        packet_fn(),
                        MonotonicInstant::ORIGIN,
                    )
                    .unwrap();
            })
            .unwrap();
        assert_eq!(excluded_mutations.load(Ordering::Relaxed), 3);
        assert!(state.remove_ipv4_addr(entry));
        state.submit_ip_packet(
            route,
            IpAddress::Ipv4(source),
            packet_fn(),
            MonotonicInstant::ORIGIN,
        );
        assert_eq!(excluded_mutations.load(Ordering::Relaxed), 3);
    }

    #[def_test]
    fn unicast_submit_uses_device_mutex_not_inventory_write() {
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
        state
            .add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: Ipv4Cidr::new(src, 24),
                scope: ROUTE_SCOPE_UNIVERSE,
                broadcast: None,
            })
            .unwrap();
        let dst = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 2));
        let route = state.resolve(&dst).unwrap();
        let packet_fn = || PacketBuf::from_ip_packet_vec(0, vec![0; 20], PacketOwner::Ipv4Stack);

        {
            let _device = route.lock_egress();
            assert!(
                state
                    .try_with_tx_batch(|batch| batch.submit_ip_packet(
                        route.device,
                        route.next_hop,
                        IpAddress::Ipv4(src),
                        packet_fn(),
                        ktime_types::MonotonicInstant::ORIGIN,
                    ))
                    .unwrap()
                    .is_err()
            );
            assert_eq!(sends.load(Ordering::Relaxed), 0);
        }

        assert!(
            !state
                .try_with_tx_batch(|batch| batch.submit_ip_packet(
                    route.device,
                    route.next_hop,
                    IpAddress::Ipv4(src),
                    packet_fn(),
                    ktime_types::MonotonicInstant::ORIGIN,
                ))
                .unwrap()
                .expect("device mutex is free")
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
