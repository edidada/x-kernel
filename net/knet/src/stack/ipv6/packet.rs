// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv6 packet validation and output helpers.

#[cfg(unittest)]
use alloc::{vec, vec::Vec};

use etherparse::{IpNumber, Ipv6Header as EtherIpv6Header, Ipv6HeaderSlice};

use crate::{
    buf::{ChecksumState, PacketBuf},
    ip::Ipv6Address,
};

pub(crate) const PROTOCOL_ICMPV6: u8 = 58;
pub(crate) const PROTOCOL_TCP: u8 = 6;
pub(crate) const PROTOCOL_UDP: u8 = 17;

pub(crate) const IPV6_HEADER_LEN: usize = 40;
pub(crate) const IPV6_DEFAULT_HOP_LIMIT: u8 = 64;

pub(crate) const IPV6_EXT_HOP_BY_HOP: u8 = 0;
const IPV6_EXT_ROUTING: u8 = 43;
pub(crate) const IPV6_EXT_FRAGMENT: u8 = 44;
const IPV6_EXT_DEST_OPTIONS: u8 = 60;
const IPV6_EXT_AH: u8 = 51;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Ipv6Error {
    Malformed,
    Fragmented,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Ipv6Header {
    src_addr: Ipv6Address,
    dst_addr: Ipv6Address,
    next_header: u8,
    hop_limit: u8,
    /// Offset from the start of the IPv6 packet to the upper-layer header.
    transport_offset: usize,
    /// Total length of the IPv6 packet including the base header.
    total_len: usize,
    traffic_class: u8,
}

impl Ipv6Header {
    pub(crate) fn parse_input(packet: &[u8]) -> Result<Self, Ipv6Error> {
        Self::parse(packet, true)
    }

    pub(crate) fn parse_output(packet: &[u8]) -> Result<Self, Ipv6Error> {
        Self::parse(packet, false)
    }

    /// Parses the original IPv6 header quoted by an ICMPv6 error.
    ///
    /// ICMPv6 quotes as much of the offending packet as fits in the minimum
    /// IPv6 MTU, so the quoted payload may be truncated relative to the
    /// original `payload_length` field.
    pub(crate) fn parse_icmp_quote(packet: &[u8]) -> Result<Self, Ipv6Error> {
        let header_slice = Ipv6HeaderSlice::from_slice(packet).map_err(|_| Ipv6Error::Malformed)?;
        if packet.len() < IPV6_HEADER_LEN {
            return Err(Ipv6Error::Malformed);
        }
        let available = packet.len();
        let payload = &packet[IPV6_HEADER_LEN..];
        let (next_header, transport_offset) =
            skip_extension_headers(header_slice.next_header().0, payload, false)?;

        Ok(Self {
            src_addr: Ipv6Address::from_octets(header_slice.source()),
            dst_addr: Ipv6Address::from_octets(header_slice.destination()),
            next_header,
            hop_limit: header_slice.hop_limit(),
            transport_offset: IPV6_HEADER_LEN + transport_offset,
            total_len: available,
            traffic_class: header_slice.traffic_class(),
        })
    }

    pub(crate) fn validate_input_packet(packet: &mut PacketBuf) -> Result<Self, Ipv6Error> {
        let header = Self::parse_input(packet.network_packet().ok_or(Ipv6Error::Malformed)?)?;
        header.apply_to_packet(packet)
    }

    pub(crate) fn prepare_output_packet(packet: &mut PacketBuf) -> Result<Self, Ipv6Error> {
        {
            let data = packet.network_packet_mut().ok_or(Ipv6Error::Malformed)?;
            let payload_len = data
                .len()
                .checked_sub(IPV6_HEADER_LEN)
                .ok_or(Ipv6Error::Malformed)?;
            let payload_len = u16::try_from(payload_len).map_err(|_| Ipv6Error::Malformed)?;
            data[4..6].copy_from_slice(&payload_len.to_be_bytes());
        }
        // Re-parse after repairing payload_length so offsets match the buffer.
        let header = Self::parse_output(packet.network_packet().ok_or(Ipv6Error::Malformed)?)?;
        header.apply_to_packet(packet)
    }

    pub(crate) fn src_addr(self) -> Ipv6Address {
        self.src_addr
    }

    pub(crate) fn dst_addr(self) -> Ipv6Address {
        self.dst_addr
    }

    pub(crate) fn next_header(self) -> u8 {
        self.next_header
    }

    pub(crate) fn hop_limit(self) -> u8 {
        self.hop_limit
    }

    #[cfg(unittest)]
    pub(crate) fn transport_offset(self) -> usize {
        self.transport_offset
    }

    pub(crate) fn total_len(self) -> usize {
        self.total_len
    }

    pub(crate) fn header_len(self) -> usize {
        self.transport_offset
    }

    pub(crate) fn is_multicast(self) -> bool {
        self.dst_addr.is_multicast()
    }

    fn parse(packet: &[u8], reject_fragment: bool) -> Result<Self, Ipv6Error> {
        let header_slice = Ipv6HeaderSlice::from_slice(packet).map_err(|_| Ipv6Error::Malformed)?;
        let payload_len = header_slice.payload_length() as usize;
        let total_len = IPV6_HEADER_LEN
            .checked_add(payload_len)
            .ok_or(Ipv6Error::Malformed)?;
        if total_len > packet.len() {
            return Err(Ipv6Error::Malformed);
        }

        let payload = &packet[IPV6_HEADER_LEN..total_len];
        let (next_header, transport_offset) =
            skip_extension_headers(header_slice.next_header().0, payload, reject_fragment)?;

        Ok(Self {
            src_addr: Ipv6Address::from_octets(header_slice.source()),
            dst_addr: Ipv6Address::from_octets(header_slice.destination()),
            next_header,
            hop_limit: header_slice.hop_limit(),
            transport_offset: IPV6_HEADER_LEN + transport_offset,
            total_len,
            traffic_class: header_slice.traffic_class(),
        })
    }

    fn apply_to_packet(self, packet: &mut PacketBuf) -> Result<Self, Ipv6Error> {
        packet
            .truncate_network_packet(self.total_len)
            .ok_or(Ipv6Error::Malformed)?;
        let transport_offset = packet
            .network_offset()
            .and_then(|offset| offset.checked_add(self.transport_offset))
            .ok_or(Ipv6Error::Malformed)?;
        packet.set_transport_offset(transport_offset);
        packet.set_checksum_state(ChecksumState::Verified);
        Ok(self)
    }
}

pub(crate) fn payload<'a>(packet: &'a [u8], header: &Ipv6Header) -> Option<&'a [u8]> {
    packet.get(header.transport_offset..header.total_len)
}

