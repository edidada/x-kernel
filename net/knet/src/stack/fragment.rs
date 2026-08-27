// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 fragment reassembly for local input.
//!
//! [`Ipv4Reassembler`] groups fragments by addresses, identification,
//! protocol, and input interface. Each queue stores non-overlapping payload
//! ranges in offset order until the first header and complete payload are
//! available. [`crate::router::Router`] drives insertion and timeout removal,
//! then feeds complete packets back through IPv4 input validation.
//!
//! Queue count, retained bytes, and lifetime are bounded. Expired queues retain
//! enough of the first fragment to build an ICMPv4 reassembly-timeout response.

use alloc::{
    collections::{BTreeMap, btree_map::Entry},
    vec::Vec,
};

use ktime_types::{MonotonicInstant, TimeSpan};

use super::ipv4::{self, Ipv4Header};
use crate::buf::{PacketBuf, PacketOwner, PacketType};

const IPV4_REASSEMBLY_TIMEOUT: TimeSpan = TimeSpan::from_secs(30);
const IPV4_REASSEMBLY_HIGH_BYTES: usize = 4 * 1024 * 1024;
const IPV4_REASSEMBLY_LOW_BYTES: usize = 3 * 1024 * 1024;
const IPV4_REASSEMBLY_MAX_QUEUES: usize = 64;
const IPV4_FRAGMENT_OFFSET_UNIT: usize = 8;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Ipv4FragKey {
    src_addr: [u8; 4],
    dst_addr: [u8; 4],
    identification: u16,
    protocol: u8,
    ifindex: i32,
}

#[derive(Debug)]
struct Ipv4ReassemblyQueue {
    packet_type: PacketType,
    first_header: Option<Vec<u8>>,
    first_ecn: Option<u8>,
    fragments: BTreeMap<usize, Vec<u8>>,
    received_len: usize,
    total_payload_len: Option<usize>,
    ecn_mask: u8,
    expires_at: MonotonicInstant,
    memory_bytes: usize,
}

pub(crate) struct ExpiredIpv4Fragment {
    pub(crate) packet_type: PacketType,
    pub(crate) header: Ipv4Header,
    pub(crate) packet: Vec<u8>,
}

pub(crate) enum Ipv4ReassemblyResult {
    Complete(PacketBuf),
    Pending,
    Dropped,
}

#[derive(Default)]
pub(crate) struct Ipv4Reassembler {
    queues: BTreeMap<Ipv4FragKey, Ipv4ReassemblyQueue>,
    memory_bytes: usize,
    next_expiration: Option<MonotonicInstant>,
}

enum InsertResult {
    Inserted { memory_bytes: usize },
    Duplicate,
    InvalidRange,
    Overlap,
}

impl Ipv4Reassembler {
    pub(crate) fn new() -> Self {
        Self {
            queues: BTreeMap::new(),
            memory_bytes: 0,
            next_expiration: None,
        }
    }

    pub(crate) fn next_expiration(&self) -> Option<MonotonicInstant> {
        self.next_expiration
    }

