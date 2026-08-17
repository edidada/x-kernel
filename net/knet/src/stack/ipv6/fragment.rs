// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv6 fragment reassembly for local input.
//!
//! [`Ipv6Reassembler`] groups fragments by source, destination, 32-bit
//! identification, and input interface. The upper-layer protocol lives in
//! the first fragment's Fragment.Next Header, so it is not part of the key.
//! Each queue stores the unfragmentable prefix from the first fragment and
//! non-overlapping fragmentable payload ranges until the datagram is complete.
//! [`crate::stack::ingress::IngressProcessor`] drives insertion and timeout
//! removal, then feeds complete packets back through IPv6 input.
//!
//! RFC 5722 overlapping ranges drop the whole datagram. Queue count, retained
//! bytes, and lifetime are bounded. Expired queues retain the original first
//! fragment so ICMPv6 Time Exceeded (code 1) can quote it.

use alloc::{collections::BTreeMap, vec::Vec};

use ktime_types::{MonotonicInstant, TimeSpan};

use super::packet::{
    IPV6_FRAGMENT_HEADER_LEN, IPV6_HEADER_LEN, IPV6_MAX_PAYLOAD_LEN, Ipv6Fragment, Ipv6Header,
    rewrite_reassembled_ipv6,
};
use crate::{
    buf::{PacketBuf, PacketOwner, PacketType},
    ip::Ipv6Address,
};

/// Linux `IPV6_FRAG_TIMEOUT` is 60 seconds.
const IPV6_REASSEMBLY_TIMEOUT: TimeSpan = TimeSpan::from_secs(60);
const IPV6_REASSEMBLY_HIGH_BYTES: usize = 4 * 1024 * 1024;
const IPV6_REASSEMBLY_LOW_BYTES: usize = 3 * 1024 * 1024;
const IPV6_REASSEMBLY_MAX_QUEUES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Ipv6FragKey {
    src_addr: [u8; 16],
    dst_addr: [u8; 16],
    identification: u32,
    ifindex: i32,
}

#[derive(Debug)]
struct Ipv6ReassemblyQueue {
    packet_type: PacketType,
    /// Arrival interface of the latest fragment, used for the completed
    /// [`PacketBuf`]. The lookup key may store `ifindex = 0` for global
    /// unicast destinations (Linux `fq_find`).
    arrival_ifindex: i32,
    /// Unfragmentable prefix plus the 8-byte Fragment header from the first
    /// fragment. Timeout ICMP quotes this with the offset-0 payload.
    first_header: Option<Vec<u8>>,
    first_ecn: Option<u8>,
    next_header_offset: usize,
    upper_next_header: u8,
    fragments: BTreeMap<usize, Vec<u8>>,
    received_len: usize,
    total_payload_len: Option<usize>,
    ecn_mask: u8,
    expires_at: MonotonicInstant,
    memory_bytes: usize,
}

pub(crate) struct ExpiredIpv6Fragment {
    pub(crate) packet_type: PacketType,
    pub(crate) header: Ipv6Header,
    pub(crate) packet: Vec<u8>,
}

pub(crate) enum Ipv6ReassemblyResult {
    Complete(PacketBuf),
    Pending,
    Dropped,
}

#[derive(Default)]
pub(crate) struct Ipv6Reassembler {
    queues: BTreeMap<Ipv6FragKey, Ipv6ReassemblyQueue>,
    memory_bytes: usize,
}

enum InsertResult {
    Inserted { memory_bytes: usize },
    Duplicate,
    InvalidRange,
    Overlap,
}

impl Ipv6Reassembler {
    pub(crate) fn new() -> Self {
        Self {
            queues: BTreeMap::new(),
            memory_bytes: 0,
        }
    }

