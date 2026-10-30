// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec, vec::Vec};

use ktime_types::MonotonicInstant;
use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::{
    device::{LinkSendSnapshot, LinkSnapshot, LoopbackDevice},
    network::output::Ipv4FragmentationPolicy,
    network::router::{
        ROUTE_PROTOCOL_KERNEL, ROUTE_SCOPE_HOST, ROUTE_SCOPE_LINK, ROUTE_TABLE_LOCAL,
        ROUTE_TYPE_BROADCAST, ROUTE_TABLE_MAIN, ROUTE_TYPE_UNICAST, RouteAttrs,
    },
};

fn valid_ipv4_cidr(address: Ipv4Address, prefix_len: u8) -> Ipv4Cidr {
    Ipv4Cidr::new(address, prefix_len).unwrap()
}

fn router_with_ready_loopback_packet() -> Router {
    let mut router = Router::new();
    let loopback = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let packet = PacketBuf::from_ip_packet_vec(1, vec![0x45, 0, 0, 20], PacketOwner::Ipv4Stack);
    let _ = router
        .devices
        .get_mut_by_id(loopback)
        .unwrap()
        .send_ip_packet(
            1,
            IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1)),
            IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1)),
            packet,
            MonotonicInstant::ORIGIN,
        );
    router
}

type TxLog = Arc<ksync::Mutex<Vec<(i32, IpAddress, PacketBuf)>>>;

struct FailingRxDevice {
    link: LoopbackDevice,
    packet: Option<PacketBuf>,
    up: bool,
    tx_log: Option<TxLog>,
}

impl NetDevice for FailingRxDevice {
    fn name(&self) -> &str {
        self.link.name()
    }

    fn link_kind(&self) -> LinkKind {
        LinkKind::Ethernet
    }

    fn mtu(&self) -> usize {
        self.link.mtu()
    }

    fn is_link_up(&self) -> bool {
        self.up
    }

    fn set_link_up(&mut self, is_up: bool) {
        self.up = is_up;
    }

    fn link_snapshot(&self, ifindex: i32) -> LinkSnapshot {
        self.link.link_snapshot(ifindex)
    }

    fn link_send_snapshot(&self) -> LinkSendSnapshot {
        self.link.link_send_snapshot()
    }

    fn poll_rx(&mut self, _ifindex: i32, _timestamp: MonotonicInstant) -> KResult<RxPoll> {
        self.packet
            .take()
            .map(|packet| RxPoll::Consumed(Some(packet)))
            .ok_or(KError::Io)
    }

    fn has_rx_work(&self) -> bool {
        self.packet.is_some()
    }

    fn send_ip_packet(
        &mut self,
        ifindex: i32,
        next_hop: IpAddress,
        _source_addr: IpAddress,
        packet: PacketBuf,
        _timestamp: MonotonicInstant,
    ) -> bool {
        if let Some(log) = &self.tx_log {
            log.lock().push((ifindex, next_hop, packet));
        }
        false
    }

    fn register_rx_waker(
        &self,
        source_waker: &core::task::Waker,
        context: &mut PollContext<'_>,
    ) -> Result<(), PollRegisterError> {
        self.link.register_rx_waker(source_waker, context)
    }
}

#[def_test]
fn rx_failure_preserves_partial_batch_and_advances_device_cursor() {
    let mut router = Router::new();
    for _ in 0..2 {
        router
            .add_device(kalloc_types::KBox::new(FailingRxDevice {
                link: LoopbackDevice::new(),
                packet: Some(tx_packet_buf(vec![0x45, 0, 0, 20])),
                up: true,
            tx_log: None,
}).unwrap())
            .unwrap();
    }
    let mut packets = Vec::new();
    let first = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 4, &mut packets);
    assert_eq!(first.work_done, 1);
    assert_eq!(packets.len(), 1);
    assert_eq!(first.error, Some((2, KError::Io)));
    assert!(first.has_more);

    let second = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 4, &mut packets);
    assert_eq!(second.work_done, 1);
    assert_eq!(packets.len(), 1);
    assert_eq!(second.error, Some((3, KError::Io)));
    assert!(!second.has_more);
}

#[def_test(serial)]
fn exact_rx_budget_does_not_report_empty_device() {
    let mut router = router_with_ready_loopback_packet();
    let mut packets = Vec::new();

    let drain = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);

    assert_eq!(drain.work_done, 1);
    assert_eq!(drain.error, None);
    assert!(!drain.has_more);
}

#[def_test(serial)]
fn zero_rx_budget_reports_ready_device() {
    let mut router = router_with_ready_loopback_packet();
    let mut packets = Vec::new();

    let drain = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 0, &mut packets);

    assert_eq!(drain.work_done, 0);
    assert!(drain.has_more);
}

#[def_test]
fn control_dispatch_does_not_free_data_tx_capacity() {
    let mut router = Router::new();
    router
        .control_tx_queue
        .push_back(tx_packet_buf(vec![0; 20]));
    let capacity_before = router.available_tx_packet_slots();

    let (work_done, _) = router.dispatch_budgeted(MonotonicInstant::ORIGIN, 1);

    assert_eq!(work_done, 1);
    assert_eq!(router.available_tx_packet_slots(), capacity_before);
}

#[def_test]
fn data_dispatch_frees_data_tx_capacity() {
    let mut router = Router::new();
    assert!(
        router
            .data_tx_queue
            .try_push(tx_packet_buf(vec![0; 20]))
            .is_ok()
    );
    let capacity_before = router.available_tx_packet_slots();

    let (work_done, _) = router.dispatch_budgeted(MonotonicInstant::ORIGIN, 1);

    assert_eq!(work_done, 1);
    assert_eq!(router.available_tx_packet_slots(), capacity_before + 1);
}