    pub(crate) fn reassemble(
        &mut self,
        packet: PacketBuf,
        header: Ipv4Header,
        now: MonotonicInstant,
    ) -> Ipv4ReassemblyResult {
        let key = Ipv4FragKey {
            src_addr: header.src_addr().octets(),
            dst_addr: header.dst_addr().octets(),
            identification: header.identification(),
            protocol: header.protocol(),
            ifindex: packet.ifindex(),
        };
        let Some(ip_packet) = packet.network_packet() else {
            return Ipv4ReassemblyResult::Dropped;
        };
        let Some(payload) = ipv4::payload(ip_packet, &header) else {
            self.remove_queue(&key);
            return Ipv4ReassemblyResult::Dropped;
        };

        let offset = header.fragment_offset();
        let payload_len = aligned_payload_len(payload.len(), header.more_fragments());
        if payload_len == 0 {
            self.remove_queue(&key);
            return Ipv4ReassemblyResult::Dropped;
        }
        let end = match offset.checked_add(payload_len) {
            Some(end) => end,
            None => {
                self.remove_queue(&key);
                return Ipv4ReassemblyResult::Dropped;
            }
        };
        let fragment_payload = payload[..payload_len].to_vec();
        let queue = match self.queues.entry(key) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let expires_at = now + IPV4_REASSEMBLY_TIMEOUT;
                self.next_expiration = Some(
                    self.next_expiration
                        .map_or(expires_at, |current| current.min(expires_at)),
                );
                entry.insert(Ipv4ReassemblyQueue {
                    packet_type: packet.packet_type(),
                    first_header: None,
                    first_ecn: None,
                    fragments: BTreeMap::new(),
                    received_len: 0,
                    total_payload_len: None,
                    ecn_mask: 0,
                    expires_at,
                    memory_bytes: 0,
                })
            }
        };

        if !header.more_fragments() {
            if queue
                .total_payload_len
                .is_some_and(|old_total| old_total != end)
                || queue
                    .last_fragment_end()
                    .is_some_and(|old_end| old_end > end)
            {
                self.remove_queue(&key);
                return Ipv4ReassemblyResult::Dropped;
            }
            queue.total_payload_len = Some(end);
        } else if queue
            .total_payload_len
            .is_some_and(|total_payload_len| end > total_payload_len)
        {
            self.remove_queue(&key);
            return Ipv4ReassemblyResult::Dropped;
        }

        match queue.insert_fragment(offset, fragment_payload) {
            InsertResult::Inserted { memory_bytes } => {
                queue.memory_bytes = queue.memory_bytes.saturating_add(memory_bytes);
                self.memory_bytes = self.memory_bytes.saturating_add(memory_bytes);
            }
            InsertResult::Duplicate => return Ipv4ReassemblyResult::Pending,
            InsertResult::InvalidRange | InsertResult::Overlap => {
                self.remove_queue(&key);
                return Ipv4ReassemblyResult::Dropped;
            }
        }
        queue.ecn_mask |= ecn_bit(header.ecn());
        if offset == 0 && queue.first_header.is_none() {
            let first_header = ip_packet[..header.header_len()].to_vec();
            queue.memory_bytes = queue.memory_bytes.saturating_add(first_header.len());
            self.memory_bytes = self.memory_bytes.saturating_add(first_header.len());
            queue.first_header = Some(first_header);
            queue.first_ecn = Some(header.ecn());
        }

        if queue.is_complete() {
            let queue = self.remove_queue(&key).expect("queue exists after insert");
            return build_reassembled_packet(key.ifindex, queue);
        }

        self.enforce_limits();
        Ipv4ReassemblyResult::Pending
    }

    pub(crate) fn remove_expired(&mut self, now: MonotonicInstant) -> Vec<ExpiredIpv4Fragment> {
        if self
            .next_expiration
            .is_none_or(|expires_at| now < expires_at)
        {
            return Vec::new();
        }

        let keys: Vec<_> = self
            .queues
            .iter()
            .filter_map(|(key, queue)| (now >= queue.expires_at).then_some(*key))
            .collect();
        let expired = keys
            .into_iter()
            .filter_map(|key| {
                let queue = self.remove_queue(&key)?;
                let first_packet = queue.first_fragment_packet()?;
                let header = Ipv4Header::parse_input(&first_packet).ok()?;
                Some(ExpiredIpv4Fragment {
                    packet_type: queue.packet_type,
                    header,
                    packet: first_packet,
                })
            })
            .collect();
        // Repair a stale cache when the stored time is due but no remaining
        // queue matches it. Otherwise `has_more` would stay true forever.
        self.refresh_next_expiration();
        expired
    }

    fn enforce_limits(&mut self) {
        while self.queues.len() > IPV4_REASSEMBLY_MAX_QUEUES {
            let Some(key) = self.oldest_queue_key() else {
                return;
            };
            self.remove_queue(&key);
        }

        while self.memory_bytes > IPV4_REASSEMBLY_HIGH_BYTES {
            let Some(key) = self.oldest_queue_key() else {
                return;
            };
            self.remove_queue(&key);
            if self.memory_bytes <= IPV4_REASSEMBLY_LOW_BYTES {
                break;
            }
        }
    }

    fn oldest_queue_key(&self) -> Option<Ipv4FragKey> {
        self.queues
            .iter()
            .min_by_key(|(_, queue)| queue.expires_at)
            .map(|(key, _)| *key)
    }

    fn remove_queue(&mut self, key: &Ipv4FragKey) -> Option<Ipv4ReassemblyQueue> {
        let queue = self.queues.remove(key)?;
        self.memory_bytes = self.memory_bytes.saturating_sub(queue.memory_bytes);
        if self.next_expiration == Some(queue.expires_at) {
            self.refresh_next_expiration();
        }
        Some(queue)
    }

    fn refresh_next_expiration(&mut self) {
        self.next_expiration = self.queues.values().map(|queue| queue.expires_at).min();
    }
}

