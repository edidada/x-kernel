// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Local IPv4 admission and fragment reassembly state, serialized by the processor.

use alloc::vec::Vec;

use ktime_types::MonotonicInstant;

use crate::{
    buf::PacketBuf,
    ip::{Ipv4Address, Ipv4Cidr},
    network::{
        fragment::{Ipv4ExpirationBatch, Ipv4Reassembler, Ipv4ReassemblyResult},
        ipv4::{Ipv4Error, Ipv4Header},
    },
};

/// A validated, locally admitted IPv4 packet with reassembly complete.
///
/// Only admission can construct this value. The address decision remains valid
/// for delivery even if the configured addresses change after admission.
pub(super) struct AdmittedIpv4Packet {
    packet: PacketBuf,
    header: Ipv4Header,
}

impl AdmittedIpv4Packet {
    pub(super) fn into_parts(self) -> (PacketBuf, Ipv4Header) {
        (self.packet, self.header)
    }
}

/// Owns the address view and queues used to admit local IPv4 input.
pub(super) struct Ipv4Admission {
    local_ipv4_addrs: Vec<Ipv4Cidr>,
    ipv4_reassembler: Ipv4Reassembler,
}

impl Ipv4Admission {
    pub(super) fn new() -> Self {
        Self {
            local_ipv4_addrs: Vec::new(),
            ipv4_reassembler: Ipv4Reassembler::new(),
        }
    }

    pub(super) fn update_local_ipv4_addrs(&mut self, addrs: &[Ipv4Cidr]) {
        self.local_ipv4_addrs.clear();
        self.local_ipv4_addrs.extend_from_slice(addrs);
    }

    pub(super) fn reassembly_deadline(&self) -> Option<MonotonicInstant> {
        self.ipv4_reassembler.expiration_deadline()
    }

    /// Detaches expired fragments for response construction after releasing the input lock.
    pub(super) fn expire_fragments(
        &mut self,
        timestamp: MonotonicInstant,
        timer_budget: usize,
    ) -> Ipv4ExpirationBatch {
        self.ipv4_reassembler
            .remove_expired(timestamp, timer_budget)
    }

    /// Validates local input and completes reassembly before transport delivery.
    pub(super) fn admit_packet(
        &mut self,
        timestamp: MonotonicInstant,
        mut packet: PacketBuf,
    ) -> Option<AdmittedIpv4Packet> {
        let header = match Ipv4Header::validate_input_packet(&mut packet) {
            Ok(header) => header,
            Err(Ipv4Error::Malformed | Ipv4Error::BadChecksum) => return None,
        };
        if !self.is_local_ipv4_destination(header.dst_addr()) {
            return None;
        }
        if header.is_fragmented() {
            return match self.ipv4_reassembler.reassemble(packet, header, timestamp) {
                Ipv4ReassemblyResult::Complete(packet) => self.admit_packet(timestamp, packet),
                Ipv4ReassemblyResult::Pending | Ipv4ReassemblyResult::Dropped => None,
            };
        }
        Some(AdmittedIpv4Packet { packet, header })
    }

    fn is_local_ipv4_destination(&self, dst_addr: Ipv4Address) -> bool {
        dst_addr.is_broadcast()
            || dst_addr.is_multicast()
            || self
                .local_ipv4_addrs
                .iter()
                .any(|addr| addr.address() == dst_addr || addr.broadcast() == Some(dst_addr))
    }
}