#[def_test]
fn unrelated_address_update_preserves_automatic_and_configured_route_order() {
    for is_configured_first in [false, true] {
        let mut router = Router::new();
        let first = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
        let second = router
            .add_device(kalloc_types::KBox::new(FailingRxDevice {
                link: LoopbackDevice::new(),
                packet: None,
                up: true,
            tx_log: None,
}).unwrap())
            .unwrap();
        let primary = Ipv4AddrEntry {
            addr: valid_ipv4_cidr(Ipv4Address::new(10, 0, 0, 1), 24),
            scope: ROUTE_SCOPE_UNIVERSE,
            broadcast: None,
            no_prefix_route: false,
            label: None,
        };
        router.add_ipv4_addr(first, primary).unwrap();
        let connected = Ipv4AddrEntry {
            addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
            ..primary
        };
        let configured = Rule::new(
            connected.addr.network().into(),
            None,
            second,
            primary.addr.address().into(),
        );
        if !is_configured_first {
            router.add_ipv4_addr(first, connected).unwrap();
        }
        router.add_route_rule(configured).unwrap();
        if is_configured_first {
            router.add_ipv4_addr(first, connected).unwrap();
        }
        let expected_dev = if is_configured_first { second } else { first };
        let destination = Ipv4Address::new(192, 0, 2, 99).into();
        assert_eq!(router.table.lookup(&destination).unwrap().dev, expected_dev);

        let unrelated = Ipv4AddrEntry {
            addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 7), 24),
            ..primary
        };
        router.add_ipv4_addr(first, unrelated).unwrap();
        assert_eq!(router.table.lookup(&destination).unwrap().dev, expected_dev);
        assert!(!router.remove_ipv4_addr(first, unrelated.addr).is_empty());
        assert_eq!(router.table.lookup(&destination).unwrap().dev, expected_dev);

        router.replace_route_rule(configured, configured).unwrap();
        assert_eq!(router.table.lookup(&destination).unwrap().dev, first);
    }
}

#[def_test]
fn cross_device_address_iteration_does_not_reorder_surviving_routes() {
    let mut router = Router::new();
    let first = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let second = router
        .add_device(kalloc_types::KBox::new(FailingRxDevice {
            link: LoopbackDevice::new(),
            packet: None,
            up: true,
            tx_log: None,
}).unwrap())
        .unwrap();
    let first_primary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(10, 0, 0, 1), 24),
        scope: ROUTE_SCOPE_UNIVERSE,
        broadcast: None,
        no_prefix_route: false,
        label: None,
    };
    let second_primary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 1), 24),
        ..first_primary
    };
    let first_secondary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 2), 24),
        ..first_primary
    };
    for (dev, entry) in [
        (first, first_primary),
        (second, second_primary),
        (first, first_secondary),
    ] {
        router.add_ipv4_addr(dev, entry).unwrap();
    }
    let destination = Ipv4Address::new(192, 0, 2, 99).into();
    assert_eq!(router.table.lookup(&destination).unwrap().dev, second);
    assert_eq!(
        router.local_ipv4_addrs(),
        vec![
            first_primary.addr,
            first_secondary.addr,
            second_primary.addr
        ]
    );
    router
        .set_primary_ipv4_addr(first, valid_ipv4_cidr(Ipv4Address::new(10, 1, 0, 1), 24))
        .unwrap();
    assert_eq!(router.table.lookup(&destination).unwrap().dev, second);
    assert_eq!(
        router
            .table
            .lookup(&destination)
            .unwrap()
            .preferred_source(),
        Some(second_primary.addr.address().into())
    );

    assert!(
        !router
            .remove_ipv4_addr(second, second_primary.addr)
            .is_empty()
    );
    assert_eq!(router.table.lookup(&destination).unwrap().dev, first);
    assert!(
        router
            .table
            .lookup(&second_primary.addr.address().into())
            .unwrap()
            .filter
            .prefix_len()
            < 32
    );
    router.add_ipv4_addr(second, second_primary).unwrap();
    assert_eq!(router.table.lookup(&destination).unwrap().dev, first);
    assert_eq!(
        router
            .table
            .lookup(&second_primary.addr.address().into())
            .unwrap()
            .filter
            .prefix_len(),
        32
    );
}

#[def_test]
fn deleting_connected_primary_cascades_secondaries_and_falls_back() {
    let mut router = Router::new();
    let first = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let second = router
        .add_device(kalloc_types::KBox::new(FailingRxDevice {
            link: LoopbackDevice::new(),
            packet: None,
            up: true,
            tx_log: None,
}).unwrap())
        .unwrap();
    let primary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 1), 24),
        scope: ROUTE_SCOPE_UNIVERSE,
        broadcast: None,
        no_prefix_route: false,
        label: None,
    };
    let competing = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 2), 24),
        ..primary
    };
    let secondary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 3), 24),
        ..primary
    };
    for (dev, entry) in [(first, primary), (second, competing), (first, secondary)] {
        router.add_ipv4_addr(dev, entry).unwrap();
    }
    // Removing the primary also removes its matching secondary.  The other
    // device's primary for the same subnet stays and provides the source.
    router.remove_primary_ipv4_addr(first).unwrap();
    assert!(router.ipv4_addr_entries(first).is_empty());
    let destination = Ipv4Address::new(192, 0, 2, 99).into();
    let selected = router.table.lookup(&destination).unwrap();
    assert_eq!(selected.dev, second);
    assert_eq!(
        selected.preferred_source(),
        Some(competing.addr.address().into())
    );
    assert_eq!(
        router.output_route_source(&destination, 0).unwrap(),
        competing.addr.address().into()
    );
    assert!(router.remove_ipv4_addr(first, secondary.addr).is_empty());
    assert_eq!(router.table.lookup(&destination).unwrap().dev, second);
}

#[def_test]
fn local_address_output_survives_owner_link_down_without_oif() {
    let mut router = Router::new();
    let loopback = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let owner = router
        .add_device(kalloc_types::KBox::new(FailingRxDevice {
            link: LoopbackDevice::new(),
            packet: None,
            up: true,
            tx_log: None,
}).unwrap())
        .unwrap();
    let entry = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 7), 24),
        scope: ROUTE_SCOPE_UNIVERSE,
        broadcast: None,
        no_prefix_route: false,
        label: None,
    };
    router.add_ipv4_addr(owner, entry).unwrap();
    for (source, output, expected) in [
        (None, None, Ipv4Address::LOOPBACK),
        (Some(entry.addr.address()), None, entry.addr.address()),
        (None, Some(owner), entry.addr.address()),
    ] {
        let route = router
            .lookup_ipv4_output(Ipv4Address::UNSPECIFIED, source, output)
            .unwrap();
        assert_eq!(route.destination, expected);
        assert_eq!(route.source, Some(expected));
        assert_eq!(route.oif, loopback);
    }
    router
        .devices
        .get_mut_by_id(owner)
        .unwrap()
        .set_link_up(false);
    router.sync_ipv4_address_routes();

    let destination = entry.addr.address().into();
    let local = router.table.lookup(&destination).unwrap();
    assert_eq!(local.dev, loopback);
    assert_eq!(local.lookup_device(), owner);
    // Linux keeps the local-table route and resolves output through loopback
    // even after the address owner goes down.
    assert_eq!(
        router.output_route_source(&destination, 0),
        Ok(entry.addr.address().into())
    );
    // An explicit OIF still validates the FIB device before the lookup.
    assert_eq!(
        router.output_route_source(&destination, owner.ifindex()),
        Err(LinuxError::ENETUNREACH.into())
    );
}

