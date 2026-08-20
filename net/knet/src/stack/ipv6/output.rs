// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv6 output dispatch, local delivery, and NDP/ICMPv6 actions.

use alloc::vec::Vec;

use kerrno::{KError, KResult};
use ktime_types::MonotonicInstant;

use super::{
    icmpv6, ndp,
    packet::{Ipv6Error, Ipv6Header},
};
use crate::{
    buf::{PacketBuf, PacketOwner},
    ip::{IpAddress, Ipv6Address, Ipv6Cidr},
    router::Router,
    wire::MacAddress,
};

/// Router-side work derived from an inbound IPv6 packet.
pub(crate) enum Ipv6Action {
    LearnNeighbor {
        ifindex: i32,
        addr: Ipv6Address,
        mac: MacAddress,
    },
    ReplyAdvertisement {
        ifindex: i32,
        target: Ipv6Address,
        dst: Ipv6Address,
        solicited: bool,
    },
    EchoReply {
        ifindex: i32,
        header: Ipv6Header,
        packet: Vec<u8>,
    },
}

pub(crate) fn queue_ipv6_packet(router: &mut Router, packet: Vec<u8>, ifindex: i32) -> KResult {
    if !router.can_enqueue_tx_packet() {
        return Err(KError::WouldBlock);
    }
    router.push_data_tx_packet(PacketBuf::from_ip_packet_vec(
        ifindex,
        packet,
        PacketOwner::Ipv6Stack,
    ));
    Ok(())
}

pub(crate) fn dispatch_ipv6_packet(
    router: &mut Router,
    mut packet: PacketBuf,
    timestamp: MonotonicInstant,
) -> bool {
    let header = match Ipv6Header::prepare_output_packet(&mut packet) {
        Ok(header) => header,
        Err(Ipv6Error::Malformed | Ipv6Error::Fragmented) => return false,
    };

    let src_addr = IpAddress::Ipv6(header.src_addr());
    let dst_addr = IpAddress::Ipv6(header.dst_addr());
    if !is_local_ipv6_source(&router.local_ipv6_addrs(), header.src_addr())
        || header.src_addr().is_multicast()
    {
        warn!("Dropping IPv6 packet with non-local source {}", src_addr);
        return false;
    }

    if let Some(dev_index) = router.local_ipv6_device_index(header.dst_addr()) {
        packet.set_ifindex(dev_index as i32 + 1);
        packet.set_owner(PacketOwner::DeviceRx);
        if !router.try_enqueue_local_ipv6_packet(packet) {
            return false;
        }
        return true;
    }

    if dst_addr.is_multicast() {
        if let Some(dev_index) = output_device_index(packet.ifindex(), router.devices.len()) {
            let dev = &mut router.devices[dev_index];
            return dev.send_ip_packet(dev_index as i32 + 1, dst_addr, src_addr, packet, timestamp);
        }
        let device_count = router.devices.len();
        let Some((last_device, preceding_devices)) = router.devices.split_last_mut() else {
            return false;
        };

        let mut poll_next = false;
        for (dev_index, dev) in preceding_devices.iter_mut().enumerate() {
            poll_next |= dev.send_ip_packet(
                dev_index as i32 + 1,
                dst_addr,
                src_addr,
                packet.clone(),
                timestamp,
            );
        }
        poll_next |=
            last_device.send_ip_packet(device_count as i32, dst_addr, src_addr, packet, timestamp);
        return poll_next;
    }

    if header.dst_addr().is_link_local() {
        let Some(dev_index) = link_local_output_device(router, packet.ifindex()) else {
            warn!("No link-local capable device for destination: {}", dst_addr);
            return false;
        };
        let next_hop = dst_addr;
        let dev = &mut router.devices[dev_index];
        return dev.send_ip_packet(dev_index as i32 + 1, next_hop, src_addr, packet, timestamp);
    }

    let Some(rule) = router.table.lookup(&dst_addr) else {
        warn!("No route found for destination: {}", dst_addr);
        return false;
    };

    let next_hop = rule.via.unwrap_or(dst_addr);
    let dev = &mut router.devices[rule.dev];
    dev.send_ip_packet(rule.dev as i32 + 1, next_hop, src_addr, packet, timestamp)
}

pub(crate) fn echo_reply_source(
    router: &Router,
    request_header: Ipv6Header,
    ifindex: i32,
) -> Option<Ipv6Address> {
    let request_dst = request_header.dst_addr();
    if !request_dst.is_multicast() {
        return Some(request_dst);
    }
    // RFC 4443 §4.2: a multicast Echo Reply uses a unicast address of the
    // receiving interface. Linux `icmpv6_echo_reply` clears saddr so
    // `ipv6_dev_get_saddr` runs against `icmp6_iif`, not a unicast prefsrc.
    let dev = router.devices.get(ifindex.saturating_sub(1) as usize)?;
    let addr = dev.ipv6_echo_source()?;
    (!addr.is_unspecified() && !addr.is_multicast()).then_some(addr)
}

