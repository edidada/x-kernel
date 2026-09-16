// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 output selection, transactional preparation, and fragmentation.
use alloc::{vec, vec::Vec};
use core::sync::atomic::{AtomicU16, Ordering};

use kerrno::{KError, KResult, LinuxError};
use khal::time::monotonic_time;
use ksync::Mutex;
use ktime_types::MonotonicInstant;

use crate::{
    buf::{PacketBuf, PacketOwner},
    io::NetIo,
    ip::IpAddress,
    network::{
        ipv4::{self, Ipv4Error, Ipv4Header},
        state::{NetworkState, ResolvedRoute, TxBatch},
    },
};

/// Socket-selected constraints validated against a copied output path.
pub(crate) struct Ipv4SendOptions {
    pub(crate) bound_src: Option<IpAddress>,
    pub(crate) dst_addr: IpAddress,
    pub(crate) oif: i32,
    pub(crate) allow_broadcast: bool,
}

pub(crate) struct Ipv4Output {
    next_identification: AtomicU16,
}

impl Ipv4Output {
    pub(crate) fn new() -> Self {
        Self {
            next_identification: AtomicU16::new(1),
        }
    }

    fn next_identification(&self) -> u16 {
        // Concurrent fragmented datagrams share this wrapping counter; all
        // fragments of one datagram use the same ID. It publishes no other state.
        loop {
            let id = self.next_identification.fetch_add(1, Ordering::Relaxed);
            if id != 0 {
                return id;
            }
        }
    }

    /// Resolves the path before taking `NetIo` for capacity preflight,
    /// packet preparation, fragmentation, and enqueue as one transaction.
    /// Loopback dispatch holds inventory read access through device handoff
    /// after the queue lock is released. Capacity failure preserves the caller's packet.
    pub fn prepare_and_send(
        &self,
        state: &NetworkState,
        io: &Mutex<NetIo>,
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
        if state.ipv4_dest_requires_broadcast(dst_addr) && !allow_broadcast {
            return Err(KError::from(LinuxError::EACCES));
        }
        let packet_len = packet.as_ref().ok_or(KError::InvalidInput)?.len();
        let route = state.resolve_output(dst_addr, oif)?;
        let source_addr = bound_src.unwrap_or(route.source);

        if route.is_loopback {
            prepare(
                packet.as_deref_mut().ok_or(KError::InvalidInput)?,
                source_addr,
                Some(route.mtu),
            )?;
            let packet = packet.take().ok_or(KError::InvalidInput)?;
            return self.send_loopback(state, route, oif, packet, monotonic_time());
        }

        let mut io = io.lock();
        let packet_count = output_ipv4_packet_count(Some(route.mtu), packet_len)?;
        if !io.can_admit_ipv4_egress(packet_count) {
            return Err(KError::WouldBlock);
        }
        prepare(
            packet.as_deref_mut().ok_or(KError::InvalidInput)?,
            source_addr,
            Some(route.mtu),
        )?;
        let packet = packet.take().ok_or(KError::InvalidInput)?;
        self.send(&mut io, Some(route.mtu), packet, oif)
    }