#[def_test]
fn ipv4_address_installs_local_and_connected_routes() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let entry = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
        scope: ROUTE_SCOPE_HOST,
        broadcast: None,
        no_prefix_route: false,
        label: None,
    };

    router.add_ipv4_addr(dev, entry).unwrap();

    let local = router
        .table
        .lookup(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 7)))
        .unwrap();
    assert_eq!(local.filter.prefix_len(), 32);
    assert_eq!(local.dev, dev);
    assert_eq!(local.scope, ROUTE_SCOPE_HOST);
    assert_eq!(local.protocol, ROUTE_PROTOCOL_KERNEL);

    let connected = router
        .table
        .lookup(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 8)))
        .unwrap();
    assert_eq!(connected.filter.prefix_len(), 24);
    assert_eq!(connected.dev, dev);
    assert_eq!(connected.scope, ROUTE_SCOPE_HOST);
    assert_eq!(connected.protocol, ROUTE_PROTOCOL_KERNEL);
}

#[def_test]
fn explicit_ipv4_broadcast_installs_and_replaces_a_broadcast_route() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let assigned = Ipv4Address::new(192, 0, 2, 111);
    let entry = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
        scope: ROUTE_SCOPE_UNIVERSE,
        broadcast: Some(assigned),
        no_prefix_route: false,
        label: None,
    };
    router.add_ipv4_addr(dev, entry).unwrap();

    let explicit = router
        .table
        .snapshot()
        .into_iter()
        .find(|route| route.filter == valid_ipv4_cidr(assigned, 32).into())
        .unwrap();
    assert_eq!(explicit.dev, dev);
    assert_eq!(explicit.table, ROUTE_TABLE_LOCAL);
    assert_eq!(explicit.route_type, ROUTE_TYPE_BROADCAST);
    assert_eq!(explicit.scope, ROUTE_SCOPE_LINK);
    assert_eq!(explicit.protocol, ROUTE_PROTOCOL_KERNEL);
    assert_eq!(
        explicit.preferred_source(),
        Some(IpAddress::Ipv4(entry.addr.address()))
    );

    // The computed network broadcast coexists with an explicit assignment.
    let computed = Ipv4Address::new(192, 0, 2, 255);
    assert!(
        router
            .table
            .snapshot()
            .iter()
            .any(|route| route.filter == valid_ipv4_cidr(computed, 32).into())
    );

    let replacement = Ipv4Address::new(192, 0, 2, 222);
    router.set_ipv4_broadcast(dev, replacement).unwrap();
    let routes = router.table.snapshot();
    assert!(
        !routes
            .iter()
            .any(|route| route.filter == valid_ipv4_cidr(assigned, 32).into())
    );
    assert!(
        routes
            .iter()
            .any(|route| route.filter == valid_ipv4_cidr(replacement, 32).into())
    );
    assert!(
        routes
            .iter()
            .any(|route| route.filter == valid_ipv4_cidr(computed, 32).into())
    );

    // Assigning the limited broadcast suppresses only the explicit route.
    router
        .set_ipv4_broadcast(dev, Ipv4Address::BROADCAST)
        .unwrap();
    let routes = router.table.snapshot();
    assert!(
        !routes
            .iter()
            .any(|route| route.filter == valid_ipv4_cidr(replacement, 32).into())
    );
    assert!(
        routes
            .iter()
            .any(|route| route.filter == valid_ipv4_cidr(computed, 32).into())
    );
}

#[def_test]
fn implicit_ipv4_broadcast_assignment_updates_stored_value_without_duplicate_route() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    router
        .add_ipv4_addr(
            dev,
            Ipv4AddrEntry {
                addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
                scope: ROUTE_SCOPE_UNIVERSE,
                broadcast: None,
                no_prefix_route: false,
                label: None,
            },
        )
        .unwrap();

    let computed = Ipv4Address::new(192, 0, 2, 255);
    router.set_ipv4_broadcast(dev, computed).unwrap();
    assert_eq!(router.ipv4_addr_entries(dev)[0].broadcast, Some(computed));
    assert_eq!(
        router
            .table
            .snapshot()
            .iter()
            .filter(|route| route.filter == valid_ipv4_cidr(computed, 32).into())
            .count(),
        1
    );

    router
        .set_ipv4_broadcast(dev, Ipv4Address::UNSPECIFIED)
        .unwrap();
    assert_eq!(router.ipv4_addr_entries(dev)[0].broadcast, None);
    assert_eq!(
        router
            .table
            .snapshot()
            .iter()
            .filter(|route| route.filter == valid_ipv4_cidr(computed, 32).into())
            .count(),
        1
    );
}

#[def_test]
fn zero_ipv4_broadcast_is_not_derived_by_netmask_changes() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    router
        .add_ipv4_addr(
            dev,
            Ipv4AddrEntry {
                addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
                scope: ROUTE_SCOPE_UNIVERSE,
                broadcast: None,
                no_prefix_route: false,
                label: None,
            },
        )
        .unwrap();

    router.set_ipv4_netmask(dev, 16).unwrap();
    assert_eq!(router.ipv4_addr_entries(dev)[0].broadcast, None);
}

#[def_test]
fn explicit_ipv4_broadcast_route_follows_link_state_and_prefix_suppression() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let assigned = Ipv4Address::new(198, 51, 100, 111);
    let entry = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 7), 24),
        scope: ROUTE_SCOPE_UNIVERSE,
        broadcast: Some(assigned),
        no_prefix_route: true,
        label: None,
    };
    router.add_ipv4_addr(dev, entry).unwrap();
    let broadcast_route = valid_ipv4_cidr(assigned, 32).into();
    let connected_route = entry.addr.network().into();
    assert!(
        router
            .table
            .snapshot()
            .iter()
            .any(|route| route.filter == broadcast_route)
    );
    assert!(
        !router
            .table
            .snapshot()
            .iter()
            .any(|route| route.filter == connected_route)
    );

    router
        .devices
        .get_mut_by_id(dev)
        .unwrap()
        .set_link_up(false);
    router.sync_ipv4_address_routes();
    assert!(
        !router
            .table
            .snapshot()
            .iter()
            .any(|route| route.filter == broadcast_route)
    );

    router.devices.get_mut_by_id(dev).unwrap().set_link_up(true);
    router.sync_ipv4_address_routes();
    assert!(
        router
            .table
            .snapshot()
            .iter()
            .any(|route| route.filter == broadcast_route)
    );
    assert!(
        !router
            .table
            .snapshot()
            .iter()
            .any(|route| route.filter == connected_route)
    );
}

