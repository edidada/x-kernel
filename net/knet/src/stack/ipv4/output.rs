// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 output dispatch and fragmentation.

use alloc::{vec, vec::Vec};

use kerrno::{KError, KResult, LinuxError};
use ktime_types::MonotonicInstant;

use super::packet::{
    IPV4_MIN_HEADER_LEN, Ipv4Error, Ipv4FragmentError, Ipv4Header, fragment_output_packet,
};
use crate::{
    buf::{PacketBuf, PacketOwner},
    ip::{IpAddress, Ipv4Address, Ipv4Cidr},
    router::Router,
};

pub(crate) fn queue_ipv4_packet(router: &mut Router, packet: Vec<u8>) -> KResult {
    let packets = fragment_ipv4_packet_for_output(router, packet)?;
    if !router.can_enqueue_tx_packets(packets.len()) {
        return Err(KError::WouldBlock);
    }

    router.extend_data_tx_packets(
        packets
            .into_iter()
            .map(|packet| PacketBuf::from_ip_packet_vec(0, packet, PacketOwner::Ipv4Stack)),
    );
    Ok(())
}

pub(crate) fn dispatch_ipv4_packet(
    router: &mut Router,
    mut packet: PacketBuf,
    timestamp: MonotonicInstant,
) -> bool {
    let header = match Ipv4Header::prepare_output_packet(&mut packet) {
        Ok(header) => header,
        Err(Ipv4Error::Malformed | Ipv4Error::BadChecksum) => return false,
    };

    let src_addr = IpAddress::Ipv4(header.src_addr());
    let dst_addr = IpAddress::Ipv4(header.dst_addr());
    if header.dst_addr().is_broadcast() {
        if !is_valid_ipv4_broadcast_source(&router.local_ipv4_addrs(), header.src_addr()) {
            warn!("Dropping IPv4 broadcast packet with source {}", src_addr);
            return false;
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

    let Some(rule) = router.table.lookup(&dst_addr) else {
        warn!("No route found for destination: {}", dst_addr);
        return false;
    };
    if !is_local_ipv4_source(&router.local_ipv4_addrs(), header.src_addr()) {
        warn!(
            "Dropping IPv4 packet with non-local source {} routed via {}",
            src_addr, rule.src
        );
        return false;
    }

    let next_hop = rule.via.unwrap_or(dst_addr);
    let dev = &mut router.devices[rule.dev];
    dev.send_ip_packet(rule.dev as i32 + 1, next_hop, src_addr, packet, timestamp)
}

fn fragment_ipv4_packet_for_output(router: &mut Router, packet: Vec<u8>) -> KResult<Vec<Vec<u8>>> {
    let header = Ipv4Header::parse_output(&packet).map_err(|_| KError::InvalidInput)?;
    let dst_addr = IpAddress::Ipv4(header.dst_addr());
    let Some(mtu) = router.route_mtu(&dst_addr) else {
        return Ok(vec![packet]);
    };
    if packet.len() <= mtu {
        return Ok(vec![packet]);
    }

    let identification = router.next_ipv4_identification();
    fragment_output_packet(&packet, mtu, identification).map_err(|err| match err {
        Ipv4FragmentError::Malformed => KError::InvalidInput,
        Ipv4FragmentError::DontFragment | Ipv4FragmentError::MtuTooSmall => {
            LinuxError::EMSGSIZE.into()
        }
    })
}

fn is_local_ipv4_source(local_addrs: &[Ipv4Cidr], src_addr: Ipv4Address) -> bool {
    local_addrs.iter().any(|addr| addr.address() == src_addr)
}

fn is_valid_ipv4_broadcast_source(local_addrs: &[Ipv4Cidr], src_addr: Ipv4Address) -> bool {
    src_addr.is_unspecified() || is_local_ipv4_source(local_addrs, src_addr)
}

pub(crate) fn output_packet_count(
    router: &Router,
    dst_addr: &IpAddress,
    packet_len: usize,
) -> KResult<usize> {
    if packet_len > u16::MAX as usize {
        return Err(LinuxError::EMSGSIZE.into());
    }

    let Some(mtu) = router.route_mtu(dst_addr) else {
        return Ok(1);
    };
    if packet_len <= mtu {
        return Ok(1);
    }

    let payload_len = packet_len
        .checked_sub(IPV4_MIN_HEADER_LEN)
        .ok_or_else(|| KError::from(LinuxError::EMSGSIZE))?;
    let max_fragment_payload_len = mtu
        .checked_sub(IPV4_MIN_HEADER_LEN)
        .map(|len| len / 8 * 8)
        .filter(|len| *len > 0)
        .ok_or_else(|| KError::from(LinuxError::EMSGSIZE))?;
    Ok(payload_len.div_ceil(max_fragment_payload_len))
}