    pub fn send(
        &self,
        io: &mut NetIo,
        route_mtu: Option<usize>,
        packet: Vec<u8>,
        oif: i32,
    ) -> KResult {
        let packets = self.fragment(route_mtu, packet)?;
        if !io.can_admit_ipv4_egress(packets.len()) {
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
    /// Uses the route and `oif` copied during `prepare_and_send` so broadcast
    /// fan-out and unicast submission do not drop the socket binding or
    /// re-resolve after packet preparation. Loopback raises NetRx and drains it
    /// on BH enable; a full shared RX queue drops the packet while retaining
    /// the existing successful xmit result.
    fn send_loopback(
        &self,
        state: &NetworkState,
        route: ResolvedRoute,
        oif: i32,
        packet: Vec<u8>,
        timestamp: MonotonicInstant,
    ) -> KResult {
        let packets = self.fragment(Some(route.mtu), packet)?;
        for packet in packets {
            let mut buf = PacketBuf::from_ip_packet_vec(oif, packet, PacketOwner::Ipv4Stack);
            let header = match Ipv4Header::prepare_output_packet(&mut buf) {
                Ok(header) => header,
                Err(Ipv4Error::Malformed | Ipv4Error::BadChecksum) => continue,
            };
            let src_addr = IpAddress::Ipv4(header.src_addr());
            let dst_addr = IpAddress::Ipv4(header.dst_addr());
            // RX-ready hint for the poller, not send success. This path is
            // not the poller; a dropped loopback packet is still a successful
            // xmit, as documented above.
            let _poll_next = if header.dst_addr().is_broadcast() {
                dispatch_ipv4_broadcast(state, buf, src_addr, dst_addr, timestamp)
            } else {
                submit_resolved_unicast(state, route.clone(), src_addr, buf, timestamp)
            };
        }
        Ok(())
    }

    /// Dispatches a poller packet, returning ownership when a shared lock is busy.
    /// The batch retains inventory and routing read access through submission.
    pub(crate) fn dispatch(
        &self,
        batch: &mut TxBatch<'_>,
        mut packet: PacketBuf,
        timestamp: MonotonicInstant,
    ) -> Result<bool, PacketBuf> {
        let header = match Ipv4Header::prepare_output_packet(&mut packet) {
            Ok(header) => header,
            Err(Ipv4Error::Malformed | Ipv4Error::BadChecksum) => return Ok(false),
        };
        let src_addr = IpAddress::Ipv4(header.src_addr());
        let dst_addr = IpAddress::Ipv4(header.dst_addr());
        if header.dst_addr().is_broadcast() {
            return batch.broadcast(packet.ifindex(), src_addr, dst_addr, packet, timestamp);
        }
        let Some(route) = batch.resolve(&dst_addr) else {
            warn!("No route found for destination: {}", dst_addr);
            return Ok(false);
        };
        let bound_if = packet.ifindex();
        if bound_if != 0 && bound_if != route.dev as i32 + 1 {
            warn!(
                "Dropping IPv4 packet bound to ifindex {bound_if} via device {}",
                route.dev as i32 + 1
            );
            return Ok(false);
        }
        batch.submit_ip_packet(
            route.dev,
            route.via.unwrap_or(dst_addr),
            src_addr,
            packet,
            timestamp,
        )
    }

    fn fragment(&self, mtu: Option<usize>, packet: Vec<u8>) -> KResult<Vec<Vec<u8>>> {
        let Some(mtu) = mtu else {
            return Ok(vec![packet]);
        };
        if packet.len() <= mtu {
            return Ok(vec![packet]);
        }

        let identification = self.next_identification();
        ipv4::fragment_output_packet(&packet, mtu, identification).map_err(|err| match err {
            ipv4::Ipv4FragmentError::Malformed => KError::InvalidInput,
            ipv4::Ipv4FragmentError::DontFragment | ipv4::Ipv4FragmentError::MtuTooSmall => {
                LinuxError::EMSGSIZE.into()
            }
        })
    }
}

fn submit_resolved_unicast(
    state: &NetworkState,
    route: ResolvedRoute,
    source: IpAddress,
    packet: PacketBuf,
    timestamp: MonotonicInstant,
) -> bool {
    let bound_if = packet.ifindex();
    if bound_if != 0 && bound_if != route.device as i32 + 1 {
        warn!(
            "Dropping IPv4 packet bound to ifindex {bound_if} via device {}",
            route.device as i32 + 1
        );
        return false;
    }
    state.submit_ip_packet(route, source, packet, timestamp)
}

fn dispatch_ipv4_broadcast(
    state: &NetworkState,
    packet: PacketBuf,
    src_addr: IpAddress,
    dst_addr: IpAddress,
    timestamp: MonotonicInstant,
) -> bool {
    state.with_interfaces(|interfaces| {
        let IpAddress::Ipv4(source) = src_addr else {
            return false;
        };
        if !source.is_unspecified() && !interfaces.has_ipv4_address(source) {
            return false;
        }
        let bound_if = packet.ifindex();
        let mut devices = interfaces
            .devices()
            .iter()
            .enumerate()
            .filter(|(index, _)| bound_if == 0 || bound_if == *index as i32 + 1);
        let Some((last_index, last_device)) = devices.next_back() else {
            return false;
        };
        let mut has_rx_work = false;
        for (index, device) in devices {
            has_rx_work |= device.send_ip_packet(
                index as i32 + 1,
                dst_addr,
                src_addr,
                packet.clone(),
                timestamp,
            );
        }
        has_rx_work
            | last_device.send_ip_packet(
                last_index as i32 + 1,
                dst_addr,
                src_addr,
                packet,
                timestamp,
            )
    })
}

fn output_ipv4_packet_count(mtu: Option<usize>, packet_len: usize) -> KResult<usize> {
    if packet_len > u16::MAX as usize {
        return Err(LinuxError::EMSGSIZE.into());
    }

    let Some(mtu) = mtu else {
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
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::{
        device::{LoopbackDevice, interfaces::Ipv4AddrEntry, test_device::TestDevice},
        ip::{IpAddress, Ipv4Address, Ipv4Cidr},
        router::{ROUTE_SCOPE_HOST, ROUTE_SCOPE_UNIVERSE, Rule},
    };

    #[def_test]
    fn broadcast_returns_packet_before_contended_fanout() {
        let state = NetworkState::new();
        let sends = alloc::sync::Arc::new(AtomicUsize::new(0));
        for id in 1..=2 {
            state.add_device(Box::new(TestDevice::ethernet(id, 1500).with_send_hook({
                let sends = sends.clone();
                move || {
                    sends.fetch_add(1, Ordering::Relaxed);
                }
            })));
        }
        let output = Ipv4Output::new();
        let original = PacketBuf::from_ip_packet_vec(
            0,
            ipv4::build_ipv4_packet(
                Ipv4Address::UNSPECIFIED,
                Ipv4Address::BROADCAST,
                ipv4::PROTOCOL_ICMP,
                64,
                &[],
            )
            .unwrap(),
            PacketOwner::Ipv4Stack,
        );
        let handle = state.with_interfaces(|interfaces| interfaces.devices()[1].clone());
        let _device = handle.lock();
        let returned = state
            .try_with_tx_batch(|batch| {
                output.dispatch(batch, original.clone(), MonotonicInstant::ORIGIN)
            })
            .unwrap()
            .unwrap_err();
        assert_eq!(returned.network_packet(), original.network_packet());
        assert_eq!(returned.ifindex(), 0);
        assert_eq!(sends.load(Ordering::Relaxed), 0);
    }

    #[def_test]
    fn fragmented_send_retains_packet_until_all_slots_are_available() {
        let state = NetworkState::new();
        let dev = state.add_device(Box::new(TestDevice::ethernet(1, 68)));
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
        let output = Ipv4Output::new();
        while io.can_admit_ipv4_egress(2) {
            io.enqueue_tx(core::iter::once(PacketBuf::from_ip_packet_vec(
                0,
                vec![0; 20],
                PacketOwner::Ipv4Stack,
            )));
        }
        let io = Mutex::new(io);
        let original =
            ipv4::build_ipv4_packet(src, dst, ipv4::PROTOCOL_ICMP, 64, &[0; 80]).unwrap();
        let mut packet = Some(original.clone());
        let mut prepared = false;
        let result = output.prepare_and_send(
            &state,
            &io,
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
        assert!(io.lock().can_admit_ipv4_egress(1));
        assert!(!io.lock().can_admit_ipv4_egress(2));
        io.lock().dequeue_tx().unwrap();
        let mut selected = None;
        output
            .prepare_and_send(
                &state,
                &io,
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
        assert!(!io.lock().can_admit_ipv4_egress(1));
    }

    #[def_test(serial)]
    fn loopback_limited_broadcast_respects_bound_interface() {
        static ETH_SENDS: AtomicUsize = AtomicUsize::new(0);

        ETH_SENDS.store(0, Ordering::Relaxed);

        let state = NetworkState::new();
        let lo = state.add_device(Box::new(LoopbackDevice::new()));
        let lo_ifindex = lo as i32 + 1;
        state
            .add_ipv4_addr(Ipv4AddrEntry {
                dev: lo,
                addr: Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8),
                scope: ROUTE_SCOPE_HOST,
                broadcast: None,
            })
            .unwrap();
        let eth = state.add_device(Box::new(TestDevice::ethernet(1, 1500).with_send_hook(
            || {
                ETH_SENDS.fetch_add(1, Ordering::Relaxed);
            },
        )));
        let eth_addr = Ipv4Address::new(192, 0, 2, 1);
        state
            .add_ipv4_addr(Ipv4AddrEntry {
                dev: eth,
                addr: Ipv4Cidr::new(eth_addr, 24),
                scope: ROUTE_SCOPE_UNIVERSE,
                broadcast: None,
            })
            .unwrap();
        state.add_rule(Rule::new(
            Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0).into(),
            None,
            eth,
            IpAddress::Ipv4(eth_addr),
        ));

        let output = Ipv4Output::new();
        let io = Mutex::new(NetIo::new());
        let src = Ipv4Address::new(127, 0, 0, 1);
        let dst = Ipv4Address::BROADCAST;
        let mut packet =
            Some(ipv4::build_ipv4_packet(src, dst, ipv4::PROTOCOL_UDP, 64, &[]).unwrap());

        output
            .prepare_and_send(
                &state,
                &io,
                Ipv4SendOptions {
                    bound_src: None,
                    dst_addr: IpAddress::Ipv4(dst),
                    oif: lo_ifindex,
                    allow_broadcast: true,
                },
                &mut packet,
                |_, _, _| Ok(()),
            )
            .unwrap();

        assert_eq!(ETH_SENDS.load(Ordering::Relaxed), 0);
    }

    #[def_test(serial)]
    fn loopback_unicast_keeps_route_selected_during_prepare() {
        static ETH_SENDS: AtomicUsize = AtomicUsize::new(0);

        ETH_SENDS.store(0, Ordering::Relaxed);

        let state = NetworkState::new();
        let lo = state.add_device(Box::new(LoopbackDevice::new()));
        let eth = state.add_device(Box::new(TestDevice::ethernet(1, 1500).with_send_hook(
            || {
                ETH_SENDS.fetch_add(1, Ordering::Relaxed);
            },
        )));
        let lo_addr = Ipv4Address::new(127, 0, 0, 1);
        let lo_entry = Ipv4AddrEntry {
            dev: lo,
            addr: Ipv4Cidr::new(lo_addr, 8),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        };
        state.add_ipv4_addr(lo_entry).unwrap();
        let eth_addr = Ipv4Address::new(192, 0, 2, 1);
        state
            .add_ipv4_addr(Ipv4AddrEntry {
                dev: eth,
                addr: Ipv4Cidr::new(eth_addr, 24),
                scope: ROUTE_SCOPE_UNIVERSE,
                broadcast: None,
            })
            .unwrap();

        let output = Ipv4Output::new();
        let io = Mutex::new(NetIo::new());
        let mut packet =
            Some(ipv4::build_ipv4_packet(lo_addr, lo_addr, ipv4::PROTOCOL_ICMP, 64, &[]).unwrap());

        output
            .prepare_and_send(
                &state,
                &io,
                Ipv4SendOptions {
                    bound_src: None,
                    dst_addr: IpAddress::Ipv4(lo_addr),
                    oif: 0,
                    allow_broadcast: false,
                },
                &mut packet,
                |_, _, _| {
                    state.remove_ipv4_addr(lo_entry);
                    state.add_rule(Rule::new(
                        Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 0), 8).into(),
                        None,
                        eth,
                        IpAddress::Ipv4(eth_addr),
                    ));
                    Ok(())
                },
            )
            .unwrap();

        assert_eq!(ETH_SENDS.load(Ordering::Relaxed), 0);
    }
}