#[def_test]
fn ipv4_netmask_change_updates_computed_broadcast_and_keeps_explicit_one() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let explicit = Ipv4Address::new(192, 168, 1, 111);
    router
        .add_ipv4_addr(
            dev,
            Ipv4AddrEntry {
                addr: valid_ipv4_cidr(Ipv4Address::new(192, 168, 1, 7), 24),
                scope: ROUTE_SCOPE_UNIVERSE,
                broadcast: Some(explicit),
                no_prefix_route: false,
                label: None,
            },
        )
        .unwrap();
    router.set_ipv4_netmask(dev, 16).unwrap();
    let routes = router.table.snapshot();
    assert!(
        routes
            .iter()
            .any(|route| route.filter == valid_ipv4_cidr(explicit, 32).into())
    );
    assert!(
        routes.iter().any(|route| route.filter
            == valid_ipv4_cidr(Ipv4Address::new(192, 168, 255, 255), 32).into())
    );
    assert!(!routes.iter().any(
        |route| route.filter == valid_ipv4_cidr(Ipv4Address::new(192, 168, 1, 255), 32).into()
    ));
}

#[def_test]
fn ipv4_broadcast_detection_covers_limited_and_directed_addresses() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    router
        .add_ipv4_addr(
            dev,
            Ipv4AddrEntry {
                addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
                scope: ROUTE_SCOPE_HOST,
                broadcast: None,
                no_prefix_route: false,
                label: None,
            },
        )
        .unwrap();

    assert!(output::requires_broadcast(
        &router.devices,
        &IpAddress::Ipv4(Ipv4Address::BROADCAST)
    ));
    assert!(output::requires_broadcast(
        &router.devices,
        &IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 255))
    ));
    assert!(!output::requires_broadcast(
        &router.devices,
        &IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 8))
    ));
}

#[def_test]
fn setting_primary_ipv4_address_preserves_unrelated_addresses() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let primary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
        scope: ROUTE_SCOPE_HOST,
        broadcast: Some(Ipv4Address::new(192, 0, 2, 111)),
        no_prefix_route: true,
        label: None,
    };
    let secondary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 7), 24),
        scope: ROUTE_SCOPE_HOST,
        broadcast: None,
        no_prefix_route: false,
        label: None,
    };
    router.add_ipv4_addr(dev, primary).unwrap();
    router.add_ipv4_addr(dev, secondary).unwrap();

    router
        .set_primary_ipv4_addr(dev, valid_ipv4_cidr(primary.addr.address(), 16))
        .unwrap();
    assert_eq!(router.ipv4_addr_entries(dev)[0], primary);
    assert_eq!(
        router.set_primary_ipv4_addr(dev, secondary.addr),
        Err(LinuxError::EEXIST)
    );
    assert_eq!(router.ipv4_addr_entries(dev), &[primary, secondary]);

    let replacement = valid_ipv4_cidr(Ipv4Address::new(203, 0, 113, 7), 24);
    router.set_primary_ipv4_addr(dev, replacement).unwrap();

    assert_eq!(router.ipv4_addr_entries(dev).len(), 2);
    // Linux reinserts a replacement at its scope-derived position.  The new
    // universe-scope primary follows the surviving host-scope address.
    assert_eq!(router.ipv4_addr_entries(dev)[0], secondary);
    assert_eq!(router.ipv4_addr_entries(dev)[1].addr, replacement);
    assert_eq!(router.ipv4_addr_entries(dev)[1].scope, ROUTE_SCOPE_UNIVERSE);
    assert_eq!(router.ipv4_addr_entries(dev)[1].broadcast, None);
    // `SIOCSIFADDR` clears the derived broadcast and resets scope, but it
    // preserves `IFA_F_NOPREFIXROUTE` on the replaced address.
    assert!(router.ipv4_addr_entries(dev)[1].no_prefix_route);
}

#[def_test]
fn removing_primary_ipv4_address_preserves_unrelated_addresses() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let primary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
        scope: ROUTE_SCOPE_HOST,
        broadcast: None,
        no_prefix_route: false,
        label: None,
    };
    let secondary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 7), 24),
        scope: ROUTE_SCOPE_HOST,
        broadcast: None,
        no_prefix_route: false,
        label: None,
    };
    router.add_ipv4_addr(dev, primary).unwrap();
    router.add_ipv4_addr(dev, secondary).unwrap();

    router.remove_primary_ipv4_addr(dev).unwrap();

    assert_eq!(router.ipv4_addr_entries(dev), &[secondary]);
    assert_eq!(router.interface_ipv4_addr("lo"), Ok(secondary.addr));
}

#[def_test]
fn setting_ipv4_netmask_keeps_secondary_scope_and_custom_broadcast() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let custom_broadcast = Ipv4Address::new(192, 0, 2, 111);
    let secondary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 7), 24),
        scope: ROUTE_SCOPE_UNIVERSE,
        broadcast: None,
        no_prefix_route: false,
        label: None,
    };
    router
        .add_ipv4_addr(
            dev,
            Ipv4AddrEntry {
                addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
                scope: ROUTE_SCOPE_HOST,
                broadcast: Some(custom_broadcast),
                no_prefix_route: false,
                label: None,
            },
        )
        .unwrap();
    router.add_ipv4_addr(dev, secondary).unwrap();

    router.set_ipv4_netmask(dev, 16).unwrap();

    assert_eq!(router.ipv4_addr_entries(dev).len(), 2);
    assert_eq!(router.ipv4_addr_entries(dev)[0].addr.prefix_len(), 16);
    assert_eq!(router.ipv4_addr_entries(dev)[0].scope, ROUTE_SCOPE_HOST);
    assert_eq!(
        router.ipv4_addr_entries(dev)[0].broadcast,
        Some(custom_broadcast)
    );
    assert_eq!(router.ipv4_addr_entries(dev)[1], secondary);
}