/// Writes an IPv6 header into a complete preallocated packet.
pub(crate) fn write_ipv6_packet_header(
    packet: &mut [u8],
    src_addr: Ipv6Address,
    dst_addr: Ipv6Address,
    next_header: u8,
    hop_limit: u8,
) -> Option<()> {
    let payload_len = packet.len().checked_sub(IPV6_HEADER_LEN)?;
    write_ipv6_header(
        packet.get_mut(..IPV6_HEADER_LEN)?,
        src_addr,
        dst_addr,
        next_header,
        hop_limit,
        payload_len,
    )
}

fn write_ipv6_header(
    header: &mut [u8],
    src_addr: Ipv6Address,
    dst_addr: Ipv6Address,
    next_header: u8,
    hop_limit: u8,
    payload_len: usize,
) -> Option<()> {
    if header.len() < IPV6_HEADER_LEN {
        return None;
    }
    let payload_len = u16::try_from(payload_len).ok()?;
    let ether_header = EtherIpv6Header {
        traffic_class: 0,
        flow_label: Default::default(),
        payload_length: payload_len,
        next_header: IpNumber(next_header),
        hop_limit,
        source: src_addr.octets(),
        destination: dst_addr.octets(),
    };
    header[..IPV6_HEADER_LEN].copy_from_slice(&ether_header.to_bytes());
    Some(())
}

#[cfg(unittest)]
pub(crate) fn build_ipv6_packet(
    src_addr: Ipv6Address,
    dst_addr: Ipv6Address,
    next_header: u8,
    hop_limit: u8,
    payload: &[u8],
) -> Option<Vec<u8>> {
    let total_len = IPV6_HEADER_LEN.checked_add(payload.len())?;
    let mut packet = vec![0u8; total_len];
    write_ipv6_packet_header(&mut packet, src_addr, dst_addr, next_header, hop_limit)?;
    packet[IPV6_HEADER_LEN..].copy_from_slice(payload);
    Some(packet)
}

fn skip_extension_headers(
    mut next_header: u8,
    payload: &[u8],
    reject_fragment: bool,
) -> Result<(u8, usize), Ipv6Error> {
    let mut offset = 0usize;
    loop {
        match next_header {
            IPV6_EXT_HOP_BY_HOP | IPV6_EXT_ROUTING | IPV6_EXT_DEST_OPTIONS => {
                let hdr = payload.get(offset..).ok_or(Ipv6Error::Malformed)?;
                if hdr.len() < 2 {
                    return Err(Ipv6Error::Malformed);
                }
                let len = (usize::from(hdr[1]) + 1) * 8;
                if hdr.len() < len {
                    return Err(Ipv6Error::Malformed);
                }
                next_header = hdr[0];
                offset = offset.checked_add(len).ok_or(Ipv6Error::Malformed)?;
            }
            IPV6_EXT_FRAGMENT => {
                if reject_fragment {
                    return Err(Ipv6Error::Fragmented);
                }
                // Quoted ICMPv6 payloads may include a Fragment header. Input
                // parsing rejects fragments instead of reassembling them.
                return Ok((IPV6_EXT_FRAGMENT, offset));
            }
            IPV6_EXT_AH => {
                let hdr = payload.get(offset..).ok_or(Ipv6Error::Malformed)?;
                if hdr.len() < 2 {
                    return Err(Ipv6Error::Malformed);
                }
                let len = (usize::from(hdr[1]) + 2) * 4;
                if hdr.len() < len {
                    return Err(Ipv6Error::Malformed);
                }
                next_header = hdr[0];
                offset = offset.checked_add(len).ok_or(Ipv6Error::Malformed)?;
            }
            _ => return Ok((next_header, offset)),
        }
    }
}