    pub(crate) fn reassemble(
        &mut self,
        packet: PacketBuf,
        header: Ipv6Header,
        fragment: Ipv6Fragment,
        now: MonotonicInstant,
    ) -> Ipv6ReassemblyResult {
        let key = Ipv6FragKey {
            src_addr: header.src_addr().octets(),
            dst_addr: header.dst_addr().octets(),
            identification: fragment.identification,
            ifindex: reassembly_ifindex(header.dst_addr(), packet.ifindex()),
        };
        let Some(ip_packet) = packet.network_packet() else {
            return Ipv6ReassemblyResult::Dropped;
        };
        let Some(payload) = fragment.payload(ip_packet, &header) else {
            self.remove_queue(&key);
            return Ipv6ReassemblyResult::Dropped;
        };

        let offset = fragment.offset;
        let payload_len = payload.len();
        let end = match offset.checked_add(payload_len) {
            Some(end) => end,
            None => {
                self.remove_queue(&key);
                return Ipv6ReassemblyResult::Dropped;
            }
        };
        // Linux `ip6_frag_queue` discards the queue on a zero-length fragment.
        if end == offset {
            self.remove_queue(&key);
            return Ipv6ReassemblyResult::Dropped;
        }

        let fragment_payload = payload.to_vec();
        let arrival_ifindex = packet.ifindex();
        let queue = self
            .queues
            .entry(key)
            .or_insert_with(|| Ipv6ReassemblyQueue {
                packet_type: packet.packet_type(),
                arrival_ifindex,
                first_header: None,
                first_ecn: None,
                next_header_offset: 0,
                upper_next_header: 0,
                fragments: BTreeMap::new(),
                received_len: 0,
                total_payload_len: None,
                ecn_mask: 0,
                expires_at: now + IPV6_REASSEMBLY_TIMEOUT,
                memory_bytes: 0,
            });
        queue.arrival_ifindex = arrival_ifindex;

        if !fragment.more_fragments {
            if queue
                .total_payload_len
                .is_some_and(|old_total| old_total != end)
                || queue
                    .last_fragment_end()
                    .is_some_and(|old_end| old_end > end)
            {
                self.remove_queue(&key);
                return Ipv6ReassemblyResult::Dropped;
            }
            queue.total_payload_len = Some(end);
        } else if queue
            .total_payload_len
            .is_some_and(|total_payload_len| end > total_payload_len)
        {
            self.remove_queue(&key);
            return Ipv6ReassemblyResult::Dropped;
        }

        match queue.insert_fragment(offset, fragment_payload) {
            InsertResult::Inserted { memory_bytes } => {
                queue.memory_bytes = queue.memory_bytes.saturating_add(memory_bytes);
                self.memory_bytes = self.memory_bytes.saturating_add(memory_bytes);
            }
            InsertResult::Duplicate => return Ipv6ReassemblyResult::Pending,
            InsertResult::InvalidRange | InsertResult::Overlap => {
                self.remove_queue(&key);
                return Ipv6ReassemblyResult::Dropped;
            }
        }
        queue.ecn_mask |= ecn_bit(header.ecn());
        if offset == 0 && queue.first_header.is_none() {
            let first_end = header.header_len().saturating_add(IPV6_FRAGMENT_HEADER_LEN);
            let Some(first_header) = ip_packet.get(..first_end).map(|bytes| bytes.to_vec()) else {
                self.remove_queue(&key);
                return Ipv6ReassemblyResult::Dropped;
            };
            queue.memory_bytes = queue.memory_bytes.saturating_add(first_header.len());
            self.memory_bytes = self.memory_bytes.saturating_add(first_header.len());
            queue.next_header_offset = header.next_header_offset();
            queue.upper_next_header = fragment.next_header;
            queue.first_header = Some(first_header);
            queue.first_ecn = Some(header.ecn());
        }

        if queue.is_complete() {
            let queue = self.remove_queue(&key).expect("queue exists after insert");
            return build_reassembled_packet(queue);
        }

        self.enforce_limits();
        Ipv6ReassemblyResult::Pending
    }

    pub(crate) fn remove_expired(&mut self, now: MonotonicInstant) -> Vec<ExpiredIpv6Fragment> {
        let keys: Vec<_> = self
            .queues
            .iter()
            .filter_map(|(key, queue)| (now >= queue.expires_at).then_some(*key))
            .collect();
        keys.into_iter()
            .filter_map(|key| {
                let queue = self.remove_queue(&key)?;
                // Linux `ip6frag_expire_frag_queue` only sends ICMP when the
                // first fragment arrived.
                let first_packet = queue.first_fragment_packet()?;
                let header = Ipv6Header::parse_input_allowing_fragment(&first_packet).ok()?;
                Some(ExpiredIpv6Fragment {
                    packet_type: queue.packet_type,
                    header,
                    packet: first_packet,
                })
            })
            .collect()
    }

    fn enforce_limits(&mut self) {
        while self.queues.len() > IPV6_REASSEMBLY_MAX_QUEUES {
            let Some(key) = self.oldest_queue_key() else {
                return;
            };
            self.remove_queue(&key);
        }

        while self.memory_bytes > IPV6_REASSEMBLY_HIGH_BYTES {
            let Some(key) = self.oldest_queue_key() else {
                return;
            };
            self.remove_queue(&key);
            if self.memory_bytes <= IPV6_REASSEMBLY_LOW_BYTES {
                break;
            }
        }
    }

