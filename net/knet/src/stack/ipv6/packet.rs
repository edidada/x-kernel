// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv6 packet validation and output helpers.

#[cfg(unittest)]
use alloc::vec;
use alloc::vec::Vec;

use etherparse::{
    IpNumber, Ipv6FragmentHeaderSlice, Ipv6Header as EtherIpv6Header, Ipv6HeaderSlice,
};

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
pub(crate) const IPV6_EXT_ROUTING: u8 = 43;
pub(crate) const IPV6_EXT_FRAGMENT: u8 = 44;
pub(crate) const IPV6_EXT_DEST_OPTIONS: u8 = 60;
pub(crate) const IPV6_EXT_AH: u8 = 51;

pub(crate) const IPV6_FRAGMENT_HEADER_LEN: usize = 8;
/// RFC 8200 payload_length maximum; Linux `IPV6_MAXPLEN`.
pub(crate) const IPV6_MAX_PAYLOAD_LEN: usize = 65535;

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
    /// Offset of the next-header field that named `next_header`.
    ///
    /// Points into the base header (offset 6) or into the extension header
    /// that carried the value, so ICMPv6 Parameter Problem can locate it.
    next_header_offset: usize,
}

impl Ipv6Header {
    pub(crate) fn parse_input(packet: &[u8]) -> Result<Self, Ipv6Error> {
        Self::parse(packet, true)
    }

    /// Parses an inbound packet, stopping at a Fragment header instead of
    /// rejecting it.
    ///
    /// [`Self::next_header`] is [`IPV6_EXT_FRAGMENT`] and
    /// [`Self::header_len`] points at the Fragment header so the reassembler
    /// can copy the unfragmentable prefix.
    pub(crate) fn parse_input_allowing_fragment(packet: &[u8]) -> Result<Self, Ipv6Error> {
        Self::parse(packet, false)
    }

    /// Parses an outbound packet, stopping at a Fragment header.
    ///
    /// Local delivery re-enters [`super::process_ipv6_input`], so dispatch
    /// must not drop fragments that a raw socket injected toward a local
    /// address.
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
        let (next_header, transport_offset, next_header_offset) =
            skip_extension_headers(header_slice.next_header().0, payload, false)?;

        Ok(Self {
            src_addr: Ipv6Address::from_octets(header_slice.source()),
            dst_addr: Ipv6Address::from_octets(header_slice.destination()),
            next_header,
            hop_limit: header_slice.hop_limit(),
            transport_offset: IPV6_HEADER_LEN + transport_offset,
            total_len: available,
            traffic_class: header_slice.traffic_class(),
            next_header_offset,
        })
    }

    #[cfg(unittest)]
    pub(crate) fn validate_input_packet(packet: &mut PacketBuf) -> Result<Self, Ipv6Error> {
        let header = Self::parse_input(packet.network_packet().ok_or(Ipv6Error::Malformed)?)?;
        header.apply_to_packet(packet)
    }

    pub(crate) fn validate_input_packet_allowing_fragment(
        packet: &mut PacketBuf,
    ) -> Result<Self, Ipv6Error> {
        let header = Self::parse_input_allowing_fragment(
            packet.network_packet().ok_or(Ipv6Error::Malformed)?,
        )?;
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

    pub(crate) fn ecn(self) -> u8 {
        self.traffic_class & 0b11
    }

    #[cfg(unittest)]
    pub(crate) fn transport_offset(self) -> usize {
        self.transport_offset
    }

    /// Offset of the next-header field that named [`Self::next_header`].
    pub(crate) fn next_header_offset(self) -> usize {
        self.next_header_offset
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
        let (next_header, transport_offset, next_header_offset) =
            skip_extension_headers(header_slice.next_header().0, payload, reject_fragment)?;

        Ok(Self {
            src_addr: Ipv6Address::from_octets(header_slice.source()),
            dst_addr: Ipv6Address::from_octets(header_slice.destination()),
            next_header,
            hop_limit: header_slice.hop_limit(),
            transport_offset: IPV6_HEADER_LEN + transport_offset,
            total_len,
            traffic_class: header_slice.traffic_class(),
            next_header_offset,
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

/// Returns the resolved next header, the transport offset, and the offset of
/// the next-header field that named it (6 for the base header, otherwise the
/// extension header that carried the value).
fn skip_extension_headers(
    mut next_header: u8,
    payload: &[u8],
    reject_fragment: bool,
) -> Result<(u8, usize, usize), Ipv6Error> {
    let mut offset = 0usize;
    let mut next_header_offset = 6usize;
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
                next_header_offset = IPV6_HEADER_LEN + offset;
                offset = offset.checked_add(len).ok_or(Ipv6Error::Malformed)?;
            }
            IPV6_EXT_FRAGMENT => {
                if reject_fragment {
                    return Err(Ipv6Error::Fragmented);
                }
                // Stop at the Fragment header: RFC 8200 §4.5 places it last
                // in the unfragmentable chain, and the reassembler needs its
                // offset plus the next-header field that named it.
                return Ok((IPV6_EXT_FRAGMENT, offset, next_header_offset));
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
                next_header_offset = IPV6_HEADER_LEN + offset;
                offset = offset.checked_add(len).ok_or(Ipv6Error::Malformed)?;
            }
            _ => return Ok((next_header, offset, next_header_offset)),
        }
    }
}

/// Parsed IPv6 Fragment header (RFC 8200 §4.5).
///
/// `offset` is a byte offset in the fragmentable payload (`frag_off & ~0x7`
/// on the wire, matching Linux `ip6_frag_queue`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Ipv6Fragment {
    pub(crate) next_header: u8,
    pub(crate) offset: usize,
    pub(crate) more_fragments: bool,
    pub(crate) identification: u32,
}

