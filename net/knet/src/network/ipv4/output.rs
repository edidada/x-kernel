// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 output selection, transactional preparation, and fragmentation.
use alloc::{vec, vec::Vec};

use kerrno::{KError, KResult, LinuxError};
use khal::time::monotonic_time;
use ktime_types::MonotonicInstant;

use crate::{
    buf::{PacketBuf, PacketOwner},
    datapath::NetIo,
    ip::IpAddress,
    network::{
        ipv4::{self, Ipv4Error, Ipv4Header},
        state::NetworkState,
    },
};
/// Socket-selected constraints validated under the IPv4 output transaction.
pub(crate) struct Ipv4SendOptions {
    pub(crate) bound_src: Option<IpAddress>,
    pub(crate) dst_addr: IpAddress,
    pub(crate) oif: i32,
    pub(crate) allow_broadcast: bool,
}
pub(crate) struct Ipv4Output {
    next_identification: u16,
}
impl Ipv4Output {
    pub(crate) fn new() -> Self {
        Self {
            next_identification: 1,
        }
    }

    /// Performs capacity preflight before preparation and packet ownership transfer.
    /// The caller holds the network transaction throughout this operation.
    pub fn prepare_and_send(
        &mut self,
        state: &mut NetworkState,
        io: &mut NetIo,
        options: Ipv4SendOptions,
        packet: &mut Option<Vec<u8>>,
        prepare: impl FnOnce(&mut [u8], IpAddress, Option<usize>) -> KResult,
    ) -> KResult {
        let Ipv4SendOptions {
            bound_src,
            dst_addr,
            oif,
            allow_broadcast,
        } = options;
        let dst_addr = &dst_addr;
        if state.interfaces.ipv4_dest_requires_broadcast(dst_addr) && !allow_broadcast {
            return Err(KError::from(LinuxError::EACCES));
        }
        let packet_len = packet.as_ref().ok_or(KError::InvalidInput)?.len();
        let route_source = state
            .routing
            .output_route_source(&state.interfaces, dst_addr, oif)?;
        let source_addr = bound_src.unwrap_or(route_source);
        let is_loopback = state
            .routing
            .is_loopback_destination(&state.interfaces, dst_addr);

        if !is_loopback {
            if !io.can_enqueue_tx_packet() {
                return Err(KError::WouldBlock);
            }

            let packet_count = output_ipv4_packet_count(state, dst_addr, packet_len)?;
            if !io.can_enqueue_tx_packets(packet_count) {
                return Err(KError::WouldBlock);
            }
        }

        let route_mtu = state.routing.route_mtu(&state.interfaces, dst_addr);
        prepare(
            packet.as_deref_mut().ok_or(KError::InvalidInput)?,
            source_addr,
            route_mtu,
        )?;
        let packet = packet.take().ok_or(KError::InvalidInput)?;
        if is_loopback {
            self.send_loopback(state, io, packet, monotonic_time())
        } else {
            self.send(state, io, packet, oif)
        }
    }

    pub fn send(
        &mut self,
        state: &NetworkState,
        io: &mut NetIo,
        packet: Vec<u8>,
        oif: i32,
    ) -> KResult {
        let packets = self.fragment(state, packet)?;
        if !io.can_enqueue_tx_packets(packets.len()) {
            return Err(KError::WouldBlock);
        }

        io.enqueue_tx(
            packets
                .into_iter()
                .map(|packet| PacketBuf::from_ip_packet_vec(oif, packet, PacketOwner::Ipv4Stack)),
        );
        Ok(())
    }

