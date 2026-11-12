// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ::core::net::Ipv4Addr;
use etherparse::UdpHeader;
use kerrno::{KError, KResult, LinuxError};

use super::UDP_HEADER_LEN;
use crate::{
    ip::{IpAddress, Ipv4Address},
    ipv4,
};

/// Precomputed 16-bit ones' complement sum of a UDP payload.
///
/// Computed outside the Router mutex after reading user data so that finishing
/// the UDP header under the lock only folds in the 8-byte IPv4 pseudo-header
/// addresses and UDP header fields in $O(1)$ time, and retries after
/// `WouldBlock` do not rescan the payload.
#[derive(Clone, Copy, Debug)]
pub(super) struct UdpPayloadChecksum {
    sum: u64,
    payload_len: usize,
}

impl UdpPayloadChecksum {
    pub(super) fn compute(payload: &[u8]) -> Self {
        let mut sum = 0u64;
        let mut chunks = payload.chunks_exact(2);
        for chunk in &mut chunks {
            sum += u64::from(u16::from_be_bytes([chunk[0], chunk[1]]));
        }
        if let &[last] = chunks.remainder() {
            sum += u64::from(u16::from_be_bytes([last, 0]));
        }
        Self {
            sum,
            payload_len: payload.len(),
        }
    }

    fn finish(
        self,
        src_addr: Ipv4Address,
        dst_addr: Ipv4Address,
        source_port: u16,
        destination_port: u16,
    ) -> Option<u16> {
        let udp_len = u16::try_from(self.payload_len.checked_add(UDP_HEADER_LEN)?).ok()?;
        let src = src_addr.octets();
        let dst = dst_addr.octets();
        let mut sum = self.sum
            + u64::from(u16::from_be_bytes([src[0], src[1]]))
            + u64::from(u16::from_be_bytes([src[2], src[3]]))
            + u64::from(u16::from_be_bytes([dst[0], dst[1]]))
            + u64::from(u16::from_be_bytes([dst[2], dst[3]]))
            + u64::from(ipv4::PROTOCOL_UDP)
            + u64::from(udp_len)
            + u64::from(source_port)
            + u64::from(destination_port)
            + u64::from(udp_len);
        while (sum >> 16) != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        let checksum = !(sum as u16);
        Some(if checksum == 0 { 0xffff } else { checksum })
    }
}

#[cfg(unittest)]
pub(super) fn write_udp_header(
    udp_packet: &mut [u8],
    src_addr: Ipv4Address,
    dst_addr: Ipv4Address,
    source_port: u16,
    destination_port: u16,
) -> Option<()> {
    let payload_checksum = UdpPayloadChecksum::compute(udp_packet.get(UDP_HEADER_LEN..)?);
    write_udp_header_with_checksum(
        udp_packet,
        payload_checksum,
        src_addr,
        dst_addr,
        source_port,
        destination_port,
    )
}

pub(super) fn write_udp_header_with_checksum(
    udp_packet: &mut [u8],
    payload_checksum: UdpPayloadChecksum,
    src_addr: Ipv4Address,
    dst_addr: Ipv4Address,
    source_port: u16,
    destination_port: u16,
) -> Option<()> {
    let payload = udp_packet.get(UDP_HEADER_LEN..)?;
    if payload.len() != payload_checksum.payload_len {
        return None;
    }
    let Ok(mut header) =
        UdpHeader::without_ipv4_checksum(source_port, destination_port, payload.len())
    else {
        return None;
    };
    header.checksum = payload_checksum.finish(src_addr, dst_addr, source_port, destination_port)?;
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

pub(super) fn ipv4_to_core(addr: Ipv4Address) -> Ipv4Addr {
    let octets = addr.octets();
    Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3])
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

#[cfg(unittest)]
pub(super) fn read_u16_be(data: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([data[offset], data[offset + 1]])
}
