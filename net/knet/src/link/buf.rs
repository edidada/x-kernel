// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Packet buffer owned by the in-kernel network stack.

use alloc::{sync::Arc, vec::Vec};
use core::ops::Range;

use super::wire::{ETHERNET_HEADER_LEN, EtherType, MacAddress};

/// Link-layer packet type used by packet sockets and link input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PacketType {
    Host, // Destination MAC = local MAC.
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
    pub(crate) fn from_ip_packet_vec(ifindex: i32, data: Vec<u8>) -> Self {
        let tail = data.len();
        Self {
            inner: Arc::new(PacketBufInner {
                data,
                head: 0,
                tail,
                ifindex,
                packet_type: PacketType::Host,
                network_offset: Some(0),
            }),
        }
    }

    pub(crate) fn from_ip_packet_vec_with_type(
        ifindex: i32,
        data: Vec<u8>,
        packet_type: PacketType,
    ) -> Self {
        let mut packet = Self::from_ip_packet_vec(ifindex, data);
        Arc::make_mut(&mut packet.inner).packet_type = packet_type;
        packet
    }

    pub(crate) fn from_ethernet_frame(
        ifindex: i32,
        frame: &[u8],
        dst_addr: MacAddress,
        protocol: EtherType,
        local_addr: MacAddress,
    ) -> Self {
        // TODO: Parse 802.1Q and 802.1ad tags before deriving the network offset.
        let network_offset = match protocol {
            EtherType::Ipv4 | EtherType::Ipv6 => Some(ETHERNET_HEADER_LEN),
            _ => None,
        };

        Self {
            inner: Arc::new(PacketBufInner {
                data: frame.to_vec(),
                head: 0,
                tail: frame.len(),
                ifindex,
                packet_type: packet_type(dst_addr, local_addr),
                network_offset,
            }),
        }
    }

    pub(crate) fn ifindex(&self) -> i32 {
        self.inner.ifindex
    }

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

    #[cfg(unittest)]
    pub(crate) fn network_offset(&self) -> Option<usize> {
        self.inner.network_offset
    }

    pub(crate) fn network_packet_mut(&mut self) -> Option<&mut [u8]> {
        let offset = self.inner.network_offset?;
        let inner = Arc::make_mut(&mut self.inner);
        inner.data.get_mut(offset..inner.tail)
    }

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
    pub(crate) fn is_ip_packet(&self) -> bool {
        self.network_packet()
            .and_then(|packet| packet.first().map(|byte| byte >> 4))
            .is_some_and(|version| matches!(version, 4 | 6))
    }

    #[cfg(unittest)]
    pub(crate) fn shares_storage_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    fn data_range(&self) -> Range<usize> {
        self.inner.head..self.inner.tail
    }
}

fn packet_type(dst_addr: MacAddress, local_addr: MacAddress) -> PacketType {
    if dst_addr.is_broadcast() {
        PacketType::Broadcast
    } else if dst_addr.is_multicast() {
        PacketType::Multicast
    } else if dst_addr == local_addr {
        PacketType::Host
    } else {
        PacketType::OtherHost
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::vec;

    use unittest::def_test;

    use super::*;
    use crate::link::wire::{EtherType, MacAddress};

    const LOCAL_MAC: MacAddress = MacAddress([0x02, 0, 0, 0, 0, 1]);
    const REMOTE_MAC: MacAddress = MacAddress([0x02, 0, 0, 0, 0, 2]);

    #[def_test]
    fn test_ip_packet_records_network_offset() {
        let packet = PacketBuf::from_ip_packet_vec(
            2,
            vec![
                0x45, 0, 0, 20, 0, 0, 0, 0, 64, 17, 0, 0, 127, 0, 0, 1, 127, 0, 0, 1,
            ],
        );

        assert_eq!(packet.ifindex(), 2);
        assert_eq!(packet.network_offset(), Some(0));
        assert!(packet.is_ip_packet());
        assert_eq!(packet.network_packet().unwrap(), packet.data());
    }

    #[def_test]
    fn test_ethernet_frame_records_network_offset_and_packet_type() {
        let frame = [
            LOCAL_MAC.0[0],
            LOCAL_MAC.0[1],
            LOCAL_MAC.0[2],
            LOCAL_MAC.0[3],
            LOCAL_MAC.0[4],
            LOCAL_MAC.0[5],
            REMOTE_MAC.0[0],
            REMOTE_MAC.0[1],
            REMOTE_MAC.0[2],
            REMOTE_MAC.0[3],
            REMOTE_MAC.0[4],
            REMOTE_MAC.0[5],
            0x08,
            0x00,
            0x45,
            0,
        ];
        let packet =
            PacketBuf::from_ethernet_frame(3, &frame, LOCAL_MAC, EtherType::Ipv4, LOCAL_MAC);

        assert_eq!(packet.ifindex(), 3);
        assert_eq!(packet.packet_type(), PacketType::Host);
        assert_eq!(packet.network_offset(), Some(14));
        assert_eq!(packet.network_packet().unwrap(), &[0x45, 0]);
    }

    #[def_test]
    fn test_packet_handle_is_pointer_sized_and_uses_copy_on_write() {
        let packet = PacketBuf::from_ip_packet_vec(1, vec![0x45, 0, 0, 20]);
        let mut copy = packet.clone();

        assert_eq!(
            core::mem::size_of::<PacketBuf>(),
            core::mem::size_of::<usize>()
        );
        assert!(packet.shares_storage_with(&copy));

        copy.network_packet_mut().unwrap()[0] = 0x60;

        assert!(!packet.shares_storage_with(&copy));
        assert_eq!(packet.network_packet().unwrap()[0], 0x45);
        assert_eq!(copy.network_packet().unwrap()[0], 0x60);
    }
}
