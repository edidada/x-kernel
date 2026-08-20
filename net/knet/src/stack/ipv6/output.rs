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

pub(crate) fn queue_ipv6_packet(router: &mut Router, packet: Vec<u8>) -> KResult {
    if !router.can_enqueue_tx_packet() {
        return Err(KError::WouldBlock);
    }
    router.push_data_tx_packet(PacketBuf::from_ip_packet_vec(
        0,
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
        && !header.src_addr().is_multicast()
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
        let Some(dev_index) = router
            .devices
            .iter()
            .position(|dev| dev.ipv6_link_local().is_some())
        else {
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
    if let Some(IpAddress::Ipv6(addr)) = router
        .table
        .lookup(&IpAddress::Ipv6(request_header.src_addr()))
        .map(|rule| rule.src)
        && !addr.is_unspecified()
        && !addr.is_multicast()
    {
        return Some(addr);
    }
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
                {
                    let _ = router.queue_control_ip_packet(na);
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