#[def_test]
fn setting_ipv4_netmask_collision_keeps_existing_target() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let primary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
        scope: ROUTE_SCOPE_HOST,
        broadcast: None,
        no_prefix_route: false,
        label: None,
    };
    let secondary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(primary.addr.address(), 16),
        broadcast: Some(Ipv4Address::new(192, 0, 255, 111)),
        no_prefix_route: false,
        label: None,
        ..primary
    };
    router.add_ipv4_addr(dev, primary).unwrap();
    router.add_ipv4_addr(dev, secondary).unwrap();
    assert_eq!(router.set_ipv4_netmask(dev, 24), Ok(()));
    assert_eq!(router.ipv4_addr_entries(dev), &[primary, secondary]);
    assert_eq!(router.set_ipv4_netmask(dev, 16), Ok(()));
    assert_eq!(router.ipv4_addr_entries(dev), &[secondary]);
    assert_eq!(router.local_ipv4_addrs(), vec![secondary.addr]);
    let destination = Ipv4Address::new(192, 0, 2, 8).into();
    assert_eq!(
        router.table.lookup(&destination).unwrap().filter,
        secondary.addr.network().into()
    );
    assert_eq!(
        router.output_route_source(&destination, 0),
        Ok(primary.addr.address().into())
    );
}

#[def_test]
fn deleting_ipv4_address_removes_automatic_routes() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let entry = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 7), 24),
        scope: ROUTE_SCOPE_HOST,
        broadcast: None,
        no_prefix_route: false,
        label: None,
    };
    let secondary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(entry.addr.address(), 16),
        ..entry
    };
    router.add_ipv4_addr(dev, entry).unwrap();
    router.add_ipv4_addr(dev, secondary).unwrap();
    let host = valid_ipv4_cidr(entry.addr.address(), 32);
    assert_eq!(
        router
            .table
            .snapshot()
            .iter()
            .filter(|route| route.filter == host.into())
            .count(),
        1
    );

    assert!(!router.remove_ipv4_addr(dev, entry.addr).is_empty());
    assert_eq!(
        router.table.lookup(&host.address().into()).unwrap().filter,
        host.into()
    );
    assert!(
        router
            .table
            .lookup(&Ipv4Address::new(198, 51, 100, 8).into())
            .is_some()
    );
    assert!(!router.remove_ipv4_addr(dev, secondary.addr).is_empty());
    assert!(router.table.lookup(&host.address().into()).is_none());
    assert!(
        router
            .table
            .lookup(&Ipv4Address::new(198, 51, 100, 8).into())
            .is_none()
    );
}

#[def_test]
fn configured_gateway_must_be_on_device_subnet() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    router
        .add_ipv4_addr(
            dev,
            Ipv4AddrEntry {
                addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
                scope: ROUTE_SCOPE_HOST,
                broadcast: None,
                no_prefix_route: false,
                label: None,
            },
        )
        .unwrap();

    let unreachable = Rule::new(
        valid_ipv4_cidr(Ipv4Address::UNSPECIFIED, 0).into(),
        Some(IpAddress::Ipv4(Ipv4Address::new(198, 51, 100, 1))),
        dev,
        IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 7)),
    );
    assert_eq!(
        router.add_route_rule(unreachable),
        Err(LinuxError::ENETUNREACH)
    );

    let default = Rule::new(
        valid_ipv4_cidr(Ipv4Address::UNSPECIFIED, 0).into(),
        Some(IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 1))),
        dev,
        IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 7)),
    );
    router.add_route_rule(default).unwrap();
    let routes_before = router.table.snapshot().len();
    assert_eq!(
        router.replace_route_rule(default, unreachable),
        Err(LinuxError::ENETUNREACH)
    );
    assert_eq!(router.table.snapshot().len(), routes_before);
    assert_eq!(
        router
            .table
            .lookup(&Ipv4Address::new(203, 0, 113, 1).into())
            .unwrap()
            .via,
        default.via
    );
    router.table.remove_exact_rule(default);
    assert!(
        router
            .table
            .snapshot()
            .iter()
            .all(|route| !route.is_configured())
    );
}

fn output_test_router() -> (Router, NetDeviceId) {
    let (router, dev, _) = recording_output_router();
    (router, dev)
}

fn recording_output_router() -> (Router, NetDeviceId, TxLog) {
    let mut router = Router::new();
    let tx_log = Arc::new(ksync::Mutex::new(Vec::new()));
    let mut link = LoopbackDevice::new();
    link.set_mtu(100).unwrap();
    let dev = router
        .add_device(kalloc_types::KBox::new(FailingRxDevice { link, packet: None, up: true,
            tx_log: Some(tx_log.clone()),
}).unwrap())
        .unwrap();
    router
        .add_ipv4_addr(
            dev,
            Ipv4AddrEntry {
                addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
                scope: ROUTE_SCOPE_UNIVERSE,
                broadcast: None,
            },
        )
        .unwrap();
    (router, dev, tx_log)
}

#[def_test]
fn output_route_uses_selected_device_and_validates_binding() {
    let (mut router, dev) = output_test_router();
    let dst = Ipv4Address::new(203, 0, 113, 1).into();
    assert_eq!(
        router.resolve_output_route(&dst, 0).err(),
        Some(LinuxError::ENETUNREACH.into())
    );
    let mut loopback = LoopbackDevice::new();
    loopback.set_mtu(68).unwrap();
    let lo = router.add_device(kalloc_types::KBox::new(loopback).unwrap()).unwrap();
    let source = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 7));
    let gateway = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 1));
    router
        .add_route_rule(Rule::new(
            valid_ipv4_cidr(Ipv4Address::UNSPECIFIED, 0).into(),
            Some(gateway),
            dev,
            source,
        ))
        .unwrap();
    let route = router.resolve_output_route(&dst, dev.ifindex()).unwrap();
    assert_eq!(route.dev, dev);
    assert_eq!(IpAddress::Ipv4(route.source), source);
    assert_eq!(IpAddress::Ipv4(route.next_hop), gateway);
    assert_eq!(route.mtu, 100);
    assert_eq!(route.link_kind, LinkKind::Ethernet);
    assert_eq!(router.devices.effective_mtu(), 68);
    assert!(router.resolve_output_route(&dst, lo.ifindex()).is_err());
    router
        .devices
        .get_mut_by_id(dev)
        .unwrap()
        .set_link_up(false);
    assert!(router.resolve_output_route(&dst, 0).is_err());
}

