// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 output preparation, fragmentation, and device dispatch.
//!
//! `Ipv4Output` owns the fragmentation identification sequence and the complete
//! send transaction. Routing, devices, and queue capacity are borrowed from one
//! exclusive owner for preparation and submission. Output decisions must be
//! consumed within that transaction. Queued packets resolve routing again when
//! dispatched.

use alloc::{vec, vec::Vec};

use kerrno::{KError, KResult, LinuxError};
use khal::time::monotonic_time;
use ktime_types::MonotonicInstant;

use crate::{
    buf::{PacketBuf, PacketOwner},
    device::{DeviceRegistry, LinkKind, NetDeviceId},
    ip::{IpAddress, Ipv4Address},
    network::{
        ipv4::{self, Ipv4Header},
        router::RouteTable,
    },
    runtime::tx_queue::DataTxQueue,
};

const IPV4_HEADER_LEN: usize = 20;

/// One stack-generated datagram and its socket-selected output constraints.
pub(crate) struct Ipv4SendRequest<'a> {
    pub(crate) bound_src: Option<IpAddress>,
    pub(crate) dst_addr: IpAddress,
    /// Stable interface index; zero leaves device selection to routing.
    pub(crate) oif: i32,
    pub(crate) allow_broadcast: bool,
    pub(crate) packet: &'a mut Option<Vec<u8>>,
}

/// A single-route output decision, valid within the current output transaction.
///
/// Queued packets retain their socket's interface constraint and resolve again
/// at dispatch. Limited-broadcast fan-out is handled separately.
pub(crate) struct OutputRoute {
    /// Default source; an explicitly bound socket source takes precedence.
    pub(crate) source: IpAddress,
    pub(crate) dev: NetDeviceId,
    pub(crate) next_hop: IpAddress,
    pub(crate) mtu: usize,
    pub(crate) link_kind: LinkKind,
}

/// Owns IPv4 output policy and the identification sequence for fragmentation.
///
/// The enclosing Router mutex serializes the sequence together with route,
/// device, and queue access. The output path never acquires that mutex itself.
pub(crate) struct Ipv4Output {
    next_identification: u16,
}

impl Ipv4Output {
    pub(crate) fn new() -> Self {
        Self {
            next_identification: 1,
        }
    }

    /// Prepares and submits a datagram using one consistent configuration view.
    ///
    /// The caller must retain exclusive configuration and queue access throughout
    /// this call. `prepare_fn` must build a packet with a 20-byte IPv4 header for
    /// the requested destination without changing its length. Capacity rejection
    /// leaves the original buffer untouched and does not call `prepare_fn`;
    /// a preparation error also retains the buffer. After successful preparation,
    /// fragmentation consumes it.
    /// Loopback bypasses the data queue and completes device submission here.
    ///
    /// # Errors
    /// Returns `EACCES` for a forbidden broadcast, `ENETUNREACH` for an invalid
    /// output route or binding, `WouldBlock` for insufficient queue capacity,
    /// and `EMSGSIZE` when fragmentation cannot fit the MTU. Packet validation
    /// and `prepare_fn` errors propagate to the caller.
    pub(crate) fn prepare_and_send(
        &mut self,
        routes: &RouteTable,
        devices: &mut DeviceRegistry,
        tx_queue: &mut DataTxQueue,
        request: Ipv4SendRequest<'_>,
        prepare_fn: impl FnOnce(&mut [u8], IpAddress, Option<usize>) -> KResult,
    ) -> KResult {
        let Ipv4SendRequest {
            bound_src,
            dst_addr,
            oif,
            allow_broadcast,
            packet,
        } = request;
        if requires_broadcast(devices, &dst_addr) && !allow_broadcast {
            return Err(LinuxError::EACCES.into());
        }
        let packet_len = packet.as_ref().ok_or(KError::InvalidInput)?.len();
        // Limited broadcasts can bypass route lookup or fan out across devices.
        let route = if dst_addr.is_broadcast() {
            None
        } else {
            Some(resolve_output_route(routes, devices, &dst_addr, oif)?)
        };
        let (source, mtu, is_loopback) = if let Some(route) = &route {
            (
                route.source,
                Some(route.mtu),
                route.link_kind == LinkKind::Loopback,
            )
        } else {
            let device = routes
                .lookup(&dst_addr)
                .and_then(|rule| devices.get_by_id(rule.dev));
            (
                output_source_address(routes, devices, &dst_addr, oif)?,
                device.map(|device| device.mtu().min(u16::MAX as usize)),
                device.is_some_and(|device| device.link_kind() == LinkKind::Loopback),
            )
        };
        if !is_loopback
            && (tx_queue.is_full() || !tx_queue.can_push(output_packet_count(packet_len, mtu)?))
        {
            return Err(KError::WouldBlock);
        }
        prepare_fn(
            packet.as_deref_mut().ok_or(KError::InvalidInput)?,
            bound_src.unwrap_or(source),
            mtu,
        )?;
        let packet = packet.take().ok_or(KError::InvalidInput)?;
        if is_loopback {
            self.transmit_now(routes, devices, packet, mtu, route.as_ref())
        } else {
            self.queue_packet(tx_queue, packet, oif, mtu)
        }
    }

