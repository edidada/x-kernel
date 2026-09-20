// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Receive context layered over shared packet bytes.

use super::buf::PacketBuf;

/// Link-layer destination classification retained through ingress processing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PacketType {
    Host,
    Broadcast,
    Multicast,
    OtherHost,
}

/// Bytes received on one interface with the context upper layers require.
///
/// The view keeps the complete link-layer frame. `network_offset` marks where
/// the network-layer header starts inside it, and `None` denotes a frame
/// without a network view, such as ARP.
#[derive(Clone, Debug)]
pub(crate) struct RxPacket {
    packet: PacketBuf,
    ingress_ifindex: i32,
    packet_type: PacketType,
    network_offset: Option<usize>,
}

impl RxPacket {
    /// Attaches receive context to a byte view.
    ///
    /// `network_offset` is relative to this view's start and must not exceed
    /// its length when present.
    pub(crate) fn new(
        packet: PacketBuf,
        ingress_ifindex: i32,
        packet_type: PacketType,
        network_offset: Option<usize>,
    ) -> Self {
        debug_assert!(network_offset.is_none_or(|offset| offset <= packet.len()));
        Self {
            packet,
            ingress_ifindex,
            packet_type,
            network_offset,
        }
    }

    pub(crate) fn ingress_ifindex(&self) -> i32 {
        self.ingress_ifindex
    }

    pub(crate) fn packet_type(&self) -> PacketType {
        self.packet_type
    }

    /// Returns the complete link-layer frame.
    pub(crate) fn data(&self) -> &[u8] {
        self.packet.as_slice()
    }

    /// Returns the network-layer packet when this frame carries one.
    pub(crate) fn network_packet(&self) -> Option<&[u8]> {
        self.data().get(self.network_offset?..)
    }

    /// Shortens the network view to `len` bytes while retaining the frame prefix.
    ///
    /// Returns `None` for an unchecked length or a frame without a network
    /// view, leaving the current view unchanged. Shared bytes are never copied.
    pub(crate) fn truncate_network_packet(&mut self, len: usize) -> Option<()> {
        let end = self.network_offset?.checked_add(len)?;
        if end > self.packet.len() {
            return None;
        }
        self.packet.trim(0..end)
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

    #[def_test]
    fn receive_context_and_view_trimming_keep_shared_bytes() {
        let frame = PacketBuf::new(vec![0, 1, 2, 3, 4, 5]);
        let mut network_view = frame.clone();
        network_view.trim(1..5).unwrap();
        let mut packet = RxPacket::new(network_view, 1, PacketType::Host, Some(2));
        let other = RxPacket::new(frame, 2, PacketType::Broadcast, Some(0));

        assert_eq!(packet.data(), &[1, 2, 3, 4]);
        assert_eq!(packet.network_packet(), Some(&[3, 4][..]));
        assert_eq!(packet.truncate_network_packet(1), Some(()));
        assert_eq!(packet.data(), &[1, 2, 3]);
        assert_eq!(packet.network_packet(), Some(&[3][..]));
        assert_eq!(packet.ingress_ifindex(), 1);
        assert_eq!(other.ingress_ifindex(), 2);
        assert_eq!(other.packet_type(), PacketType::Broadcast);
        assert_eq!(other.network_packet(), Some(&[0, 1, 2, 3, 4, 5][..]));
        assert!(packet.shares_storage_with(&other));

        for len in [usize::MAX, 2, 3] {
            assert_eq!(packet.truncate_network_packet(len), None);
            assert_eq!(packet.data(), &[1, 2, 3]);
            assert!(packet.shares_storage_with(&other));
        }
    }

    #[def_test]
    fn receive_network_offsets_cover_empty_and_non_ip_views() {
        for offset in [None, Some(0), Some(2), Some(4)] {
            let packet = RxPacket::new(
                PacketBuf::new(vec![1, 2, 3, 4]),
                3,
                PacketType::Multicast,
                offset,
            );

            assert_eq!(
                packet.network_packet(),
                offset.map(|offset| &packet.data()[offset..])
            );
        }
    }
}