fn output_options(dst_addr: Ipv4Address) -> Ipv4SendOptions {
    Ipv4SendOptions {
        bound_src: None,
        dst_addr,
        oif: 0,
        allow_broadcast: false,
        protocol: ipv4::PROTOCOL_UDP,
        ttl: 37,
        fragmentation: Ipv4FragmentationPolicy::Allow,
    }
}

#[def_test]
fn output_preparation_rejects_nonlocal_sources_without_consuming_packet_or_capacity() {
    for dst in [Ipv4Address::new(192, 0, 2, 8), Ipv4Address::BROADCAST] {
        let (mut router, dev) = output_test_router();
        let mut options = output_options(dst);
        options.allow_broadcast = true;
        options.oif = dev.ifindex();
        options.bound_src = Some(Ipv4Address::new(198, 51, 100, 1));
        let available = router.available_tx_packet_slots();
        let mut packet = Ipv4PacketBuilder::new(8).unwrap();
        packet.transport_mut().fill(42);
        let allocation = packet.transport_mut().as_ptr();
        let (error, mut packet) = router.prepare_ipv4_send(options, packet).err().unwrap();
        assert_eq!(error, LinuxError::ENETUNREACH.into());
        assert_eq!(packet.transport_mut().as_ptr(), allocation);
        assert_eq!(packet.transport_mut(), &[42; 8]);
        assert_eq!(router.available_tx_packet_slots(), available);
        options.bound_src = Some(Ipv4Address::new(192, 0, 2, 7));
        router
            .prepare_ipv4_send(options, packet)
            .unwrap()
            .submit()
            .unwrap();
        let packet = router.data_tx_queue.pop_front().unwrap();
        let header = ipv4::Ipv4Header::parse_input(packet.network_packet().unwrap()).unwrap();
        assert_eq!(header.src_addr(), options.bound_src.unwrap());
    }
}

#[def_test]
fn bound_output_source_does_not_require_an_automatic_source_on_the_route() {
    let (mut router, dev, tx_log) = recording_output_router();
    let mut entry = router.first_ipv4_entry(dev).unwrap();
    router.remove_ipv4_addr(dev, entry.addr);
    entry.scope = ROUTE_SCOPE_HOST;
    router.add_ipv4_addr(dev, entry).unwrap();
    let other = add_recording_output_device(&mut router, &tx_log);
    let other_source = router.first_ipv4_entry(other).unwrap().addr.address();
    let destination = Ipv4Address::new(203, 0, 113, 1);
    router
        .add_route_rule(Rule::with_route_attrs(
            valid_ipv4_cidr(destination, 32).into(),
            None,
            dev,
            entry.addr.address().into(),
            RouteAttrs {
                table: ROUTE_TABLE_MAIN,
                protocol: ROUTE_PROTOCOL_KERNEL,
                scope: ROUTE_SCOPE_UNIVERSE,
                route_type: ROUTE_TYPE_UNICAST,
                prefsrc: None,
            },
        ))
        .unwrap();
    let mut options = output_options(destination);
    options.oif = dev.ifindex();
    let (error, _) = router
        .prepare_ipv4_send(options, Ipv4PacketBuilder::new(8).unwrap())
        .err()
        .unwrap();
    assert_eq!(error, LinuxError::ENETUNREACH.into());
    for source in [entry.addr.address(), other_source] {
        options.bound_src = Some(source);
        let send = router
            .prepare_ipv4_send(options, Ipv4PacketBuilder::new(8).unwrap())
            .unwrap();
        assert_eq!(send.source_addr(), source);
        send.submit().unwrap();
        let (count, _) = router.dispatch_budgeted(MonotonicInstant::ORIGIN, 1);
        assert_eq!(count, 1);
        let sent = tx_log.lock().pop().unwrap();
        assert_eq!(sent.0, dev.ifindex());
        let header = ipv4::Ipv4Header::parse_input(sent.2.network_packet().unwrap()).unwrap();
        assert_eq!(header.src_addr(), source);
    }
}

#[def_test]
fn output_preflight_preserves_retry_and_uses_one_mtu_for_fragmentation() {
    let source = Ipv4Address::new(192, 0, 2, 7);
    let dst = Ipv4Address::new(192, 0, 2, 8);
    for capacity in [0, 2, 3] {
        let (mut router, _) = output_test_router();
        router.data_tx_queue = DataTxQueue::with_capacity(capacity);
        let service = crate::runtime::service::Service::new(router);
        let mut packet = Ipv4PacketBuilder::new(200).unwrap();
        packet.transport_mut().fill(42);
        {
            let mut output = service.ipv4_output();
            let result = output.prepare(output_options(dst), packet);
            if capacity < 3 {
                let (error, mut packet) = result.err().unwrap();
                assert_eq!(error, KError::WouldBlock);
                assert_eq!(packet.transport_mut(), &[42; 200]);
            } else {
                let send = result.unwrap();
                assert_eq!(send.source_addr(), source);
                assert_eq!(send.destination_addr(), dst);
                send.submit().unwrap();
            }
        }
        assert_eq!(service.can_send_ip_packet(), capacity == 2);
    }
}

#[def_test]
fn output_capacity_retry_reuses_the_original_payload_allocation() {
    let (mut router, _) = output_test_router();
    router.data_tx_queue = DataTxQueue::with_capacity(3);
    router
        .data_tx_queue
        .try_push(tx_packet_buf(vec![0]))
        .unwrap();
    let mut packet = Ipv4PacketBuilder::new(200).unwrap();
    packet.transport_mut().fill(42);
    let allocation = packet.transport_mut().as_ptr();
    let options = output_options(Ipv4Address::new(192, 0, 2, 8));
    let (error, mut packet) = router.prepare_ipv4_send(options, packet).err().unwrap();
    assert_eq!(error, KError::WouldBlock);
    assert_eq!(packet.transport_mut().as_ptr(), allocation);
    assert_eq!(packet.transport_mut(), &[42; 200]);
    assert_eq!(router.available_tx_packet_slots(), 2);
    router.data_tx_queue.pop_front().unwrap();
    router
        .prepare_ipv4_send(options, packet)
        .unwrap()
        .submit()
        .unwrap();
    for offset in [0, 80, 160] {
        let fragment = router.data_tx_queue.pop_front().unwrap();
        let bytes = fragment.network_packet().unwrap();
        let header = ipv4::Ipv4Header::parse_input(bytes).unwrap();
        assert_eq!(header.src_addr(), Ipv4Address::new(192, 0, 2, 7));
        assert_eq!(header.identification(), 1);
        assert_eq!(header.fragment_offset(), offset);
        assert!(header.total_len() <= 100);
        assert!(bytes[header.header_len()..].iter().all(|byte| *byte == 42));
    }
    assert!(router.data_tx_queue.is_empty());
}