    /// Fragments and atomically enqueues a complete datagram into the data queue.
    ///
    /// Consumes the buffer even on error. Callers requiring capacity retries use
    /// `prepare_and_send`, which checks capacity before taking the buffer.
    pub(crate) fn queue_packet(
        &mut self,
        tx_queue: &mut DataTxQueue,
        packet: Vec<u8>,
        oif: i32,
        mtu: Option<usize>,
    ) -> KResult {
        let packets = self.fragment(packet, mtu)?;
        tx_queue
            .try_extend(
                packets.into_iter().map(|packet| {
                    PacketBuf::from_ip_packet_vec(oif, packet, PacketOwner::Ipv4Stack)
                }),
            )
            .map_err(|_| KError::WouldBlock)
    }

    /// Dispatches queued output through current routing and source ownership.
    ///
    /// Returns an RX-readiness hint, including loopback delivery; it does not
    /// report transmit success. Limited broadcasts obey the packet's binding.
    pub(crate) fn dispatch_packet(
        routes: &RouteTable,
        devices: &mut DeviceRegistry,
        packet: PacketBuf,
        timestamp: MonotonicInstant,
    ) -> bool {
        Self::dispatch_resolved_packet(routes, devices, packet, timestamp, None)
    }

    /// The sequence advances for oversized packets, including DF rejection.
    fn fragment(&mut self, packet: Vec<u8>, mtu: Option<usize>) -> KResult<Vec<Vec<u8>>> {
        if packet.len() > usize::from(u16::MAX) {
            return Err(KError::InvalidInput);
        }
        ipv4::Ipv4Header::parse_header_only(&packet).map_err(|_| KError::InvalidInput)?;
        let Some(mtu) = mtu else {
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

    /// Loopback RX overflow remains a successful submission with a dropped packet.
    fn transmit_now(
        &mut self,
        routes: &RouteTable,
        devices: &mut DeviceRegistry,
        packet: Vec<u8>,
        mtu: Option<usize>,
        route: Option<&OutputRoute>,
    ) -> KResult {
        let timestamp = monotonic_time();
        for packet in self.fragment(packet, mtu)? {
            let packet = PacketBuf::from_ip_packet_vec(0, packet, PacketOwner::Ipv4Stack);
            let _poll_next =
                Self::dispatch_resolved_packet(routes, devices, packet, timestamp, route);
        }
        Ok(())
    }

    fn dispatch_resolved_packet(
        routes: &RouteTable,
        devices: &mut DeviceRegistry,
        mut packet: PacketBuf,
        timestamp: MonotonicInstant,
        route: Option<&OutputRoute>,
    ) -> bool {
        if crate::ipv4::prepare_output_packet(&mut packet).is_err() {
            return false;
        }
        let header =
            Ipv4Header::parse_header_only(packet.network_packet().expect("finalized IPv4 packet"))
                .expect("finalized IPv4 header");

        let ipv4_src_addr = header.src_addr();
        let src_addr = IpAddress::Ipv4(ipv4_src_addr);
        let dst_addr = IpAddress::Ipv4(header.dst_addr());
        if header.dst_addr().is_broadcast() {
            if !(header.src_addr().is_unspecified() || has_local_source(devices, header.src_addr()))
            {
                warn!("Dropping IPv4 broadcast packet with source {}", src_addr);
                return false;
            }

            return Self::dispatch_broadcast(devices, packet, src_addr, dst_addr, timestamp);
        }

        // Queued output resolves again; synchronous output can reuse a decision
        // made under the same Router guard.
        let (dev_id, next_hop) = if let Some(route) = route {
            (route.dev, route.next_hop)
        } else {
            let Some(rule) = routes.lookup(&dst_addr) else {
                warn!("No route found for destination: {}", dst_addr);
                return false;
            };
            (rule.dev, rule.via.unwrap_or(dst_addr))
        };
        let bound_if = packet.ifindex();
        if bound_if != 0 && bound_if != dev_id.ifindex() {
            warn!(
                "Dropping IPv4 packet bound to ifindex {bound_if} via device {}",
                dev_id.ifindex()
            );
            return false;
        }
        if !has_local_source(devices, ipv4_src_addr) {
            warn!("Dropping IPv4 packet with non-local source {}", src_addr);
            return false;
        }

        let dev = devices
            .get_mut_by_id(dev_id)
            .expect("route device must be registered");
        dev.send_ip_packet(dev_id.ifindex(), next_hop, src_addr, packet, timestamp)
    }

    fn dispatch_broadcast(
        devices: &mut DeviceRegistry,
        packet: PacketBuf,
        src_addr: IpAddress,
        dst_addr: IpAddress,
        timestamp: MonotonicInstant,
    ) -> bool {
        let bound_if = packet.ifindex();
        let mut devices = devices.iter_mut_with_ids();
        let Some((last_id, last_device)) = devices.next_back() else {
            return false;
        };

        let mut poll_next = false;
        for (id, dev) in devices {
            let ifindex = id.ifindex();
            if bound_if != 0 && bound_if != ifindex {
                continue;
            }
            poll_next |= dev.send_ip_packet(ifindex, dst_addr, src_addr, packet.clone(), timestamp);
        }
        let last_ifindex = last_id.ifindex();
        if bound_if == 0 || bound_if == last_ifindex {
            poll_next |=
                last_device.send_ip_packet(last_ifindex, dst_addr, src_addr, packet, timestamp);
        }
        poll_next
    }
}

/// Resolves the route and its device without touching packet queues.
///
/// `oif` is a stable interface index; zero means unbound. Returns `ENETUNREACH`
/// for a missing route, absent or down device, mismatched binding, or unavailable
/// source address. The caller must serialize configuration access and use of
/// the result in the same critical section.
pub(crate) fn resolve_output_route(
    routes: &RouteTable,
    devices: &DeviceRegistry,
    dst: &IpAddress,
    oif: i32,
) -> KResult<OutputRoute> {
    // FIB ownership can differ from the actual output device for local routes.
    let oif = NetDeviceId::new(oif).filter(|id| id.ifindex() > 0);
    let rule = routes.lookup_for_output(dst, oif).ok_or(LinuxError::ENETUNREACH)?;
    if oif.is_some() {
        let fib_device = devices.get_by_id(rule.lookup_device()).ok_or(LinuxError::ENETUNREACH)?;
        if !fib_device.is_link_up() {
            return Err(LinuxError::ENETUNREACH.into());
        }
    }
    let device = devices.get_by_id(rule.dev).ok_or(LinuxError::ENETUNREACH)?;
    if !device.is_link_up() {
        return Err(LinuxError::ENETUNREACH.into());
    }
    let next_hop = rule.via.unwrap_or(*dst);
    let source = match rule.preferred_source() {
        Some(source) => source,
        None => devices
            .ipv4(rule.dev)
            .and_then(|addrs| addrs.select_source(&rule.via.unwrap_or(crate::ip::Ipv4Address::UNSPECIFIED.into()), rule.scope))
            .map(IpAddress::Ipv4)
            .ok_or(LinuxError::ENETUNREACH)?,
    };
    Ok(OutputRoute {
        source,
        dev: rule.dev,
        next_hop,
        mtu: device.mtu().min(u16::MAX as usize),
        link_kind: device.link_kind(),
    })
}

/// Selects the default output source, including bound route-less broadcasts.
pub(crate) fn output_source_address(
    routes: &RouteTable,
    devices: &DeviceRegistry,
    dst: &IpAddress,
    oif: i32,
) -> KResult<IpAddress> {
    if dst.is_broadcast() && oif > 0 {
        let device_id = NetDeviceId::new(oif).ok_or(LinuxError::ENETUNREACH)?;
        let device = devices
            .get_by_id(device_id)
            .ok_or(LinuxError::ENETUNREACH)?;
        if !device.is_link_up() {
            return Err(LinuxError::ENETUNREACH.into());
        }
        return Ok(devices
            .ipv4(device_id)
            .and_then(|addrs| addrs.as_slice().first())
            .map(|entry| entry.addr.address())
            .unwrap_or(Ipv4Address::UNSPECIFIED)
            .into());
    }
    Ok(resolve_output_route(routes, devices, dst, oif)?.source)
}

/// Returns whether a destination requires the socket's broadcast permission.
pub(crate) fn requires_broadcast(devices: &DeviceRegistry, address: &IpAddress) -> bool {
    let IpAddress::Ipv4(address) = *address else {
        return false;
    };
    address.is_broadcast()
        || devices
            .iter_ipv4_addrs()
            .any(|(_, entry)| entry.addr.is_directed_broadcast(address))
}

fn has_local_source(devices: &DeviceRegistry, address: Ipv4Address) -> bool {
    devices
        .iter_ipv4_addrs()
        .any(|(_, entry)| entry.addr.address() == address)
}

fn output_packet_count(packet_len: usize, mtu: Option<usize>) -> KResult<usize> {
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
        .checked_sub(IPV4_HEADER_LEN)
        .ok_or_else(|| KError::from(LinuxError::EMSGSIZE))?;
    let max_fragment_payload_len = mtu
        .checked_sub(IPV4_HEADER_LEN)
        .map(|len| len / 8 * 8)
        .filter(|len| *len > 0)
        .ok_or_else(|| KError::from(LinuxError::EMSGSIZE))?;
    Ok(payload_len
        .checked_add(max_fragment_payload_len - 1)
        .ok_or_else(|| KError::from(LinuxError::EMSGSIZE))?
        / max_fragment_payload_len)
}