    fn oldest_queue_key(&self) -> Option<Ipv6FragKey> {
        self.queues
            .iter()
            .min_by_key(|(_, queue)| queue.expires_at)
            .map(|(key, _)| *key)
    }

    fn remove_queue(&mut self, key: &Ipv6FragKey) -> Option<Ipv6ReassemblyQueue> {
        let queue = self.queues.remove(key)?;
        self.memory_bytes = self.memory_bytes.saturating_sub(queue.memory_bytes);
        Some(queue)
    }
}

impl Ipv6ReassemblyQueue {
    fn insert_fragment(&mut self, offset: usize, payload: Vec<u8>) -> InsertResult {
        let Some(end) = offset.checked_add(payload.len()) else {
            return InsertResult::InvalidRange;
        };

        if let Some((&start, existing)) = self.fragments.range(..=offset).next_back() {
            let existing_end = start + existing.len();
            if existing_end > offset {
                return if end <= existing_end {
                    InsertResult::Duplicate
                } else {
                    InsertResult::Overlap
                };
            }
        }

        if let Some((&start, _)) = self.fragments.range(offset..).next()
            && start < end
        {
            return InsertResult::Overlap;
        }

        let memory_bytes = payload.len();
        self.fragments.insert(offset, payload);
        self.received_len = self.received_len.saturating_add(memory_bytes);
        InsertResult::Inserted { memory_bytes }
    }

    fn is_complete(&self) -> bool {
        let Some(total_payload_len) = self.total_payload_len else {
            return false;
        };
        self.first_header.is_some()
            && self.first_ecn.is_some()
            && self.received_len == total_payload_len
    }

    fn first_fragment_packet(&self) -> Option<Vec<u8>> {
        let header = self.first_header.as_ref()?;
        let payload = self.fragments.get(&0)?;
        let mut packet = Vec::with_capacity(header.len().checked_add(payload.len())?);
        packet.extend_from_slice(header);
        packet.extend_from_slice(payload);
        Some(packet)
    }

    fn last_fragment_end(&self) -> Option<usize> {
        self.fragments
            .iter()
            .next_back()
            .map(|(offset, payload)| offset + payload.len())
    }
}

fn build_reassembled_packet(queue: Ipv6ReassemblyQueue) -> Ipv6ReassemblyResult {
    let Some(first_header) = queue.first_header else {
        return Ipv6ReassemblyResult::Dropped;
    };
    let Some(total_payload_len) = queue.total_payload_len else {
        return Ipv6ReassemblyResult::Dropped;
    };
    let Some(first_ecn) = queue.first_ecn else {
        return Ipv6ReassemblyResult::Dropped;
    };
    let Some(ecn) = reassembled_ecn(queue.ecn_mask, first_ecn) else {
        return Ipv6ReassemblyResult::Dropped;
    };
    let unfragmentable_len = match first_header.len().checked_sub(IPV6_FRAGMENT_HEADER_LEN) {
        Some(len) if len >= IPV6_HEADER_LEN => len,
        _ => return Ipv6ReassemblyResult::Dropped,
    };
    let packet_len = match unfragmentable_len.checked_add(total_payload_len) {
        Some(packet_len) => packet_len,
        None => return Ipv6ReassemblyResult::Dropped,
    };
    let payload_len = match packet_len.checked_sub(IPV6_HEADER_LEN) {
        Some(payload_len) if payload_len <= IPV6_MAX_PAYLOAD_LEN => payload_len,
        _ => return Ipv6ReassemblyResult::Dropped,
    };

    let mut packet = alloc::vec![0u8; packet_len];
    packet[..unfragmentable_len].copy_from_slice(&first_header[..unfragmentable_len]);
    for (offset, payload) in queue.fragments {
        let start = unfragmentable_len + offset;
        let end = start + payload.len();
        packet[start..end].copy_from_slice(&payload);
    }
    if rewrite_reassembled_ipv6(
        &mut packet,
        payload_len,
        ecn,
        queue.next_header_offset,
        queue.upper_next_header,
    )
    .is_err()
    {
        return Ipv6ReassemblyResult::Dropped;
    }

    Ipv6ReassemblyResult::Complete(PacketBuf::from_ip_packet_vec_with_type(
        queue.arrival_ifindex,
        packet,
        PacketOwner::Ipv6Stack,
        queue.packet_type,
    ))
}