impl Ipv6Fragment {
    pub(crate) fn parse(packet: &[u8], header: &Ipv6Header) -> Result<Self, Ipv6Error> {
        if header.next_header() != IPV6_EXT_FRAGMENT {
            return Err(Ipv6Error::Malformed);
        }
        let start = header.header_len();
        let bytes = packet.get(start..).ok_or(Ipv6Error::Malformed)?;
        let frag = Ipv6FragmentHeaderSlice::from_slice(bytes).map_err(|_| Ipv6Error::Malformed)?;
        let offset = usize::from(frag.fragment_offset().value())
            .checked_mul(8)
            .ok_or(Ipv6Error::Malformed)?;
        Ok(Self {
            next_header: frag.next_header().0,
            offset,
            more_fragments: frag.more_fragments(),
            identification: frag.identification(),
        })
    }

    pub(crate) fn is_atomic(self) -> bool {
        self.offset == 0 && !self.more_fragments
    }

    pub(crate) fn payload<'a>(self, packet: &'a [u8], header: &Ipv6Header) -> Option<&'a [u8]> {
        let start = header.header_len().checked_add(IPV6_FRAGMENT_HEADER_LEN)?;
        packet.get(start..header.total_len())
    }

    pub(crate) fn frag_off_pointer(header: &Ipv6Header) -> u32 {
        (header.header_len() + 2) as u32
    }
}

/// RFC 8200 §4.5 / RFC 7112: the first fragment must carry a complete
/// header chain through the upper-layer header.
///
/// Linux `ipv6frag_thdr_truncated` only fires when skip succeeds and the
/// remaining bytes are shorter than TCP (20), UDP (8), ICMPv6 (8), or 1
/// for any other next header. Skip failures are not reported as code 3.
pub(crate) fn first_fragment_upper_header_is_truncated(
    fragment_next_header: u8,
    fragmentable: &[u8],
) -> bool {
    let Ok((upper, offset, _)) = skip_extension_headers(fragment_next_header, fragmentable, true)
    else {
        return false;
    };
    fragmentable
        .len()
        .checked_sub(offset)
        .is_none_or(|available| available < min_upper_header_len(upper))
}

fn min_upper_header_len(next_header: u8) -> usize {
    match next_header {
        PROTOCOL_TCP => 20,
        PROTOCOL_UDP | PROTOCOL_ICMPV6 => 8,
        _ => 1,
    }
}

/// Removes the 8-byte Fragment header and rewrites the next-header field
/// that named it. Used for atomic fragments (offset=0 and M=0).
pub(crate) fn strip_ipv6_fragment_header(
    packet: &[u8],
    header: &Ipv6Header,
    fragment: Ipv6Fragment,
) -> Result<Vec<u8>, Ipv6Error> {
    let frag_start = header.header_len();
    let frag_end = frag_start
        .checked_add(IPV6_FRAGMENT_HEADER_LEN)
        .ok_or(Ipv6Error::Malformed)?;
    if packet.len() < header.total_len() || header.total_len() < frag_end {
        return Err(Ipv6Error::Malformed);
    }
    let mut out = Vec::with_capacity(header.total_len() - IPV6_FRAGMENT_HEADER_LEN);
    out.extend_from_slice(&packet[..frag_start]);
    out.extend_from_slice(&packet[frag_end..header.total_len()]);
    *out.get_mut(header.next_header_offset())
        .ok_or(Ipv6Error::Malformed)? = fragment.next_header;
    rewrite_ipv6_payload_len(&mut out)?;
    Ok(out)
}

pub(crate) fn rewrite_reassembled_ipv6(
    packet: &mut [u8],
    payload_len: usize,
    ecn: u8,
    next_header_offset: usize,
    next_header: u8,
) -> Result<(), Ipv6Error> {
    let header_slice = Ipv6HeaderSlice::from_slice(packet).map_err(|_| Ipv6Error::Malformed)?;
    let mut header = header_slice.to_header();
    header.payload_length = u16::try_from(payload_len).map_err(|_| Ipv6Error::Malformed)?;
    header.traffic_class = (header.traffic_class & !0b11) | (ecn & 0b11);
    packet
        .get_mut(..IPV6_HEADER_LEN)
        .ok_or(Ipv6Error::Malformed)?
        .copy_from_slice(&header.to_bytes());
    *packet
        .get_mut(next_header_offset)
        .ok_or(Ipv6Error::Malformed)? = next_header;
    Ok(())
}

fn rewrite_ipv6_payload_len(packet: &mut [u8]) -> Result<(), Ipv6Error> {
    let payload_len = packet
        .len()
        .checked_sub(IPV6_HEADER_LEN)
        .ok_or(Ipv6Error::Malformed)?;
    let payload_len = u16::try_from(payload_len).map_err(|_| Ipv6Error::Malformed)?;
    packet
        .get_mut(4..6)
        .ok_or(Ipv6Error::Malformed)?
        .copy_from_slice(&payload_len.to_be_bytes());
    Ok(())
}