    /// Dispatches loopback output synchronously in task context.
    /// Loopback raises NetRx and drains it on BH enable; a full shared RX queue
    /// drops the packet while retaining the existing successful xmit result.
    pub(crate) fn send_loopback(
        &mut self,
        state: &mut NetworkState,
        io: &mut NetIo,
        packet: Vec<u8>,
        timestamp: MonotonicInstant,
    ) -> KResult {
        let packets = self.fragment(state, packet)?;
        for packet in packets {
            let buf = PacketBuf::from_ip_packet_vec(0, packet, PacketOwner::Ipv4Stack);
            // RX-ready hint for the poller, not send success. This path is
            // not the poller; a dropped loopback packet is still a successful
            // xmit, as documented above.
            let _poll_next = self.dispatch(state, io, buf, timestamp);
        }
        Ok(())
    }

    /// Selects an IPv4 egress device and returns whether transmission made RX ready.
    /// The result is a poller hint; dropped packets and Ethernet TX return false.
    pub(crate) fn dispatch(
        &mut self,
        state: &mut NetworkState,
        io: &mut NetIo,
        mut packet: PacketBuf,
        timestamp: MonotonicInstant,
    ) -> bool {
        let header = match Ipv4Header::prepare_output_packet(&mut packet) {
            Ok(header) => header,
            Err(Ipv4Error::Malformed | Ipv4Error::BadChecksum) => return false,
        };

        let src_addr = IpAddress::Ipv4(header.src_addr());
        let dst_addr = IpAddress::Ipv4(header.dst_addr());
        if header.dst_addr().is_broadcast() {
            if !(header.src_addr().is_unspecified()
                || state.interfaces.has_ipv4_address(header.src_addr()))
            {
                warn!("Dropping IPv4 broadcast packet with source {}", src_addr);
                return false;
            }

            let bound_if = packet.ifindex();
            let device_count = state.interfaces.devices().len();
            let mut poll_next = false;
            for dev_index in 0..device_count.saturating_sub(1) {
                let ifindex = dev_index as i32 + 1;
                if bound_if != 0 && bound_if != ifindex {
                    continue;
                }
                poll_next |= io.submit_ip_packet(
                    &mut state.interfaces,
                    dev_index,
                    dst_addr,
                    src_addr,
                    packet.clone(),
                    timestamp,
                );
            }
            if device_count > 0 && (bound_if == 0 || bound_if == device_count as i32) {
                poll_next |= io.submit_ip_packet(
                    &mut state.interfaces,
                    device_count - 1,
                    dst_addr,
                    src_addr,
                    packet,
                    timestamp,
                );
            }
            return poll_next;
        }

        let Some(rule) = state.routing.lookup(&dst_addr).copied() else {
            warn!("No route found for destination: {}", dst_addr);
            return false;
        };
        let bound_if = packet.ifindex();
        if bound_if != 0 && bound_if != rule.dev as i32 + 1 {
            warn!(
                "Dropping IPv4 packet bound to ifindex {bound_if} via device {}",
                rule.dev as i32 + 1
            );
            return false;
        }
        if !state.interfaces.has_ipv4_address(header.src_addr()) {
            warn!(
                "Dropping IPv4 packet with non-local source {} routed via {}",
                src_addr, rule.src
            );
            return false;
        }

        let next_hop = rule.via.unwrap_or(dst_addr);
        io.submit_ip_packet(
            &mut state.interfaces,
            rule.dev,
            next_hop,
            src_addr,
            packet,
            timestamp,
        )
    }

