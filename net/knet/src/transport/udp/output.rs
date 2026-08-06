// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ::core::net::{Ipv4Addr, Ipv6Addr};
use etherparse::UdpHeader;
use kerrno::{KError, KResult, LinuxError};

use super::UDP_HEADER_LEN;
use crate::ip::{IpAddress, Ipv4Address, Ipv6Address};

pub(super) fn write_udp_header(
    udp_packet: &mut [u8],
    src_addr: Ipv4Address,
    dst_addr: Ipv4Address,
    source_port: u16,
    destination_port: u16,
) -> Option<()> {
    let payload = udp_packet.get(UDP_HEADER_LEN..)?;
    let Ok(mut header) =
        UdpHeader::without_ipv4_checksum(source_port, destination_port, payload.len())
    else {
        return None;
    };
    let Ok(checksum) = header.calc_checksum_ipv4_raw(src_addr.octets(), dst_addr.octets(), payload)
    else {
        return None;
    };
    header.checksum = checksum;
    udp_packet
        .get_mut(..UDP_HEADER_LEN)?
        .copy_from_slice(&header.to_bytes());
    Some(())
}

/// Writes a UDP header with a mandatory IPv6 pseudo-header checksum.
pub(super) fn write_udp_header_v6(
    udp_packet: &mut [u8],
    src_addr: Ipv6Address,
    dst_addr: Ipv6Address,
    source_port: u16,
    destination_port: u16,
) -> Option<()> {
    let payload = udp_packet.get(UDP_HEADER_LEN..)?;
    let Ok(mut header) =
        UdpHeader::without_ipv4_checksum(source_port, destination_port, payload.len())
    else {
        return None;
    };
    let Ok(checksum) = header.calc_checksum_ipv6_raw(src_addr.octets(), dst_addr.octets(), payload)
    else {
        return None;
    };
    // IPv6 forbids a zero UDP checksum; a computed zero becomes 0xffff.
    header.checksum = if checksum == 0 { 0xffff } else { checksum };
    udp_packet
        .get_mut(..UDP_HEADER_LEN)?
        .copy_from_slice(&header.to_bytes());
    Some(())
}

pub(super) fn ipv4_pair(src: IpAddress, dst: IpAddress) -> KResult<(Ipv4Address, Ipv4Address)> {
    match (src, dst) {
        (IpAddress::Ipv4(src), IpAddress::Ipv4(dst)) => Ok((src, dst)),
        _ => Err(KError::from(LinuxError::EAFNOSUPPORT)),
    }
}

pub(super) fn ipv6_pair(src: IpAddress, dst: IpAddress) -> KResult<(Ipv6Address, Ipv6Address)> {
    match (src, dst) {
        (IpAddress::Ipv6(src), IpAddress::Ipv6(dst)) => Ok((src, dst)),
        _ => Err(KError::from(LinuxError::EAFNOSUPPORT)),
    }
}

pub(super) fn ipv4_to_core(addr: Ipv4Address) -> Ipv4Addr {
    let octets = addr.octets();
    Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3])
}

pub(super) fn ipv6_to_core(addr: Ipv6Address) -> Ipv6Addr {
    Ipv6Addr::from(addr.octets())
}

pub(super) fn has_valid_udp_checksum(
    src_addr: Ipv4Address,
    dst_addr: Ipv4Address,
    udp_packet: &[u8],
) -> bool {
    let Ok((header, payload)) = UdpHeader::from_slice(udp_packet) else {
        return false;
    };
    header.checksum == 0
        || header
            .calc_checksum_ipv4_raw(src_addr.octets(), dst_addr.octets(), payload)
            .ok()
            == Some(header.checksum)
}

/// Validates a UDP checksum over IPv6. A zero checksum is always rejected.
pub(super) fn has_valid_udp_checksum_v6(
    src_addr: Ipv6Address,
    dst_addr: Ipv6Address,
    udp_packet: &[u8],
) -> bool {
    let Ok((header, payload)) = UdpHeader::from_slice(udp_packet) else {
        return false;
    };
    if header.checksum == 0 {
        return false;
    }
    header
        .calc_checksum_ipv6_raw(src_addr.octets(), dst_addr.octets(), payload)
        .ok()
        == Some(header.checksum)
}

#[cfg(unittest)]
pub(super) fn read_u16_be(data: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([data[offset], data[offset + 1]])
}
