// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Existing smoltcp IPv6 output adaptation.
use ktime_types::MonotonicInstant;
use smoltcp::wire::Ipv6Packet;

use crate::{buf::PacketBuf, ip::IpAddress, network::state::NetworkState};

/// Returns the unsent packet on route or interface contention.
/// Successful dispatch returns the existing device RX-ready hint.
pub(crate) fn dispatch(
    state: &NetworkState,
    packet: PacketBuf,
    timestamp: MonotonicInstant,
) -> Result<bool, PacketBuf> {
    let Some(ip_packet) = packet.network_packet() else {
        return Ok(false);
    };
    let (src_addr, dst_addr, is_multicast) = {
        let ip_packet = Ipv6Packet::new_checked(ip_packet).expect("got invalid IPv6 packet");
        (
            IpAddress::Ipv6(ip_packet.src_addr().into()),
            IpAddress::Ipv6(ip_packet.dst_addr().into()),
            ip_packet.dst_addr().is_multicast(),
        )
    };
    if is_multicast {
        let Some(mut interfaces) = state.try_write_interfaces() else {
            return Err(packet);
        };
        let mut poll_next = false;
        let device_count = interfaces.devices().len();
        for dev_index in 0..device_count {
            poll_next |= interfaces.devices_mut()[dev_index].send_ip_packet(
                dev_index as i32 + 1,
                dst_addr,
                src_addr,
                packet.clone(),
                timestamp,
            );
        }
        return Ok(poll_next);
    }

    let Some(route) = state.try_resolve(&dst_addr) else {
        return Err(packet);
    };
    let Some(route) = route else {
        warn!("No route found for destination: {}", dst_addr);
        return Ok(false);
    };
    assert_eq!(route.source, src_addr);
    state.try_submit_ip_packet(route, src_addr, packet, timestamp)
}

#[cfg(unittest)]
mod tests {
    use alloc::{boxed::Box, vec, vec::Vec};

    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::{
        buf::PacketOwner, datapath::NetIo, device::LoopbackDevice, network::state::NetworkState,
    };
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
    #[def_test]
    fn ipv6_multicast_returns_packet_before_contended_fanout() {
        let state = NetworkState::new();
        let original = packet(true);
        let returned = state.with_interfaces(|_| {
            dispatch(&state, original.clone(), MonotonicInstant::ORIGIN).unwrap_err()
        });
        assert_eq!(returned.network_packet(), original.network_packet());
    }

    #[def_test(serial)]
    fn ipv6_multicast_reaches_each_device() {
        let state = NetworkState::new();
        state.add_device(Box::new(LoopbackDevice::new()));
        state.add_device(Box::new(LoopbackDevice::new()));
        let mut io = NetIo::new();
        assert!(dispatch(&state, packet(true), MonotonicInstant::ORIGIN).unwrap());
        let mut packets = Vec::new();
        let drain = state.with_interfaces_mut(|interfaces| {
            io.drain_device_rx_budgeted(interfaces, MonotonicInstant::ORIGIN, 4, &mut packets)
        });
        assert_eq!(drain.work_done, 2);
        assert_eq!(packets[0].ifindex(), 1);
        assert_eq!(packets[1].ifindex(), 2);
    }
    #[def_test(serial)]
    fn ipv6_unicast_without_route_does_not_submit() {
        let state = NetworkState::new();
        state.add_device(Box::new(LoopbackDevice::new()));
        let mut io = NetIo::new();
        assert!(!dispatch(&state, packet(false), MonotonicInstant::ORIGIN).unwrap());
        let mut packets = Vec::new();
        let drain = state.with_interfaces_mut(|interfaces| {
            io.drain_device_rx_budgeted(interfaces, MonotonicInstant::ORIGIN, 1, &mut packets)
        });
        assert_eq!(drain.work_done, 0);
    }
}