    fn fragment(&mut self, state: &NetworkState, packet: Vec<u8>) -> KResult<Vec<Vec<u8>>> {
        let header = Ipv4Header::parse_output(&packet).map_err(|_| KError::InvalidInput)?;
        let dst_addr = IpAddress::Ipv4(header.dst_addr());
        let Some(mtu) = state.routing.route_mtu(&state.interfaces, &dst_addr) else {
            return Ok(vec![packet]);
        };
        if packet.len() <= mtu {
            return Ok(vec![packet]);
        }

        let identification = self.next_identification;
        self.next_identification = self.next_identification.wrapping_add(1).max(1);
        ipv4::fragment_output_packet(&packet, mtu, identification).map_err(|err| match err {
            ipv4::Ipv4FragmentError::Malformed => KError::InvalidInput,
            ipv4::Ipv4FragmentError::DontFragment | ipv4::Ipv4FragmentError::MtuTooSmall => {
                LinuxError::EMSGSIZE.into()
            }
        })
    }
}
fn output_ipv4_packet_count(
    state: &NetworkState,
    dst_addr: &IpAddress,
    packet_len: usize,
) -> KResult<usize> {
    if packet_len > u16::MAX as usize {
        return Err(LinuxError::EMSGSIZE.into());
    }

    let Some(mtu) = state.route_mtu(dst_addr) else {
        return Ok(1);
    };
    if packet_len <= mtu {
        return Ok(1);
    }

    let payload_len = packet_len
        .checked_sub(ipv4::IPV4_MIN_HEADER_LEN)
        .ok_or_else(|| KError::from(LinuxError::EMSGSIZE))?;
    let max_fragment_payload_len = mtu
        .checked_sub(ipv4::IPV4_MIN_HEADER_LEN)
        .map(|len| len / 8 * 8)
        .filter(|len| *len > 0)
        .ok_or_else(|| KError::from(LinuxError::EMSGSIZE))?;
    Ok(payload_len
        .checked_add(max_fragment_payload_len - 1)
        .ok_or_else(|| KError::from(LinuxError::EMSGSIZE))?
        / max_fragment_payload_len)
}

#[cfg(unittest)]
mod tests {
    use alloc::boxed::Box;

    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::{
        device::{interfaces::Ipv4AddrEntry, test_device::TestDevice},
        ip::{Ipv4Address, Ipv4Cidr},
        router::ROUTE_SCOPE_UNIVERSE,
    };

    #[def_test]
    fn fragmented_send_retains_packet_until_all_slots_are_available() {
        let mut state = NetworkState::new();
        let dev = state
            .interfaces
            .add_device(Box::new(TestDevice::ethernet(1, 68)));
        let src = Ipv4Address::new(192, 0, 2, 1);
        let dst = Ipv4Address::new(192, 0, 2, 2);
        state
            .add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: Ipv4Cidr::new(src, 24),
                scope: ROUTE_SCOPE_UNIVERSE,
                broadcast: None,
            })
            .unwrap();
        let mut io = NetIo::new();
        let mut output = Ipv4Output::new();
        let capacity = io.available_data_tx_slots();
        io.enqueue_tx(
            (0..capacity - 1)
                .map(|_| PacketBuf::from_ip_packet_vec(0, vec![0; 20], PacketOwner::Ipv4Stack)),
        );
        let original =
            ipv4::build_ipv4_packet(src, dst, ipv4::PROTOCOL_ICMP, 64, &[0; 80]).unwrap();
        let mut packet = Some(original.clone());
        let mut prepared = false;
        let result = output.prepare_and_send(
            &mut state,
            &mut io,
            Ipv4SendOptions {
                bound_src: None,
                dst_addr: IpAddress::Ipv4(dst),
                oif: 0,
                allow_broadcast: false,
            },
            &mut packet,
            |_, _, _| {
                prepared = true;
                Ok(())
            },
        );
        assert_eq!(result, Err(KError::WouldBlock));
        assert!(!prepared);
        assert_eq!(packet.as_ref(), Some(&original));
        assert_eq!(io.available_data_tx_slots(), 1);
        io.dequeue_tx().unwrap();
        let mut selected = None;
        output
            .prepare_and_send(
                &mut state,
                &mut io,
                Ipv4SendOptions {
                    bound_src: None,
                    dst_addr: IpAddress::Ipv4(dst),
                    oif: 0,
                    allow_broadcast: false,
                },
                &mut packet,
                |_, source, mtu| {
                    selected = Some((source, mtu));
                    prepared = true;
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(selected, Some((IpAddress::Ipv4(src), Some(68))));
        assert!(prepared);
        assert!(packet.is_none());
        assert_eq!(io.available_data_tx_slots(), 0);
    }
}
