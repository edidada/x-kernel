// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network inventory, addresses, and route projections under the service transaction.
use alloc::vec::Vec;

use kerrno::{KResult, LinuxError};

use crate::{
    device::{
        LinkSendSnapshot, LinkSnapshot,
        interfaces::{InterfaceSet, Ipv4AddrEntry},
    },
    ip::{IpAddress, Ipv4Cidr},
    network::router::{Router, Rule},
};

pub(crate) struct NetworkState {
    pub(crate) routing: Router,
    pub(crate) interfaces: InterfaceSet,
}
impl NetworkState {
    pub(crate) fn new() -> Self {
        Self {
            routing: Router::new(),
            interfaces: InterfaceSet::new(),
        }
    }

    pub(crate) fn select_source_address(&self, dst: &IpAddress) -> KResult<IpAddress> {
        self.routing.output_route_source(&self.interfaces, dst, 0)
    }

    pub(crate) fn route_mtu(&self, dst: &IpAddress) -> Option<usize> {
        self.routing.route_mtu(&self.interfaces, dst)
    }

    /// Returns the primary address for a zero-based device slot without allocating a snapshot.
    pub(crate) fn primary_ipv4_addr(&self, dev: usize) -> Option<Ipv4Cidr> {
        self.interfaces
            .ipv4_addr_entries()
            .iter()
            .find(|entry| entry.dev == dev)
            .map(|entry| entry.addr)
    }

    pub(crate) fn remove_device(&mut self, id: kdevice::DeviceId) -> Option<usize> {
        let previous_addrs = self.interfaces.local_ipv4_addrs();
        let index = self.interfaces.remove_device_by_model_id(id)?;
        self.routing.remove_device_routes(index);
        self.refresh_address_routes(&previous_addrs);
        Some(index)
    }

    fn update_primary_address<T>(
        &mut self,
        dev: usize,
        update: impl FnOnce(&mut InterfaceSet) -> Result<T, LinuxError>,
    ) -> Result<T, LinuxError> {
        let previous_addr = self.primary_ipv4_addr(dev);
        let result = update(&mut self.interfaces)?;
        self.refresh_address_routes(previous_addr.as_slice());
        Ok(result)
    }

    fn refresh_address_routes(&mut self, previous_addrs: &[Ipv4Cidr]) {
        for addr in previous_addrs {
            if !self.interfaces.has_ipv4_address(addr.address()) {
                self.routing.remove_routes_with_source(addr.address());
            }
        }
        self.routing.rebuild_ipv4_address_routes(&self.interfaces);
    }

    pub(crate) fn add_ipv4_addr(&mut self, entry: Ipv4AddrEntry) -> Result<(), LinuxError> {
        self.interfaces.add_ipv4_addr(entry)?;
        self.routing.rebuild_ipv4_address_routes(&self.interfaces);
        Ok(())
    }

    pub(crate) fn set_primary_ipv4_addr(
        &mut self,
        dev: usize,
        addr: Ipv4Cidr,
    ) -> Result<(), LinuxError> {
        self.update_primary_address(dev, |interfaces| {
            interfaces.set_primary_ipv4_addr(dev, addr)
        })
    }

    pub(crate) fn remove_primary_ipv4_addr(&mut self, dev: usize) -> Result<(), LinuxError> {
        self.update_primary_address(dev, |interfaces| interfaces.remove_primary_ipv4_addr(dev))
    }

    /// Removes an address and its derived routes, reporting whether it existed.
    pub(crate) fn remove_ipv4_addr(&mut self, entry: Ipv4AddrEntry) -> bool {
        let removed = self.interfaces.remove_ipv4_addr(entry);
        if removed {
            self.refresh_address_routes(&[entry.addr]);
        }
        removed
    }

    pub(crate) fn set_ipv4_netmask(
        &mut self,
        dev: usize,
        prefix_len: u8,
    ) -> Result<(), LinuxError> {
        self.interfaces.set_ipv4_netmask(dev, prefix_len)?;
        self.routing.rebuild_ipv4_address_routes(&self.interfaces);
        Ok(())
    }
}
impl NetworkState {
    pub fn link_snapshots(&self) -> Vec<LinkSnapshot> {
        self.interfaces.link_snapshots()
    }

    pub fn link_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSnapshot> {
        self.interfaces.link_snapshot_for_ifindex(ifindex)
    }

    pub fn has_device(&self, ifindex: i32) -> bool {
        self.interfaces.has_device(ifindex)
    }

    pub fn device_index_by_name(&self, name: &str) -> Option<usize> {
        self.interfaces.device_index_by_name(name)
    }

    pub fn link_send_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSendSnapshot> {
        self.interfaces.link_send_snapshot_for_ifindex(ifindex)
    }

    pub(crate) fn ipv4_addr_snapshots(&self) -> Vec<crate::device::interfaces::Ipv4AddrSnapshot> {
        self.interfaces.ipv4_addr_snapshots()
    }

    #[cfg(unittest)]
    pub(crate) fn ipv4_addr_entries(&self) -> Vec<crate::device::interfaces::Ipv4AddrEntry> {
        self.interfaces.ipv4_addr_entries().to_vec()
    }

    pub(crate) fn interface_ipv4_addr(
        &self,
        name: &str,
    ) -> Result<crate::ip::Ipv4Cidr, LinuxError> {
        self.interfaces.interface_ipv4_addr(name)
    }

    pub(crate) fn route_snapshot(&self) -> Vec<Rule> {
        self.routing.route_snapshot()
    }