pub(crate) fn apply_actions(router: &mut Router, actions: &mut Vec<Ipv6Action>) {
    for action in actions.drain(..) {
        match action {
            Ipv6Action::LearnNeighbor { ifindex, addr, mac } => {
                if let Some(dev) = router.devices.get_mut(ifindex.saturating_sub(1) as usize) {
                    dev.learn_neighbor(IpAddress::Ipv6(addr), mac);
                }
            }
            Ipv6Action::ReplyAdvertisement {
                ifindex,
                target,
                dst,
                solicited,
            } => {
                let mac = router
                    .devices
                    .get(ifindex.saturating_sub(1) as usize)
                    .and_then(|dev| dev.hardware_addr());
                if let Some(mac) = mac
                    && let Some(na) =
                        ndp::build_neighbor_advertisement(target, dst, target, mac, solicited)
                    && let Err(err) = router.queue_control_ip_packet(na)
                {
                    warn!("Failed to queue IPv6 neighbor advertisement: {}", err);
                }
            }
            Ipv6Action::EchoReply {
                ifindex,
                header,
                packet,
            } => {
                let Some(reply_src) = echo_reply_source(router, header, ifindex) else {
                    continue;
                };
                if let Some(reply) = icmpv6::build_echo_reply(header, &packet, reply_src)
                    && let Err(err) = router.queue_control_ip_packet(reply)
                {
                    warn!("Failed to queue ICMPv6 echo reply: {}", err);
                }
            }
        }
    }
}

fn is_local_ipv6_source(local_addrs: &[Ipv6Cidr], src_addr: Ipv6Address) -> bool {
    local_addrs.iter().any(|addr| addr.address() == src_addr)
}

fn output_device_index(ifindex: i32, device_count: usize) -> Option<usize> {
    (ifindex > 0)
        .then_some(ifindex.saturating_sub(1) as usize)
        .filter(|&dev| dev < device_count)
}

fn link_local_output_device(router: &Router, ifindex: i32) -> Option<usize> {
    if let Some(dev) = output_device_index(ifindex, router.devices.len()) {
        return router.devices[dev]
            .ipv6_link_local()
            .is_some()
            .then_some(dev);
    }
    router
        .devices
        .iter()
        .position(|dev| dev.ipv6_link_local().is_some())
}

#[cfg(unittest)]
mod tests {
    use alloc::boxed::Box;

    use unittest::{assert_eq, def_test};

    use super::*;
    use crate::{
        device::LoopbackDevice,
        ipv6::{PROTOCOL_ICMPV6, build_ipv6_packet},
        router::{Ipv6AddrEntry, ROUTE_SCOPE_HOST, ROUTE_SCOPE_UNIVERSE},
    };

    fn global_unicast(octet: u8) -> Ipv6Address {
        Ipv6Address::from_octets([
            0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, octet,
        ])
    }

    #[def_test]
    fn multicast_echo_reply_uses_receiving_interface_source() {
        let mut router = Router::new();
        let lo = router.add_device(Box::new(LoopbackDevice::new()));
        let gua = global_unicast(1);
        let pinger = global_unicast(2);
        router
            .add_ipv6_addr(Ipv6AddrEntry {
                dev: lo,
                addr: Ipv6Cidr::new(gua, 64),
                scope: ROUTE_SCOPE_UNIVERSE,
            })
            .unwrap();
        router
            .add_ipv6_addr(Ipv6AddrEntry {
                dev: lo,
                addr: Ipv6Cidr::new(Ipv6Address::LOCALHOST, 128),
                scope: ROUTE_SCOPE_HOST,
            })
            .unwrap();

        assert_eq!(
            router
                .table
                .lookup(&IpAddress::Ipv6(pinger))
                .map(|rule| rule.src),
            Some(IpAddress::Ipv6(gua))
        );

        let packet = build_ipv6_packet(
            pinger,
            Ipv6Address::ALL_NODES_LINK_LOCAL,
            PROTOCOL_ICMPV6,
            64,
            &[],
        )
        .unwrap();
        let header = Ipv6Header::parse_input(&packet).unwrap();
        assert_eq!(
            echo_reply_source(&router, header, lo as i32 + 1),
            Some(Ipv6Address::LOCALHOST)
        );
    }
}
