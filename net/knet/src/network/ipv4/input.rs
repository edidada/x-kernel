// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Borrowed IPv4 input validation and packet-boundary normalization.

use etherparse::Ipv4HeaderSlice;

use crate::buf::{ChecksumState, PacketBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Ipv4Error {
    Malformed,
    BadChecksum,
}

/// Validates a complete datagram without interpreting extension or transport headers.
pub(crate) fn parse_input(bytes: &[u8]) -> Result<Ipv4HeaderSlice<'_>, Ipv4Error> {
    let header = Ipv4HeaderSlice::from_slice(bytes).map_err(|_| Ipv4Error::Malformed)?;
    if usize::from(header.total_len()) < header.slice().len()
        || usize::from(header.total_len()) > bytes.len()
    {
        return Err(Ipv4Error::Malformed);
    }
    check_checksum(&header)?;
    Ok(header)
}

/// Validates the complete header in an ICMP quote, allowing truncated payload.
pub(crate) fn parse_icmp_quote(bytes: &[u8]) -> Result<Ipv4HeaderSlice<'_>, Ipv4Error> {
    let header = Ipv4HeaderSlice::from_slice(bytes).map_err(|_| Ipv4Error::Malformed)?;
    if usize::from(header.total_len()) < header.slice().len() {
        return Err(Ipv4Error::Malformed);
    }
    check_checksum(&header)?;
    Ok(header)
}

fn check_checksum(header: &Ipv4HeaderSlice<'_>) -> Result<(), Ipv4Error> {
    if header.header_checksum() != header.to_header().calc_header_checksum() {
        return Err(Ipv4Error::BadChecksum);
    }
    Ok(())
}

/// Applies lengths from a validated input header or a finalized output header.
/// Call after borrowing ends, since normalization can detach shared packet storage.
pub(crate) fn normalize_packet(
    packet: &mut PacketBuf,
    header_len: usize,
    total_len: usize,
) -> Result<(), Ipv4Error> {
    if header_len > total_len {
        return Err(Ipv4Error::Malformed);
    }
    let transport_offset = packet
        .network_offset()
        .and_then(|offset| offset.checked_add(header_len))
        .ok_or(Ipv4Error::Malformed)?;
    packet
        .truncate_network_packet(total_len)
        .ok_or(Ipv4Error::Malformed)?;
    packet.set_transport_offset(transport_offset);
    packet.set_checksum_state(ChecksumState::Verified);
    Ok(())
}