    #[cfg(unittest)]
    pub(crate) fn has_neighbor(&self, dev: usize, dst: IpAddress) -> bool {
        self.interfaces.has_neighbor(dev, dst)
    }
}
#[cfg(unittest)]
mod configuration_tests {
    use alloc::boxed::Box;

    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::{
        device::LoopbackDevice,
        ip::Ipv4Address,
        router::{ROUTE_PROTOCOL_KERNEL, ROUTE_SCOPE_HOST, ROUTE_SCOPE_UNIVERSE, Rule},
    };

    #[def_test]
    fn ipv4_address_installs_local_and_connected_routes() {
        let mut router = NetworkState::new();
        let dev = router
            .interfaces
            .add_device(Box::new(LoopbackDevice::new()));
        let entry = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        };

        router.add_ipv4_addr(entry).unwrap();

        let local = router
            .routing
            .lookup(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 7)))
            .unwrap();
        assert_eq!(local.filter.prefix_len(), 32);
        assert_eq!(local.dev, dev);
        assert_eq!(local.scope, ROUTE_SCOPE_HOST);
        assert_eq!(local.protocol, ROUTE_PROTOCOL_KERNEL);

        let connected = router
            .routing
            .lookup(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 8)))
            .unwrap();
        assert_eq!(connected.filter.prefix_len(), 24);
        assert_eq!(connected.dev, dev);
        assert_eq!(connected.scope, ROUTE_SCOPE_HOST);
        assert_eq!(connected.protocol, ROUTE_PROTOCOL_KERNEL);
    }

    #[def_test]
    fn ipv4_broadcast_detection_covers_limited_and_directed_addresses() {
        let mut router = NetworkState::new();
        let dev = router
            .interfaces
            .add_device(Box::new(LoopbackDevice::new()));
        router
            .add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 7), 24),
                scope: ROUTE_SCOPE_HOST,
                broadcast: None,
            })
            .unwrap();

        assert!(
            router
                .interfaces
                .ipv4_dest_requires_broadcast(&IpAddress::Ipv4(Ipv4Address::BROADCAST))
        );
        assert!(
            router
                .interfaces
                .ipv4_dest_requires_broadcast(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 255)))
        );
        assert!(
            !router
                .interfaces
                .ipv4_dest_requires_broadcast(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 8)))
        );
    }

    #[def_test]
    fn ipv4_address_capacity_matches_smoltcp_interface_limit() {
        let mut router = NetworkState::new();
        let dev = router
            .interfaces
            .add_device(Box::new(LoopbackDevice::new()));

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
        let mut router = NetworkState::new();
        let dev = router
            .interfaces
            .add_device(Box::new(LoopbackDevice::new()));
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

        assert_eq!(router.interfaces.ipv4_addr_entries().len(), 2);
        assert_eq!(router.interfaces.ipv4_addr_entries()[0].addr, replacement);
        assert_eq!(
            router.interfaces.ipv4_addr_entries()[0].scope,
            ROUTE_SCOPE_UNIVERSE
        );
        assert_eq!(router.interfaces.ipv4_addr_entries()[0].broadcast, None);
        assert_eq!(router.interfaces.ipv4_addr_entries()[1], secondary);
    }

    #[def_test]
    fn removing_primary_ipv4_address_keeps_secondary_addresses() {
        let mut router = NetworkState::new();
        let dev = router
            .interfaces
            .add_device(Box::new(LoopbackDevice::new()));
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

        assert_eq!(router.interfaces.ipv4_addr_entries(), &[secondary]);
    }

    #[def_test]
    fn setting_ipv4_netmask_keeps_secondary_scope_and_custom_broadcast() {
        let mut router = NetworkState::new();
        let dev = router
            .interfaces
            .add_device(Box::new(LoopbackDevice::new()));
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

        assert_eq!(router.interfaces.ipv4_addr_entries().len(), 2);
        assert_eq!(
            router.interfaces.ipv4_addr_entries()[0].addr.prefix_len(),
            16
        );
        assert_eq!(
            router.interfaces.ipv4_addr_entries()[0].scope,
            ROUTE_SCOPE_HOST
        );
        assert_eq!(
            router.interfaces.ipv4_addr_entries()[0].broadcast,
            Some(custom_broadcast)
        );
        assert_eq!(router.interfaces.ipv4_addr_entries()[1], secondary);
    }

    #[def_test]
    fn deleting_ipv4_address_removes_automatic_routes() {
        let mut router = NetworkState::new();
        let dev = router
            .interfaces
            .add_device(Box::new(LoopbackDevice::new()));
        let entry = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        };

        router.add_ipv4_addr(entry).unwrap();
        assert!(
            router
                .routing
                .lookup(&IpAddress::Ipv4(Ipv4Address::new(198, 51, 100, 7)))
                .is_some()
        );

        assert!(router.remove_ipv4_addr(entry));
        assert!(
            router
                .routing
                .lookup(&IpAddress::Ipv4(Ipv4Address::new(198, 51, 100, 7)))
                .is_none()
        );
    }

    #[def_test]
    fn configured_gateway_must_be_on_device_subnet() {
        let mut router = NetworkState::new();
        let dev = router
            .interfaces
            .add_device(Box::new(LoopbackDevice::new()));
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
            router
                .routing
                .add_route_rule(&router.interfaces, unreachable),
            Err(LinuxError::ENETUNREACH)
        );

        let default = Rule::new(
            Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0).into(),
            Some(IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 1))),
            dev,
            IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 7)),
        );
        router
            .routing
            .add_route_rule(&router.interfaces, default)
            .unwrap();
        router.routing.remove_exact_route_rule(default);
        assert!(
            router
                .routing
                .route_snapshot()
                .iter()
                .all(|route| !route.is_configured())
        );
    }
}
