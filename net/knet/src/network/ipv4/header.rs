// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 header parsing and decoded header fields.
//!
//! This module parses IPv4 headers, validates their basic structure,
//! and provides decoded header and fragmentation information.

use etherparse::Ipv4HeaderSlice;

use crate::ip::Ipv4Address;

pub(super) const IPV4_FRAGMENT_OFFSET_UNIT: usize = 8;

/// IPv4 header decoding failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Ipv4Error {
    /// Header structure or packet length does not satisfy the parsing contract.
    Malformed,
    /// The supplied IPv4 header checksum is invalid.
    BadChecksum,
}

/// IPv4 fragmentation flags and fragment offset decoded from the header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Ipv4FragmentInfo {
    dont_fragment: bool,
    more_fragments: bool,
    offset_units: u16,
}

impl Ipv4FragmentInfo {
    pub(super) const DONT_FRAGMENT: Self = Self {
        dont_fragment: true,
        more_fragments: false,
        offset_units: 0,
    };
    pub(super) const NONE: Self = Self {
        dont_fragment: false,
        more_fragments: false,
        offset_units: 0,
    };

    /// Payload offset in the wire format's eight-byte units.
    pub(super) fn offset_units(self) -> u16 {
        self.offset_units
    }

    /// Whether the sender prohibits fragmentation.
    pub(super) fn dont_fragment(self) -> bool {
        self.dont_fragment
    }

    /// Whether more fragments follow this one.
    pub(super) fn more_fragments(self) -> bool {
        self.more_fragments
    }

    /// Payload offset in bytes within the original datagram.
    pub(super) fn offset_bytes(self) -> usize {
        usize::from(self.offset_units) * IPV4_FRAGMENT_OFFSET_UNIT
    }

    /// Whether this header describes a first or subsequent fragment.
    pub(super) fn is_fragmented(self) -> bool {
        self.more_fragments || self.offset_units != 0
    }
}

/// Parsed IPv4 header fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Ipv4Header {
    src_addr: Ipv4Address,
    dst_addr: Ipv4Address,
    protocol: u8,
    ttl: u8,
    header_len: usize,
    total_len: usize,
    identification: u16,
    fragment: Ipv4FragmentInfo,
    dscp_ecn: u8,
}

impl Ipv4Header {
    /// Parses an IPv4 header, including any header options.
    ///
    /// Reads the total length from the header without validating the full packet
    /// length or header checksum. Callers must perform these checks separately
    /// before accepting an incoming packet.
    pub(crate) fn parse(packet: &[u8]) -> Result<Self, Ipv4Error> {
        Ok(Self::from_slice(&Self::parse_header(packet)?))
    }

    /// Checks that the IPv4 header bytes are structurally available.
    /// Datagram bounds and checksum policy belong to the processing stage.
    pub(crate) fn parse_header(packet: &[u8]) -> Result<Ipv4HeaderSlice<'_>, Ipv4Error> {
        Ipv4HeaderSlice::from_slice(packet).map_err(|_| Ipv4Error::Malformed)
    }

    /// Verifies the checksum of a structurally parsed IPv4 header.
    pub(crate) fn check_header_checksum(header: &Ipv4HeaderSlice<'_>) -> Result<(), Ipv4Error> {
        if header.header_checksum() != header.to_header().calc_header_checksum() {
            return Err(Ipv4Error::BadChecksum);
        }
        Ok(())
    }

    /// Returns the source IPv4 address.
    pub(crate) fn src_addr(self) -> Ipv4Address {
        self.src_addr
    }

    /// Returns the destination IPv4 address.
    pub(crate) fn dst_addr(self) -> Ipv4Address {
        self.dst_addr
    }

    /// Returns the IP protocol number for the payload.
    pub(crate) fn protocol(self) -> u8 {
        self.protocol
    }

    /// Returns the IPv4 header length in bytes, including options.
    pub(crate) fn header_len(self) -> usize {
        self.header_len
    }

    /// Returns the datagram length in bytes declared by the IPv4 header.
    pub(crate) fn total_len(self) -> usize {
        self.total_len
    }

    /// Returns the identification used to group fragments of a datagram.
    pub(crate) fn identification(self) -> u16 {
        self.identification
    }

    /// Returns the payload offset in bytes within the original datagram.
    pub(crate) fn fragment_offset(self) -> usize {
        self.fragment.offset_bytes()
    }

    /// Whether more fragments follow this one.
    pub(crate) fn more_fragments(self) -> bool {
        self.fragment.more_fragments()
    }

    /// Whether the sender prohibits fragmentation.
    pub(crate) fn dont_fragment(self) -> bool {
        self.fragment.dont_fragment()
    }

    /// Whether the header describes a first or subsequent fragment.
    pub(crate) fn is_fragmented(self) -> bool {
        self.fragment.is_fragmented()
    }

    /// Returns the two-bit Explicit Congestion Notification value.
    pub(crate) fn ecn(self) -> u8 {
        self.dscp_ecn & 0b11
    }

    /// Whether the destination is a broadcast or multicast address.
    pub(crate) fn is_broadcast_or_multicast(self) -> bool {
        self.dst_addr.is_broadcast() || self.dst_addr.is_multicast()
    }

    /// Copies wire fields from a structurally parsed header without validation.
    pub(crate) fn from_slice(header_slice: &Ipv4HeaderSlice<'_>) -> Self {
        Self {
            src_addr: Ipv4Address::from_octets(header_slice.source()),
            dst_addr: Ipv4Address::from_octets(header_slice.destination()),
            protocol: header_slice.protocol().0,
            ttl: header_slice.ttl(),
            header_len: header_slice.slice().len(),
            total_len: usize::from(header_slice.total_len()),
            identification: header_slice.identification(),
            fragment: Ipv4FragmentInfo {
                offset_units: header_slice.fragments_offset().value(),
                more_fragments: header_slice.more_fragments(),
                dont_fragment: header_slice.dont_fragment(),
            },
            dscp_ecn: header_slice.slice()[1],
        }
    }
}

unittest::path_mod_test!(tests, "../../../ut-tests/network/ipv4/header/tests.rs");