impl Ipv4ReassemblyQueue {
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

fn build_reassembled_packet(ifindex: i32, queue: Ipv4ReassemblyQueue) -> Ipv4ReassemblyResult {
    let Some(first_header) = queue.first_header else {
        return Ipv4ReassemblyResult::Dropped;
    };
    let Some(total_payload_len) = queue.total_payload_len else {
        return Ipv4ReassemblyResult::Dropped;
    };
    let Some(first_ecn) = queue.first_ecn else {
        return Ipv4ReassemblyResult::Dropped;
    };
    let Some(ecn) = reassembled_ecn(queue.ecn_mask, first_ecn) else {
        return Ipv4ReassemblyResult::Dropped;
    };
    let packet_len = match first_header.len().checked_add(total_payload_len) {
        Some(packet_len) if packet_len <= u16::MAX as usize => packet_len,
        _ => return Ipv4ReassemblyResult::Dropped,
    };

    let mut packet = alloc::vec![0u8; packet_len];
    packet[..first_header.len()].copy_from_slice(&first_header);
    for (offset, payload) in queue.fragments {
        let start = first_header.len() + offset;
        let end = start + payload.len();
        packet[start..end].copy_from_slice(&payload);
    }
    if ipv4::rewrite_reassembled_header(&mut packet, total_payload_len, ecn).is_err() {
        return Ipv4ReassemblyResult::Dropped;
    }

    Ipv4ReassemblyResult::Complete(PacketBuf::from_ip_packet_vec_with_type(
        ifindex,
        packet,
        PacketOwner::Ipv4Stack,
        queue.packet_type,
    ))
}

fn aligned_payload_len(payload_len: usize, more_fragments: bool) -> usize {
    if more_fragments {
        payload_len / IPV4_FRAGMENT_OFFSET_UNIT * IPV4_FRAGMENT_OFFSET_UNIT
    } else {
        payload_len
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
    use alloc::vec::Vec;

    use etherparse::{IpFragOffset, IpNumber, Ipv4Header as EtherIpv4Header};
    use ktime_types::{MonotonicInstant, TimeSpan};
    use unittest::def_test;

    use super::*;
    use crate::{buf::PacketOwner, ipv4};

    fn fragment(offset: usize, more_fragments: bool, payload: &[u8]) -> (PacketBuf, Ipv4Header) {
        fragment_with_id(0x1234, offset, more_fragments, payload)
    }

    fn fragment_with_id(
        identification: u16,
        offset: usize,
        more_fragments: bool,
        payload: &[u8],
    ) -> (PacketBuf, Ipv4Header) {
        let offset_units = u16::try_from(offset / 8).unwrap();
        let mut header = EtherIpv4Header::new(
            payload.len() as u16,
            64,
            IpNumber(ipv4::PROTOCOL_UDP),
            [192, 0, 2, 1],
            [192, 0, 2, 2],
        )
        .unwrap();
        header.identification = identification;
        header.dont_fragment = false;
        header.more_fragments = more_fragments;
        header.fragment_offset = IpFragOffset::try_new(offset_units).unwrap();
        header.header_checksum = header.calc_header_checksum();

        let mut bytes: Vec<u8> = header.to_bytes().into_iter().collect();
        bytes.extend_from_slice(payload);
        let mut packet = PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceRx);
        let header = Ipv4Header::validate_input_packet(&mut packet).unwrap();
        (packet, header)
    }

    fn reassemble(parts: &[(usize, bool, &[u8])]) -> Ipv4ReassemblyResult {
        let mut reassembler = Ipv4Reassembler::new();
        let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
        let mut result = Ipv4ReassemblyResult::Pending;
        for (offset, more, payload) in parts {
            let (packet, header) = fragment(*offset, *more, payload);
            result = reassembler.reassemble(packet, header, now);
        }
        result
    }

    #[def_test]
    fn test_ipv4_reassembly_accepts_in_order_fragments() {
        let result = reassemble(&[(0, true, b"abcdefgh"), (8, false, b"ijkl")]);

        let Ipv4ReassemblyResult::Complete(packet) = result else {
            panic!("expected complete packet");
        };
        let ip_packet = packet.network_packet().unwrap();
        let header = Ipv4Header::parse_input(ip_packet).unwrap();
        assert!(!header.is_fragmented());
        assert_eq!(ipv4::payload(ip_packet, &header).unwrap(), b"abcdefghijkl");
    }

    #[def_test]
    fn test_ipv4_reassembly_accepts_reverse_order_fragments() {
        let result = reassemble(&[(8, false, b"ijkl"), (0, true, b"abcdefgh")]);

        let Ipv4ReassemblyResult::Complete(packet) = result else {
            panic!("expected complete packet");
        };
        let ip_packet = packet.network_packet().unwrap();
        let header = Ipv4Header::parse_input(ip_packet).unwrap();
        assert_eq!(ipv4::payload(ip_packet, &header).unwrap(), b"abcdefghijkl");
    }

    #[def_test]
    fn test_ipv4_reassembly_drops_overlapping_fragments() {
        let result = reassemble(&[(0, true, b"abcdefgh"), (0, false, b"abcdefghi")]);

        assert!(matches!(result, Ipv4ReassemblyResult::Dropped));
    }

    #[def_test]
    fn test_ipv4_reassembly_keeps_queue_on_duplicate_fragment() {
        let mut reassembler = Ipv4Reassembler::new();
        let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
        let (packet, header) = fragment(0, true, b"abcdefgh");
        assert!(matches!(
            reassembler.reassemble(packet, header, now),
            Ipv4ReassemblyResult::Pending
        ));
        let (packet, header) = fragment(0, true, b"abcdefgh");
        assert!(matches!(
            reassembler.reassemble(packet, header, now),
            Ipv4ReassemblyResult::Pending
        ));
        let (packet, header) = fragment(8, false, b"ijkl");
        assert!(matches!(
            reassembler.reassemble(packet, header, now),
            Ipv4ReassemblyResult::Complete(_)
        ));
        assert_eq!(reassembler.next_expiration(), None);
    }

    #[def_test]
    fn test_ipv4_reassembly_deadline_is_fixed_at_queue_creation() {
        let mut reassembler = Ipv4Reassembler::new();
        let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
        let (packet, header) = fragment(0, true, b"abcdefgh");
        assert!(matches!(
            reassembler.reassemble(packet, header, now),
            Ipv4ReassemblyResult::Pending
        ));
        let expires_at = now + IPV4_REASSEMBLY_TIMEOUT;
        assert_eq!(reassembler.next_expiration(), Some(expires_at));

        let (packet, header) = fragment(0, true, b"abcdefgh");
        assert!(matches!(
            reassembler.reassemble(packet, header, now + TimeSpan::from_secs(1)),
            Ipv4ReassemblyResult::Pending
        ));
        assert_eq!(reassembler.next_expiration(), Some(expires_at));
    }

    #[def_test]
    fn test_ipv4_reassembly_expires_first_fragment() {
        let mut reassembler = Ipv4Reassembler::new();
        let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
        let (packet, header) = fragment(0, true, b"abcdefgh");
        assert!(matches!(
            reassembler.reassemble(packet, header, now),
            Ipv4ReassemblyResult::Pending
        ));

        let expired = reassembler.remove_expired(now + IPV4_REASSEMBLY_TIMEOUT);

        assert_eq!(expired.len(), 1);
        assert_eq!(reassembler.next_expiration(), None);
        assert_eq!(expired[0].header.fragment_offset(), 0);
        assert_eq!(
            ipv4::payload(&expired[0].packet, &expired[0].header).unwrap(),
            b"abcdefgh"
        );
    }

    #[def_test]
    fn test_ipv4_reassembly_expire_advances_to_the_next_queue_deadline() {
        let mut reassembler = Ipv4Reassembler::new();
        let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
        let (packet, header) = fragment_with_id(0x1234, 0, true, b"abcdefgh");
        assert!(matches!(
            reassembler.reassemble(packet, header, now),
            Ipv4ReassemblyResult::Pending
        ));
        let later = now + TimeSpan::from_secs(5);
        let (packet, header) = fragment_with_id(0x1235, 0, true, b"ijklmnop");
        assert!(matches!(
            reassembler.reassemble(packet, header, later),
            Ipv4ReassemblyResult::Pending
        ));

        let expired = reassembler.remove_expired(now + IPV4_REASSEMBLY_TIMEOUT);

        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].header.identification(), 0x1234);
        assert_eq!(
            reassembler.next_expiration(),
            Some(later + IPV4_REASSEMBLY_TIMEOUT)
        );
    }

    #[def_test]
    fn test_ipv4_reassembly_repairs_stale_expiration_cache() {
        let mut reassembler = Ipv4Reassembler::new();
        let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
        let (packet, header) = fragment(0, true, b"abcdefgh");
        assert!(matches!(
            reassembler.reassemble(packet, header, now),
            Ipv4ReassemblyResult::Pending
        ));
        let expires_at = now + IPV4_REASSEMBLY_TIMEOUT;
        reassembler.next_expiration = Some(now);

        let expired = reassembler.remove_expired(now);

        assert!(expired.is_empty());
        assert_eq!(reassembler.next_expiration(), Some(expires_at));
    }
}