#[def_test]
fn abandoned_output_does_not_occupy_capacity_or_advance_identification() {
    let (mut router, _) = output_test_router();
    let mut options = output_options(Ipv4Address::new(192, 0, 2, 8));
    let available = router.available_tx_packet_slots();
    let send = router
        .prepare_ipv4_send(options, Ipv4PacketBuilder::new(200).unwrap())
        .unwrap();
    drop(send);
    assert_eq!(router.available_tx_packet_slots(), available);
    options.fragmentation = Ipv4FragmentationPolicy::Forbid;
    assert_eq!(
        router
            .prepare_ipv4_send(options, Ipv4PacketBuilder::new(200).unwrap())
            .unwrap()
            .submit(),
        Err(LinuxError::EMSGSIZE.into())
    );
    assert_eq!(router.available_tx_packet_slots(), available);
    options.fragmentation = Ipv4FragmentationPolicy::Allow;
    router
        .prepare_ipv4_send(options, Ipv4PacketBuilder::new(200).unwrap())
        .unwrap()
        .submit()
        .unwrap();
    for offset in [0, 80, 160] {
        let fragment = router.data_tx_queue.pop_front().unwrap();
        let header = ipv4::Ipv4Header::parse_input(fragment.network_packet().unwrap()).unwrap();
        assert_eq!(header.identification(), 1);
        assert_eq!(header.fragment_offset(), offset);
    }
}

#[def_test]
fn fragmentable_output_identifies_datagrams_before_downstream_fragmentation() {
    let (mut router, _) = output_test_router();
    let options = output_options(Ipv4Address::new(192, 0, 2, 8));
    let mut identifications = Vec::new();
    for transport_len in [16, 200, 16] {
        router
            .prepare_ipv4_send(options, Ipv4PacketBuilder::new(transport_len).unwrap())
            .unwrap()
            .submit()
            .unwrap();
        let first = router.data_tx_queue.pop_front().unwrap();
        let bytes = first.network_packet().unwrap();
        let header = ipv4::Ipv4Header::parse_input(bytes).unwrap();
        let identification = header.identification();
        assert!(!identifications.contains(&identification));
        identifications.push(identification);
        if transport_len == 16 {
            assert!(!header.is_fragmented());
            let fragments = ipv4::fragment_output_packet(bytes, 28, identification).unwrap();
            assert_eq!(fragments.len(), 2);
            for fragment in fragments {
                let header = ipv4::Ipv4Header::parse_input(&fragment).unwrap();
                assert_eq!(header.identification(), identification);
            }
        } else {
            while let Some(fragment) = router.data_tx_queue.pop_front() {
                let header =
                    ipv4::Ipv4Header::parse_input(fragment.network_packet().unwrap()).unwrap();
                assert_eq!(header.identification(), identification);
            }
        }
    }
}

#[def_test]
fn output_fragmentation_policy_preserves_mtu_boundary_behavior() {
    use Ipv4FragmentationPolicy::{Allow, Forbid, PreferDontFragment};
    for policy in [Allow, Forbid, PreferDontFragment] {
        for transport_len in [79, 80, 81] {
            let (mut router, _) = output_test_router();
            let mut options = output_options(Ipv4Address::new(192, 0, 2, 8));
            options.fragmentation = policy;
            let available = router.available_tx_packet_slots();
            let result = router
                .prepare_ipv4_send(options, Ipv4PacketBuilder::new(transport_len).unwrap())
                .unwrap()
                .submit();
            if policy == Forbid && transport_len > 80 {
                assert_eq!(result, Err(LinuxError::EMSGSIZE.into()));
                assert_eq!(router.available_tx_packet_slots(), available);
                continue;
            }
            result.unwrap();
            let expected_count = if transport_len > 80 { 2 } else { 1 };
            assert_eq!(
                router.available_tx_packet_slots(),
                available - expected_count
            );
            for _ in 0..expected_count {
                let packet = router.data_tx_queue.pop_front().unwrap();
                let bytes = packet.network_packet().unwrap();
                let header = ipv4::Ipv4Header::parse_input(bytes).unwrap();
                assert_eq!(
                    header.dont_fragment(),
                    policy != Allow && transport_len <= 80
                );
                assert_eq!(header.total_len(), bytes.len());
                assert_eq!(header.protocol(), ipv4::PROTOCOL_UDP);
                assert_eq!(header.bytes()[8], 37);
            }
        }
    }
}

#[def_test]
fn limited_broadcast_with_bound_device_preserves_no_route_output() {
    use Ipv4FragmentationPolicy::{Allow, Forbid, PreferDontFragment};
    for policy in [Allow, Forbid, PreferDontFragment] {
        let (mut router, dev) = output_test_router();
        let addr = router.first_ipv4_entry(dev).unwrap().addr;
        router.remove_ipv4_addr(dev, addr);
        assert!(
            router
                .table
                .lookup(&Ipv4Address::BROADCAST.into())
                .is_none()
        );
        router.data_tx_queue = DataTxQueue::with_capacity(1);
        let mut options = output_options(Ipv4Address::BROADCAST);
        options.oif = dev.ifindex();
        options.fragmentation = policy;
        let packet = Ipv4PacketBuilder::new(200).unwrap();
        let (error, packet) = router.prepare_ipv4_send(options, packet).err().unwrap();
        assert_eq!(error, LinuxError::EACCES.into());
        options.allow_broadcast = true;
        options.oif = 0;
        let (error, packet) = router.prepare_ipv4_send(options, packet).err().unwrap();
        assert_eq!(error, LinuxError::ENETUNREACH.into());
        options.oif = dev.ifindex();
        let send = router.prepare_ipv4_send(options, packet).unwrap();
        assert_eq!(send.source_addr(), Ipv4Address::UNSPECIFIED);
        send.submit().unwrap();
        assert_eq!(router.available_tx_packet_slots(), 0);
        let packet = router.data_tx_queue.pop_front().unwrap();
        let header = ipv4::Ipv4Header::parse_input(packet.network_packet().unwrap()).unwrap();
        assert_eq!(header.src_addr(), Ipv4Address::UNSPECIFIED);
        assert_eq!(header.dst_addr(), Ipv4Address::BROADCAST);
        assert_eq!(header.total_len(), 220);
        assert_eq!(header.dont_fragment(), policy != Allow);
        assert_eq!(header.identification() == 0, policy != Allow);
    }
}

