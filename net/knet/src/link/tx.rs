// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Transmit context layered over shared packet bytes.

use super::buf::PacketBuf;

/// Bytes queued for egress with an optional output-interface constraint.
///
/// The packet view starts at the network-layer header. `egress_ifindex` is
/// `None` when routing selects the device, and `Some` when a bound socket
/// restricts routing to that interface.
#[derive(Clone, Debug)]
pub(crate) struct TxPacket {
    packet: PacketBuf,
    egress_ifindex: Option<i32>,
}

impl TxPacket {
    /// Wraps bytes beginning at the IP header.
    ///
    /// `egress_ifindex` is `None` when routing selects the device and `Some`
    /// when the socket is bound to one interface.
    pub(crate) fn new(packet: PacketBuf, egress_ifindex: Option<i32>) -> Self {
        Self {
            packet,
            egress_ifindex,
        }
    }

    pub(crate) fn egress_ifindex(&self) -> Option<i32> {
        self.egress_ifindex
    }

    pub(crate) fn packet(&self) -> &PacketBuf {
        &self.packet
    }

    /// Extracts the byte view without copying bytes or allocating storage.
    pub(crate) fn into_packet(self) -> PacketBuf {
        self.packet
    }

    #[cfg(unittest)]
    pub(crate) fn shares_storage_with(&self, other: &Self) -> bool {
        self.packet.shares_storage_with(&other.packet)
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::vec;

    use unittest::def_test;

    use super::*;
    use crate::rx::{PacketType, RxPacket};

    #[def_test]
    fn transmit_and_loopback_handoff_keep_shared_bytes() {
        let packet = TxPacket::new(PacketBuf::new(vec![1, 2, 3]), Some(7));
        let mut view = packet.packet().clone();
        view.trim(1..3).unwrap();
        let trimmed = TxPacket::new(view, packet.egress_ifindex());

        assert_eq!(packet.packet().as_slice(), &[1, 2, 3]);
        assert_eq!(trimmed.packet().as_slice(), &[2, 3]);
        assert_eq!(trimmed.egress_ifindex(), Some(7));
        assert!(trimmed.shares_storage_with(&packet));

        // Loopback returns a transmission as ingress on the same interface.
        let rx = RxPacket::new(trimmed.packet().clone(), 2, PacketType::Host, Some(0));
        let other = RxPacket::new(packet.packet().clone(), 2, PacketType::Host, Some(0));
        assert_eq!(rx.ingress_ifindex(), 2);
        assert_eq!(rx.data(), &[2, 3]);
        assert_eq!(packet.egress_ifindex(), Some(7));
        assert!(rx.shares_storage_with(&other));

        let unique = trimmed.into_packet();
        assert!(unique.shares_storage_with(packet.packet()));
        assert_eq!(unique.as_slice(), &[2, 3]);
    }
}
