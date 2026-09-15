// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Existing smoltcp IPv6 output adaptation.
use ktime_types::MonotonicInstant;
use smoltcp::wire::Ipv6Packet;

use crate::{buf::PacketBuf, datapath::NetIo, ip::IpAddress, network::state::NetworkState};
pub(crate) fn dispatch(
    state: &mut NetworkState,
    io: &mut NetIo,
    packet: PacketBuf,
    timestamp: MonotonicInstant,
) -> bool {
    let Some(ip_packet) = packet.network_packet() else {
        return false;
    };
    let mut poll_next = false;
    let (src_addr, dst_addr, is_multicast) = {
        let ip_packet = Ipv6Packet::new_checked(ip_packet).expect("got invalid IPv6 packet");
        (
            IpAddress::Ipv6(ip_packet.src_addr().into()),
            IpAddress::Ipv6(ip_packet.dst_addr().into()),
            ip_packet.dst_addr().is_multicast(),
        )
    };
    if is_multicast {
        for dev_index in 0..state.interfaces.devices().len() {
            poll_next |= io.submit_ip_packet(
                &mut state.interfaces,
                dev_index,
                dst_addr,
                src_addr,
                packet.clone(),
                timestamp,
            );
        }
    } else {
        let Some(rule) = state.routing.lookup(&dst_addr).copied() else {
            warn!("No route found for destination: {}", dst_addr);
            return false;
        };
        assert_eq!(rule.src, src_addr);

        let next_hop = rule.via.unwrap_or(dst_addr);
        poll_next |= io.submit_ip_packet(
            &mut state.interfaces,
            rule.dev,
            next_hop,
            src_addr,
            packet,
            timestamp,
        );
    }
    poll_next
}

#[cfg(unittest)]
mod tests {
    use alloc::{boxed::Box, vec, vec::Vec};

    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::{buf::PacketOwner, device::LoopbackDevice};
    fn packet(multicast: bool) -> PacketBuf {
        let mut bytes = vec![0; 40];
        bytes[0] = 0x60;
        bytes[6] = 59;
        bytes[7] = 64;
        bytes[23] = 1;
        bytes[24] = if multicast { 0xff } else { 0x20 };
        bytes[25] = 2;
        bytes[39] = 1;
        PacketBuf::from_ip_packet_vec(0, bytes, PacketOwner::Ipv4Stack)
    }
    #[def_test(serial)]
    fn ipv6_multicast_reaches_each_device() {
        let mut state = NetworkState::new();
        state.interfaces.add_device(Box::new(LoopbackDevice::new()));
        state.interfaces.add_device(Box::new(LoopbackDevice::new()));
        let mut io = NetIo::new();
        assert!(dispatch(
            &mut state,
            &mut io,
            packet(true),
            MonotonicInstant::ORIGIN
        ));
        let mut packets = Vec::new();
        let drain = io.drain_device_rx_budgeted(
            &mut state.interfaces,
            MonotonicInstant::ORIGIN,
            4,
            &mut packets,
        );
        assert_eq!(drain.work_done, 2);
        assert_eq!(packets[0].ifindex(), 1);
        assert_eq!(packets[1].ifindex(), 2);
    }
    #[def_test(serial)]
    fn ipv6_unicast_without_route_does_not_submit() {
        let mut state = NetworkState::new();
        state.interfaces.add_device(Box::new(LoopbackDevice::new()));
        let mut io = NetIo::new();
        assert!(!dispatch(
            &mut state,
            &mut io,
            packet(false),
            MonotonicInstant::ORIGIN
        ));
        let mut packets = Vec::new();
        let drain = io.drain_device_rx_budgeted(
            &mut state.interfaces,
            MonotonicInstant::ORIGIN,
            1,
            &mut packets,
        );
        assert_eq!(drain.work_done, 0);
    }
}
