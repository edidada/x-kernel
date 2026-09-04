// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Packet buffer owned by the in-kernel network stack.

use alloc::{sync::Arc, vec::Vec};
use core::ops::Range;

/// Link-layer packet type used by packet sockets and link input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PacketType {
    Host,
    Broadcast,
    Multicast,
    OtherHost,
}

/// A reference-counted packet buffer with generic packet metadata.
#[derive(Clone, Debug)]
pub(crate) struct PacketBuf {
    inner: Arc<PacketBufInner>,
}

#[derive(Clone, Debug)]
struct PacketBufInner {
    data: Vec<u8>,
    // Reserved for packet headroom and future pull/push operations.
    head: usize,
    tail: usize,
    ifindex: i32,
    packet_type: PacketType,
    network_offset: Option<usize>,
}

impl PacketBuf {
    /// Creates a buffer from owned bytes and layer-derived packet metadata.
    ///
    /// `network_offset` is measured from the start of `data` and must not
    /// exceed its length.
    pub(crate) fn new(
        data: Vec<u8>,
        ifindex: i32,
        packet_type: PacketType,
        network_offset: Option<usize>,
    ) -> Self {
        let tail = data.len();
        debug_assert!(network_offset.is_none_or(|offset| offset <= tail));

        Self {
            inner: Arc::new(PacketBufInner {
                data,
                head: 0,
                tail,
                ifindex,
                packet_type,
                network_offset,
            }),
        }
    }

    pub(crate) fn ifindex(&self) -> i32 {
        self.inner.ifindex
    }

    /// Updates the network interface index using copy-on-write when the packet is shared.
    pub(crate) fn set_ifindex(&mut self, ifindex: i32) {
        Arc::make_mut(&mut self.inner).ifindex = ifindex;
    }

    pub(crate) fn packet_type(&self) -> PacketType {
        self.inner.packet_type
    }

    pub(crate) fn data(&self) -> &[u8] {
        &self.inner.data[self.data_range()]
    }

    pub(crate) fn network_packet(&self) -> Option<&[u8]> {
        let offset = self.inner.network_offset?;
        self.inner.data.get(offset..self.inner.tail)
    }

    pub(crate) fn network_packet_mut(&mut self) -> Option<&mut [u8]> {
        let offset = self.inner.network_offset?;
        let inner = Arc::make_mut(&mut self.inner);
        inner.data.get_mut(offset..inner.tail)
    }

    /// Sets the valid network packet length within the backing storage.
    pub(crate) fn truncate_network_packet(&mut self, len: usize) -> Option<()> {
        let offset = self.inner.network_offset?;
        let tail = offset.checked_add(len)?;
        if tail > self.inner.data.len() {
            return None;
        }
        let inner = Arc::make_mut(&mut self.inner);
        inner.tail = tail;
        Some(())
    }

    #[cfg(unittest)]
    pub(crate) fn shares_storage_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    fn data_range(&self) -> Range<usize> {
        self.inner.head..self.inner.tail
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::vec;

    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_network_packet_view() {
        let data = [1, 2, 3, 4];
        for offset in [None, Some(0), Some(2), Some(data.len())] {
            let packet = PacketBuf::new(data.to_vec(), 3, PacketType::Multicast, offset);

            assert_eq!(packet.ifindex(), 3);
            assert_eq!(packet.packet_type(), PacketType::Multicast);
            assert_eq!(packet.data(), data);
            assert_eq!(
                packet.network_packet(),
                offset.map(|offset| &data[offset..])
            );
        }
    }

    #[def_test]
    fn test_packet_clone_uses_copy_on_write() {
        let packet = PacketBuf::new(vec![0x45, 0, 0, 20], 1, PacketType::Host, Some(0));
        let mut copy = packet.clone();

        assert!(packet.shares_storage_with(&copy));

        copy.network_packet_mut().unwrap()[0] = 0x60;

        assert!(!packet.shares_storage_with(&copy));
        assert_eq!(packet.network_packet().unwrap()[0], 0x45);
        assert_eq!(copy.network_packet().unwrap()[0], 0x60);
    }
}
