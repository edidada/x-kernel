// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Reusable storage that preserves packet ownership and admission order across RX phases.

use alloc::vec::{Drain, Vec};

use super::admission::AdmittedIpv4Packet;
use crate::buf::PacketBuf;

/// Reusable RX storage for admission under the input lock and delivery after release.
pub(crate) struct IngressBatch {
    pub(crate) packets: Vec<PacketBuf>,
    admitted: Vec<AdmittedIpPacket>,
}

impl IngressBatch {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            packets: Vec::with_capacity(capacity),
            admitted: Vec::with_capacity(capacity),
        }
    }

    /// Reserves staging space before the processor acquires the input lock.
    pub(super) fn reserve_admitted(&mut self) {
        self.admitted.reserve(self.packets.len());
    }

    /// Applies admission in input order while retaining the staging allocation.
    ///
    /// Call only after acquiring the input lock so contention retains all raw packets.
    pub(super) fn admit_packets(
        &mut self,
        mut admit_fn: impl FnMut(PacketBuf) -> Option<AdmittedIpPacket>,
    ) {
        for packet in self.packets.drain(..) {
            if let Some(packet) = admit_fn(packet) {
                self.admitted.push(packet);
            }
        }
    }

    /// Transfers admitted packets to delivery after the input lock is released.
    pub(super) fn drain_admitted(&mut self) -> Drain<'_, AdmittedIpPacket> {
        self.admitted.drain(..)
    }
}

pub(super) enum AdmittedIpPacket {
    Ipv4(AdmittedIpv4Packet),
    Ipv6(PacketBuf),
}