#[def_test(serial)]
fn resolved_loopback_output_bypasses_full_tx_queue() {
    let mut router = Router::new();
    let lo = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let addr = Ipv4Address::new(127, 0, 0, 1);
    router
        .add_ipv4_addr(
            lo,
            Ipv4AddrEntry {
                addr: valid_ipv4_cidr(addr, 8),
                scope: ROUTE_SCOPE_HOST,
                broadcast: None,
            },
        )
        .unwrap();
    router.data_tx_queue = DataTxQueue::with_capacity(0);
    let service = crate::runtime::service::Service::new(router);
    assert!(!service.can_send_ip_packet());
    {
        let mut output = service.ipv4_output();
        let mut options = output_options(addr);
        options.oif = lo.ifindex();
        options.protocol = ipv4::PROTOCOL_ICMP;
        let send = output
            .prepare(options, Ipv4PacketBuilder::new(0).unwrap())
            .unwrap();
        assert_eq!(send.source_addr(), addr);
        send.submit().unwrap();
    }
    let progress = service.poll_budgeted(crate::poller::PollBudget {
        rx_packets: 1,
        tx_packets: 1,
        timer_events: 1,
    });
    assert_eq!(progress.rx_packets, 1);
}

fn add_recording_output_device(router: &mut Router, tx_log: &TxLog) -> NetDeviceId {
    let dev = router
        .add_device(kalloc_types::KBox::new(FailingRxDevice {
            link: LoopbackDevice::new(),
            packet: None,
            tx_log: Some(tx_log.clone()),
            up: true,
}).unwrap())
        .unwrap();
    router
        .add_ipv4_addr(
            dev,
            Ipv4AddrEntry {
                addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 9), 24),
                scope: ROUTE_SCOPE_UNIVERSE,
                broadcast: None,
            },
        )
        .unwrap();
    dev
}

#[def_test]
fn queued_output_rechecks_routing_and_binding_without_reselecting_source() {
    for is_bound in [false, true] {
        let (mut router, first_dev, tx_log) = recording_output_router();
        let dst = Ipv4Address::new(192, 0, 2, 8);
        let mut options = output_options(dst);
        if is_bound {
            options.oif = first_dev.ifindex();
        }
        router
            .prepare_ipv4_send(options, Ipv4PacketBuilder::new(8).unwrap())
            .unwrap()
            .submit()
            .unwrap();
        let second_dev = add_recording_output_device(&mut router, &tx_log);
        let gateway = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 1));
        router
            .add_route_rule(Rule::new(
                valid_ipv4_cidr(dst, 32).into(),
                Some(gateway),
                second_dev,
                Ipv4Address::new(192, 0, 2, 9).into(),
            ))
            .unwrap();
        let (count, _) = router.dispatch_budgeted(MonotonicInstant::ORIGIN, 1);
        assert_eq!(count, 1);
        let sent = tx_log.lock();
        if is_bound {
            assert!(sent.is_empty());
        } else {
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0].0, second_dev.ifindex());
            assert_eq!(sent[0].1, gateway);
            let header =
                ipv4::Ipv4Header::parse_input(sent[0].2.network_packet().unwrap()).unwrap();
            assert_eq!(header.src_addr(), Ipv4Address::new(192, 0, 2, 7));
            assert_eq!(header.dst_addr(), dst);
        }
    }
}

#[def_test]
fn queued_output_rejects_removed_source_and_missing_route_device() {
    for has_removed_device in [false, true] {
        let (mut router, dev, tx_log) = recording_output_router();
        let source = router.first_ipv4_entry(dev).unwrap().addr;
        router
            .add_ipv4_addr(
                dev,
                Ipv4AddrEntry {
                    addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 9), 24),
                    scope: ROUTE_SCOPE_UNIVERSE,
                    broadcast: None,
                },
            )
            .unwrap();
        let mut options = output_options(Ipv4Address::new(192, 0, 2, 8));
        options.bound_src = Some(source.address());
        router
            .prepare_ipv4_send(options, Ipv4PacketBuilder::new(8).unwrap())
            .unwrap()
            .submit()
            .unwrap();
        if has_removed_device {
            // Leave the old route in place to exercise the dispatch boundary too.
            let slot = router.devices.slot(dev).unwrap();
            drop(router.devices.remove_slot(slot));
        } else {
            router.remove_ipv4_addr(dev, source);
            assert!(router.table.lookup(&options.dst_addr.into()).is_some());
            let (error, _) = router
                .prepare_ipv4_send(options, Ipv4PacketBuilder::new(8).unwrap())
                .err()
                .unwrap();
            assert_eq!(error, LinuxError::ENETUNREACH.into());
        }
        let (count, _) = router.dispatch_budgeted(MonotonicInstant::ORIGIN, 1);
        assert_eq!(count, 1);
        assert!(tx_log.lock().is_empty());
    }
}

#[def_test]
fn queued_broadcast_fanout_obeys_binding() {
    for is_bound in [false, true] {
        let (mut router, first_dev, tx_log) = recording_output_router();
        let second_dev = add_recording_output_device(&mut router, &tx_log);
        router
            .add_route_rule(Rule::new(
                valid_ipv4_cidr(Ipv4Address::BROADCAST, 32).into(),
                None,
                first_dev,
                Ipv4Address::new(192, 0, 2, 7).into(),
            ))
            .unwrap();
        let mut options = output_options(Ipv4Address::BROADCAST);
        options.allow_broadcast = true;
        if is_bound {
            options.oif = first_dev.ifindex();
        }
        router
            .prepare_ipv4_send(options, Ipv4PacketBuilder::new(8).unwrap())
            .unwrap()
            .submit()
            .unwrap();
        let (count, _) = router.dispatch_budgeted(MonotonicInstant::ORIGIN, 1);
        assert_eq!(count, 1);
        let sent = tx_log.lock();
        if is_bound {
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0].0, first_dev.ifindex());
        } else {
            assert_eq!(sent.len(), 2);
            assert_eq!(sent[0].0, first_dev.ifindex());
            assert_eq!(sent[1].0, second_dev.ifindex());
        }
    }
}
