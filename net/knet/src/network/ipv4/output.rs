// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 output decoding, encoding and fragmentation using owned library headers.

use alloc::{vec, vec::Vec};

use etherparse::{IpEcn, IpFragOffset, IpNumber, Ipv4Header};

use super::{Ipv4Error, normalize_packet};
use crate::{buf::PacketBuf, ip::Ipv4Address};

const IPV4_FRAGMENT_OFFSET_UNIT: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Ipv4FragmentError {
    Malformed,
    DontFragment,
    MtuTooSmall,
}

/// Decodes a byte-oriented producer's header and derives length from its buffer.
/// The producer may leave total length and checksum unfinished.
pub(crate) fn prepare_output_header(bytes: &[u8]) -> Result<Ipv4Header, Ipv4Error> {
    let (mut header, _) = Ipv4Header::from_slice(bytes).map_err(|_| Ipv4Error::Malformed)?;
    header
        .set_payload_len(bytes.len() - header.header_len())
        .map_err(|_| Ipv4Error::Malformed)?;
    Ok(header)
}

/// Finalizes a queued byte packet with one header decode.
pub(crate) fn prepare_output_packet(packet: &mut PacketBuf) -> Result<Ipv4Header, Ipv4Error> {
    let mut header = prepare_output_header(packet.network_packet().ok_or(Ipv4Error::Malformed)?)?;
    header.header_checksum = header.calc_header_checksum();
    packet
        .network_packet_mut()
        .ok_or(Ipv4Error::Malformed)?
        .get_mut(..header.header_len())
        .ok_or(Ipv4Error::Malformed)?
        .copy_from_slice(&header.to_bytes());
    normalize_packet(packet, header.header_len(), usize::from(header.total_len))?;
    Ok(header)
}

/// Fragments using a header already decoded by the output caller.
pub(crate) fn fragment_output_packet(
    packet: &[u8],
    header: &Ipv4Header,
    path_mtu: usize,
    identification: u16,
) -> Result<Vec<Vec<u8>>, Ipv4FragmentError> {
    if packet.len() <= path_mtu {
        return Ok(vec![packet.to_vec()]);
    }
    if header.dont_fragment {
        return Err(Ipv4FragmentError::DontFragment);
    }
    if header.is_fragmenting_payload() {
        return Err(Ipv4FragmentError::Malformed);
    }
    let payload = packet
        .get(header.header_len()..)
        .ok_or(Ipv4FragmentError::Malformed)?;
    let max_payload_len = path_mtu
        .checked_sub(header.header_len())
        .ok_or(Ipv4FragmentError::MtuTooSmall)?;
    let aligned_len = max_payload_len / IPV4_FRAGMENT_OFFSET_UNIT * IPV4_FRAGMENT_OFFSET_UNIT;
    if aligned_len == 0 {
        return Err(Ipv4FragmentError::MtuTooSmall);
    }

    // Options are copied verbatim, preserving unknown kinds and padding.
    // Per-option copied-bit filtering is not implemented.
    let mut fragments = Vec::new();
    let mut offset = 0;
    while offset < payload.len() {
        let remaining = payload.len() - offset;
        let len = if remaining <= max_payload_len {
            remaining
        } else {
            aligned_len
        };
        let mut fragment_header = header.clone();
        fragment_header
            .set_payload_len(len)
            .map_err(|_| Ipv4FragmentError::Malformed)?;
        fragment_header.identification = identification;
        fragment_header.dont_fragment = false;
        fragment_header.more_fragments = offset + len < payload.len();
        fragment_header.fragment_offset = IpFragOffset::try_new(
            u16::try_from(offset / IPV4_FRAGMENT_OFFSET_UNIT)
                .map_err(|_| Ipv4FragmentError::Malformed)?,
        )
        .map_err(|_| Ipv4FragmentError::Malformed)?;
        fragment_header.header_checksum = fragment_header.calc_header_checksum();
        let mut fragment = Vec::with_capacity(fragment_header.header_len() + len);
        fragment.extend_from_slice(&fragment_header.to_bytes());
        fragment.extend_from_slice(&payload[offset..offset + len]);
        fragments.push(fragment);
        offset += len;
    }
    Ok(fragments)
}

/// Writes an IPv4 header into a complete preallocated packet.
pub(crate) fn write_ipv4_packet_header(
    packet: &mut [u8],
    src_addr: Ipv4Address,
    dst_addr: Ipv4Address,
    protocol: u8,
    ttl: u8,
    dont_fragment: bool,
) -> Option<()> {
    let payload_len = packet.len().checked_sub(Ipv4Header::MIN_LEN)?;
    let mut header = Ipv4Header::new(
        u16::try_from(payload_len).ok()?,
        ttl,
        IpNumber(protocol),
        src_addr.octets(),
        dst_addr.octets(),
    )
    .ok()?;
    header.dont_fragment = dont_fragment;
    header.header_checksum = header.calc_header_checksum();
    packet
        .get_mut(..header.header_len())?
        .copy_from_slice(&header.to_bytes());
    Some(())
}

pub(crate) fn rewrite_reassembled_header(
    packet: &mut [u8],
    payload_len: usize,
    ecn: u8,
) -> Result<(), Ipv4Error> {
    let (mut header, _) = Ipv4Header::from_slice(packet).map_err(|_| Ipv4Error::Malformed)?;
    header
        .set_payload_len(payload_len)
        .map_err(|_| Ipv4Error::Malformed)?;
    header.ecn = IpEcn::try_new(ecn).map_err(|_| Ipv4Error::Malformed)?;
    header.more_fragments = false;
    header.fragment_offset = IpFragOffset::ZERO;
    header.header_checksum = header.calc_header_checksum();
    packet
        .get_mut(..header.header_len())
        .ok_or(Ipv4Error::Malformed)?
        .copy_from_slice(&header.to_bytes());
    Ok(())
}