/// Linux `fq_find` stores `iif = 0` unless the destination is multicast or
/// link-local, so fragments of a global unicast datagram can complete across
/// interfaces.
fn reassembly_ifindex(dst_addr: Ipv6Address, ifindex: i32) -> i32 {
    if dst_addr.is_multicast() || dst_addr.is_link_local() {
        ifindex
    } else {
        0
    }
}

fn ecn_bit(ecn: u8) -> u8 {
    1 << ecn
}

fn reassembled_ecn(ecn_mask: u8, first_ecn: u8) -> Option<u8> {
    let has_not_ect = ecn_mask & ecn_bit(0) != 0;
    let has_ect_or_ce = ecn_mask & !ecn_bit(0) != 0;
    if has_not_ect && has_ect_or_ce {
        return None;
    }
    if ecn_mask & ecn_bit(3) != 0 {
        Some(3)
    } else {
        Some(first_ecn)
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::vec;

    use etherparse::{IpFragOffset, IpNumber, Ipv6FragmentHeader};
    use ktime_types::{MonotonicInstant, TimeSpan};
    use unittest::def_test;

    use super::*;
    use crate::{
        buf::PacketOwner,
        ip::Ipv6Address,
        ipv6::packet::{
            IPV6_EXT_HOP_BY_HOP, IPV6_HEADER_LEN, PROTOCOL_TCP, PROTOCOL_UDP, build_ipv6_packet,
            payload,
        },
    };

    const FRAG_ID: u32 = 0x1234_5678;

    fn sample_addrs() -> (Ipv6Address, Ipv6Address) {
        (
            Ipv6Address::from_octets([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
            Ipv6Address::from_octets([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]),
        )
    }

    fn frag_hdr(next_header: u8, offset: usize, more_fragments: bool) -> [u8; 8] {
        let offset_units = u16::try_from(offset / 8).unwrap();
        Ipv6FragmentHeader::new(
            IpNumber(next_header),
            IpFragOffset::try_new(offset_units).unwrap(),
            more_fragments,
            FRAG_ID,
        )
        .to_bytes()
    }

    fn make_fragment(
        dst: Ipv6Address,
        ifindex: i32,
        offset: usize,
        more_fragments: bool,
        payload: &[u8],
    ) -> (PacketBuf, Ipv6Header, Ipv6Fragment) {
        let (src, _) = sample_addrs();
        let mut body = frag_hdr(PROTOCOL_UDP, offset, more_fragments).to_vec();
        body.extend_from_slice(payload);
        let bytes = build_ipv6_packet(src, dst, super::super::packet::IPV6_EXT_FRAGMENT, 64, &body)
            .unwrap();
        let mut packet = PacketBuf::from_ip_packet_vec(ifindex, bytes, PacketOwner::DeviceRx);
        let header = Ipv6Header::validate_input_packet_allowing_fragment(&mut packet).unwrap();
        let fragment = Ipv6Fragment::parse(packet.network_packet().unwrap(), &header).unwrap();
        (packet, header, fragment)
    }

    fn reassemble(parts: &[(usize, bool, &[u8])]) -> Ipv6ReassemblyResult {
        let mut reassembler = Ipv6Reassembler::new();
        let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
        let (_, dst) = sample_addrs();
        let mut result = Ipv6ReassemblyResult::Pending;
        for (offset, more, payload) in parts {
            let (packet, header, fragment) = make_fragment(dst, 1, *offset, *more, payload);
            result = reassembler.reassemble(packet, header, fragment, now);
        }
        result
    }

    #[def_test]
    fn test_ipv6_reassembly_accepts_in_order_fragments() {
        let result = reassemble(&[(0, true, b"abcdefgh"), (8, false, b"ijkl")]);

        let Ipv6ReassemblyResult::Complete(packet) = result else {
            panic!("expected complete packet");
        };
        let ip_packet = packet.network_packet().unwrap();
        let header = Ipv6Header::parse_input(ip_packet).unwrap();
        assert_eq!(header.next_header(), PROTOCOL_UDP);
        assert_eq!(payload(ip_packet, &header).unwrap(), b"abcdefghijkl");
    }

    #[def_test]
    fn test_ipv6_reassembly_accepts_reverse_order_fragments() {
        let result = reassemble(&[(8, false, b"ijkl"), (0, true, b"abcdefgh")]);

        let Ipv6ReassemblyResult::Complete(packet) = result else {
            panic!("expected complete packet");
        };
        let ip_packet = packet.network_packet().unwrap();
        let header = Ipv6Header::parse_input(ip_packet).unwrap();
        assert_eq!(payload(ip_packet, &header).unwrap(), b"abcdefghijkl");
    }

    #[def_test]
    fn test_ipv6_reassembly_drops_overlapping_fragments() {
        let result = reassemble(&[(0, true, b"abcdefgh"), (0, false, b"abcdefghi")]);

        assert!(matches!(result, Ipv6ReassemblyResult::Dropped));
    }

    #[def_test]
    fn test_ipv6_reassembly_keeps_queue_on_duplicate_fragment() {
        let mut reassembler = Ipv6Reassembler::new();
        let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
        let (_, dst) = sample_addrs();
        let (packet, header, fragment) = make_fragment(dst, 1, 0, true, b"abcdefgh");
        assert!(matches!(
            reassembler.reassemble(packet, header, fragment, now),
            Ipv6ReassemblyResult::Pending
        ));
        let (packet, header, fragment) = make_fragment(dst, 1, 0, true, b"abcdefgh");
        assert!(matches!(
            reassembler.reassemble(packet, header, fragment, now),
            Ipv6ReassemblyResult::Pending
        ));
        let (packet, header, fragment) = make_fragment(dst, 1, 8, false, b"ijkl");
        assert!(matches!(
            reassembler.reassemble(packet, header, fragment, now),
            Ipv6ReassemblyResult::Complete(_)
        ));
    }

    #[def_test]
    fn test_ipv6_reassembly_expires_first_fragment() {
        let mut reassembler = Ipv6Reassembler::new();
        let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
        let (_, dst) = sample_addrs();
        let (packet, header, fragment) = make_fragment(dst, 1, 0, true, b"abcdefgh");
        assert!(matches!(
            reassembler.reassemble(packet, header, fragment, now),
            Ipv6ReassemblyResult::Pending
        ));

        let expired = reassembler.remove_expired(now + IPV6_REASSEMBLY_TIMEOUT);

        assert_eq!(expired.len(), 1);
        let expired_frag =
            Ipv6Fragment::parse(&expired[0].packet, &expired[0].header).expect("first fragment");
        assert_eq!(expired_frag.offset, 0);
        assert_eq!(
            expired_frag
                .payload(&expired[0].packet, &expired[0].header)
                .unwrap(),
            b"abcdefgh"
        );
    }

    #[def_test]
    fn test_ipv6_reassembly_timeout_without_first_fragment_sends_no_icmp() {
        let mut reassembler = Ipv6Reassembler::new();
        let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
        let (_, dst) = sample_addrs();
        let (packet, header, fragment) = make_fragment(dst, 1, 8, false, b"ijkl");
        assert!(matches!(
            reassembler.reassemble(packet, header, fragment, now),
            Ipv6ReassemblyResult::Pending
        ));

        let expired = reassembler.remove_expired(now + IPV6_REASSEMBLY_TIMEOUT);
        assert!(expired.is_empty());
    }

    #[def_test]
    fn test_ipv6_reassembly_keeps_unfragmentable_hop_by_hop() {
        let mut reassembler = Ipv6Reassembler::new();
        let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
        let (src, dst) = sample_addrs();

        let mut first = vec![super::super::packet::IPV6_EXT_FRAGMENT, 0, 0, 0, 0, 0, 0, 0];
        first.extend_from_slice(&frag_hdr(PROTOCOL_UDP, 0, true));
        first.extend_from_slice(b"abcdefgh");
        let bytes = build_ipv6_packet(src, dst, IPV6_EXT_HOP_BY_HOP, 64, &first).unwrap();
        let mut packet = PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceRx);
        let header = Ipv6Header::validate_input_packet_allowing_fragment(&mut packet).unwrap();
        let fragment = Ipv6Fragment::parse(packet.network_packet().unwrap(), &header).unwrap();
        assert!(matches!(
            reassembler.reassemble(packet, header, fragment, now),
            Ipv6ReassemblyResult::Pending
        ));

        let mut second = vec![super::super::packet::IPV6_EXT_FRAGMENT, 0, 0, 0, 0, 0, 0, 0];
        second.extend_from_slice(&frag_hdr(PROTOCOL_UDP, 8, false));
        second.extend_from_slice(b"ijkl");
        let bytes = build_ipv6_packet(src, dst, IPV6_EXT_HOP_BY_HOP, 64, &second).unwrap();
        let mut packet = PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceRx);
        let header = Ipv6Header::validate_input_packet_allowing_fragment(&mut packet).unwrap();
        let fragment = Ipv6Fragment::parse(packet.network_packet().unwrap(), &header).unwrap();
        let Ipv6ReassemblyResult::Complete(packet) =
            reassembler.reassemble(packet, header, fragment, now)
        else {
            panic!("expected complete packet");
        };

        let ip_packet = packet.network_packet().unwrap();
        let header = Ipv6Header::parse_input(ip_packet).unwrap();
        assert_eq!(header.next_header(), PROTOCOL_UDP);
        assert_eq!(header.header_len(), IPV6_HEADER_LEN + 8);
        assert_eq!(payload(ip_packet, &header).unwrap(), b"abcdefghijkl");
        assert_eq!(ip_packet[6], IPV6_EXT_HOP_BY_HOP);
        assert_eq!(ip_packet[40], PROTOCOL_UDP);
    }

    #[def_test]
    fn test_ipv6_reassembly_key_ignores_ifindex_for_global_unicast() {
        let mut reassembler = Ipv6Reassembler::new();
        let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
        let (_, dst) = sample_addrs();
        let (packet, header, fragment) = make_fragment(dst, 1, 0, true, b"abcdefgh");
        assert!(matches!(
            reassembler.reassemble(packet, header, fragment, now),
            Ipv6ReassemblyResult::Pending
        ));
        let (packet, header, fragment) = make_fragment(dst, 2, 8, false, b"ijkl");
        assert!(matches!(
            reassembler.reassemble(packet, header, fragment, now),
            Ipv6ReassemblyResult::Complete(_)
        ));
    }

    #[def_test]
    fn test_ipv6_reassembly_key_includes_ifindex_for_link_local() {
        let mut reassembler = Ipv6Reassembler::new();
        let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
        let dst = Ipv6Address::from_octets([0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        let (packet, header, fragment) = make_fragment(dst, 1, 0, true, b"abcdefgh");
        assert!(matches!(
            reassembler.reassemble(packet, header, fragment, now),
            Ipv6ReassemblyResult::Pending
        ));
        let (packet, header, fragment) = make_fragment(dst, 2, 8, false, b"ijkl");
        assert!(matches!(
            reassembler.reassemble(packet, header, fragment, now),
            Ipv6ReassemblyResult::Pending
        ));
    }

    #[def_test]
    fn test_ipv6_reassembly_does_not_leave_fragment_header_on_tcp_payload() {
        let mut reassembler = Ipv6Reassembler::new();
        let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
        let (src, dst) = sample_addrs();
        let mut first = frag_hdr(PROTOCOL_TCP, 0, true).to_vec();
        first.extend_from_slice(&[0; 8]);
        let bytes = build_ipv6_packet(
            src,
            dst,
            super::super::packet::IPV6_EXT_FRAGMENT,
            64,
            &first,
        )
        .unwrap();
        let mut packet = PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceRx);
        let header = Ipv6Header::validate_input_packet_allowing_fragment(&mut packet).unwrap();
        let fragment = Ipv6Fragment::parse(packet.network_packet().unwrap(), &header).unwrap();
        assert!(matches!(
            reassembler.reassemble(packet, header, fragment, now),
            Ipv6ReassemblyResult::Pending
        ));

        let mut second = frag_hdr(PROTOCOL_TCP, 8, false).to_vec();
        second.extend_from_slice(&[0; 12]);
        let bytes = build_ipv6_packet(
            src,
            dst,
            super::super::packet::IPV6_EXT_FRAGMENT,
            64,
            &second,
        )
        .unwrap();
        let mut packet = PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceRx);
        let header = Ipv6Header::validate_input_packet_allowing_fragment(&mut packet).unwrap();
        let fragment = Ipv6Fragment::parse(packet.network_packet().unwrap(), &header).unwrap();
        let Ipv6ReassemblyResult::Complete(packet) =
            reassembler.reassemble(packet, header, fragment, now)
        else {
            panic!("expected complete packet");
        };
        let ip_packet = packet.network_packet().unwrap();
        let header = Ipv6Header::parse_input(ip_packet).unwrap();
        assert_eq!(header.next_header(), PROTOCOL_TCP);
        assert_eq!(payload(ip_packet, &header).unwrap().len(), 20);
    }
}
