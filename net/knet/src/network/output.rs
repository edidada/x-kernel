// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 send preparation, submission, and queued device dispatch.
//!
//! Preparation borrows routing, devices, and queue capacity for the lifetime of
//! one send. The transport fills only its own bytes using the selected addresses;
//! submission consumes that transaction and completes IPv4 encapsulation and
//! fragmentation. Queued packets resolve routing again at dispatch.

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

/// IPv4 fragmentation behavior selected by the socket.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Ipv4FragmentationPolicy {
    Allow,
    Forbid,
    /// Set DF only when the datagram fits the MTU, or no MTU is available.
    PreferDontFragment,
}

impl Ipv4FragmentationPolicy {
    fn should_set_dont_fragment(self, packet_len: usize, mtu: Option<usize>) -> bool {
        match self {
            Self::Allow => false,
            Self::Forbid => true,
            Self::PreferDontFragment => mtu.is_none_or(|mtu| packet_len <= mtu),
        }
    }
}

/// Socket-selected constraints and IPv4 header fields for one send attempt.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ipv4SendOptions {
    /// Explicit local source; absence requests automatic source selection.
    pub(crate) bound_src: Option<Ipv4Address>,
    pub(crate) dst_addr: Ipv4Address,
    /// Stable interface index; zero leaves device selection to routing.
    pub(crate) oif: i32,
    pub(crate) allow_broadcast: bool,
    pub(crate) protocol: u8,
    pub(crate) ttl: u8,
    pub(crate) fragmentation: Ipv4FragmentationPolicy,
}

/// Owned datagram storage with an IPv4 header reserved for output submission.
///
/// Transport code can fill its own header and payload, but cannot change the
/// packet length or write IPv4 fields. Preparation failure returns this builder
/// unchanged, so a retry neither copies nor rereads the user's payload.
#[derive(Debug)]
pub(crate) struct Ipv4PacketBuilder {
    packet: Vec<u8>,
}

impl Ipv4PacketBuilder {
    /// Allocates space for an IPv4 datagram with `transport_len` bytes after IP.
    ///
    /// Returns `EMSGSIZE` when the complete datagram cannot fit an IPv4 length.
    pub(crate) fn new(transport_len: usize) -> KResult<Self> {
        let packet_len = transport_len
            .checked_add(IPV4_HEADER_LEN)
            .filter(|len| *len <= usize::from(u16::MAX))
            .ok_or(LinuxError::EMSGSIZE)?;
        Ok(Self {
            packet: vec![0; packet_len],
        })
    }

    /// Borrows the fixed-length transport header and payload region.
    pub(crate) fn transport_mut(&mut self) -> &mut [u8] {
        &mut self.packet[IPV4_HEADER_LEN..]
    }
}

/// A send whose configuration and queue capacity stay exclusively borrowed.
///
/// The transport uses the selected addresses for its checksum, then consumes
/// this value with `submit`. Dropping it only releases the packet and borrows;
/// it does not occupy queue slots or advance the identification sequence.
#[must_use = "fill the transport header and submit the prepared IPv4 send"]
pub(crate) struct PreparedIpv4Send<'a> {
    output: &'a mut Ipv4Output,
    _routes: &'a RouteTable,
    devices: &'a mut DeviceRegistry,
    tx_queue: &'a mut DataTxQueue,
    options: Ipv4SendOptions,
    packet: Ipv4PacketBuilder,
    decision: Ipv4SendDecision,
}

impl PreparedIpv4Send<'_> {
    /// Returns the final source address for the transport pseudo-header.
    pub(crate) fn source_addr(&self) -> Ipv4Address {
        self.decision.source
    }

    /// Returns the destination address for the transport pseudo-header.
    pub(crate) fn destination_addr(&self) -> Ipv4Address {
        self.options.dst_addr
    }

    /// Borrows transport bytes while keeping the selected addresses immutable.
    pub(crate) fn transport_mut(&mut self) -> &mut [u8] {
        self.packet.transport_mut()
    }

    /// Completes IPv4 encapsulation and consumes the datagram for submission.
    ///
    /// Capacity remains available under this transaction's exclusive borrow.
    /// Returns `EMSGSIZE` for a DF datagram exceeding the MTU or an MTU too small
    /// to fragment. Successful loopback submission can still drop on RX overflow.
    pub(crate) fn submit(mut self) -> KResult {
        let dont_fragment = self
            .options
            .fragmentation
            .should_set_dont_fragment(self.packet.packet.len(), self.decision.mtu);
        // RFC 6864 §4.3 requires distinct IDs even when only a downstream
        // router might fragment the datagram. Atomic datagrams can use zero.
        let identification = if dont_fragment {
            0
        } else {
            self.output.allocate_identification()
        };
        ipv4::write_ipv4_packet_header(
            &mut self.packet.packet,
            self.decision.source,
            self.options.dst_addr,
            self.options.protocol,
            self.options.ttl,
            identification,
            dont_fragment,
        )
        .ok_or(LinuxError::EMSGSIZE)?;
        match self.decision.submission {
            SubmissionTarget::Queued => Ipv4Output::queue_packet(
                self.tx_queue,
                self.packet.packet,
                self.options.oif,
                self.decision.mtu,
            ),
            SubmissionTarget::Immediate(target) => Ipv4Output::transmit_now(
                self.devices,
                self.packet.packet,
                self.decision.mtu,
                target,
            ),
        }
    }
}

/// RX work produced by dispatch, independent of device transmit success.
#[derive(Default)]
pub(crate) struct Ipv4DispatchOutcome {
    pub(crate) has_rx_work: bool,
}

/// An IPv4 route decision, consumed under one configuration guard.
pub(crate) struct OutputRoute {
    pub(crate) source: Ipv4Address,
    pub(crate) dev: NetDeviceId,
    pub(crate) next_hop: Ipv4Address,
    pub(crate) mtu: usize,
    pub(crate) link_kind: LinkKind,
}

enum OutputTarget {
    Routed {
        dev: NetDeviceId,
        next_hop: Ipv4Address,
    },
    LimitedBroadcast,
}

enum SubmissionTarget {
    Queued,
    Immediate(OutputTarget),
}

/// Source and submission strategy resolved under one configuration guard.
struct Ipv4SendDecision {
    source: Ipv4Address,
    mtu: Option<usize>,
    submission: SubmissionTarget,
}

/// Owns IPv4 output rules and the identification sequence for non-atomic sends.
///
/// Resources are borrowed from the enclosing Router guard. Output never locks
/// Router itself and never retains a routing decision beyond its transaction.
pub(crate) struct Ipv4Output {
    next_identification: u16,
}

impl Ipv4Output {
    pub(crate) fn new() -> Self {
        Self {
            next_identification: 1,
        }
    }

    fn allocate_identification(&mut self) -> u16 {
        let identification = self.next_identification;
        self.next_identification = self.next_identification.wrapping_add(1).max(1);
        identification
    }

    /// Selects addresses and checks capacity without modifying the datagram.
    ///
    /// On error, returns the original builder. Returns `EACCES` for a forbidden
    /// broadcast, `ENETUNREACH` for an invalid route, binding, or source,
    /// `WouldBlock` for insufficient queue capacity, and `EMSGSIZE` for an
    /// unusable fragment MTU.
    /// The returned transaction exclusively borrows all resources through
    /// submission, so neither its route nor its capacity can become stale.
    pub(crate) fn prepare<'a>(
        &'a mut self,
        routes: &'a RouteTable,
        devices: &'a mut DeviceRegistry,
        tx_queue: &'a mut DataTxQueue,
        options: Ipv4SendOptions,
        packet: Ipv4PacketBuilder,
    ) -> Result<PreparedIpv4Send<'a>, (KError, Ipv4PacketBuilder)> {
        let decision =
            match Self::prepare_output(routes, devices, tx_queue, options, packet.packet.len()) {
                Ok(decision) => decision,
                Err(error) => return Err((error, packet)),
            };
        Ok(PreparedIpv4Send {
            output: self,
            _routes: routes,
            devices,
            tx_queue,
            options,
            packet,
            decision,
        })
    }

    /// Dispatches an already queued packet using the current routing state.
    ///
    /// Normalizes the byte-buffer boundary shared with smoltcp before checking
    /// the packet's source ownership and interface binding.
    pub(crate) fn dispatch_queued(
        routes: &RouteTable,
        devices: &mut DeviceRegistry,
        mut packet: PacketBuf,
        timestamp: MonotonicInstant,
    ) -> Ipv4DispatchOutcome {
        let Some((source, destination)) = Self::normalize_packet(&mut packet) else {
            return Ipv4DispatchOutcome::default();
        };
        let target = if destination.is_broadcast() {
            OutputTarget::LimitedBroadcast
        } else {
            let dst_addr = IpAddress::Ipv4(destination);
            let Some(rule) = routes.lookup(&dst_addr) else {
                warn!("No route found for destination: {}", dst_addr);
                return Ipv4DispatchOutcome::default();
            };
            let IpAddress::Ipv4(next_hop) = rule.via.unwrap_or(dst_addr) else {
                return Ipv4DispatchOutcome::default();
            };
            OutputTarget::Routed {
                dev: rule.dev,
                next_hop,
            }
        };
        Self::dispatch_to(devices, packet, source, destination, timestamp, &target)
    }

    fn prepare_output(
        routes: &RouteTable,
        devices: &DeviceRegistry,
        tx_queue: &DataTxQueue,
        options: Ipv4SendOptions,
        packet_len: usize,
    ) -> KResult<Ipv4SendDecision> {
        let dst_addr = IpAddress::Ipv4(options.dst_addr);
        if requires_broadcast(devices, &dst_addr) && !options.allow_broadcast {
            return Err(LinuxError::EACCES.into());
        }
        let (source, mtu, link_kind, target) = if options.dst_addr.is_broadcast() {
            // Bound broadcasts work without a route. Preserve the optional MTU
            // and loopback policy supplied by a route when one exists.
            let device = routes
                .lookup(&dst_addr)
                .and_then(|rule| devices.get_by_id(rule.dev));
            (
                output_source_address(routes, devices, &dst_addr, options.oif, options.bound_src)?,
                device.map(|device| device.mtu().min(usize::from(u16::MAX))),
                device.map(|device| device.link_kind()),
                OutputTarget::LimitedBroadcast,
            )
        } else {
            let route =
                resolve_output_route(routes, devices, &dst_addr, options.oif, options.bound_src)?;
            (
                route.source,
                Some(route.mtu),
                Some(route.link_kind),
                OutputTarget::Routed {
                    dev: route.dev,
                    next_hop: route.next_hop,
                },
            )
        };
        let submission = if link_kind == Some(LinkKind::Loopback) {
            SubmissionTarget::Immediate(target)
        } else {
            if tx_queue.is_full() || !tx_queue.can_push(output_packet_count(packet_len, mtu)?) {
                return Err(KError::WouldBlock);
            }
            SubmissionTarget::Queued
        };
        Ok(Ipv4SendDecision {
            source,
            mtu,
            submission,
        })
    }

    /// Fragments before atomically enqueuing the complete group of packets.
    pub(crate) fn queue_packet(
        tx_queue: &mut DataTxQueue,
        packet: Vec<u8>,
        oif: i32,
        mtu: Option<usize>,
    ) -> KResult {
        let packets = Self::fragment(packet, mtu)?;
        tx_queue
            .try_extend(
                packets.into_iter().map(|packet| {
                    PacketBuf::from_ip_packet_vec(oif, packet, PacketOwner::Ipv4Stack)
                }),
            )
            .map_err(|_| KError::WouldBlock)
    }

    fn fragment(packet: Vec<u8>, mtu: Option<usize>) -> KResult<Vec<Vec<u8>>> {
        if packet.len() > usize::from(u16::MAX) {
            return Err(KError::InvalidInput);
        }
        let header = Ipv4Header::parse_header_only(&packet).map_err(|_| KError::InvalidInput)?;
        let Some(mtu) = mtu else {
            return Ok(vec![packet]);
        };
        if packet.len() <= mtu {
            return Ok(vec![packet]);
        }
        ipv4::fragment_output_packet(&packet, mtu, header.identification()).map_err(|err| match err
        {
            ipv4::Ipv4FragmentError::Malformed => KError::InvalidInput,
            ipv4::Ipv4FragmentError::DontFragment | ipv4::Ipv4FragmentError::MtuTooSmall => {
                LinuxError::EMSGSIZE.into()
            }
        })
    }

    fn transmit_now(
        devices: &mut DeviceRegistry,
        packet: Vec<u8>,
        mtu: Option<usize>,
        target: OutputTarget,
    ) -> KResult {
        let timestamp = monotonic_time();
        for packet in Self::fragment(packet, mtu)? {
            // Immediate loopback submission historically carries no queue binding.
            let mut packet = PacketBuf::from_ip_packet_vec(0, packet, PacketOwner::Ipv4Stack);
            let Some((source, destination)) = Self::normalize_packet(&mut packet) else {
                continue;
            };
            let _outcome =
                Self::dispatch_to(devices, packet, source, destination, timestamp, &target);
        }
        Ok(())
    }

    fn normalize_packet(packet: &mut PacketBuf) -> Option<(Ipv4Address, Ipv4Address)> {
        ipv4::prepare_output_packet(packet).ok()?;
        let header = Ipv4Header::parse_header_only(packet.network_packet()?).ok()?;
        Some((header.src_addr(), header.dst_addr()))
    }

    fn dispatch_to(
        devices: &mut DeviceRegistry,
        packet: PacketBuf,
        source: Ipv4Address,
        destination: Ipv4Address,
        timestamp: MonotonicInstant,
        target: &OutputTarget,
    ) -> Ipv4DispatchOutcome {
        let src_addr = IpAddress::Ipv4(source);
        let dst_addr = IpAddress::Ipv4(destination);
        let (dev_id, next_hop) = match *target {
            OutputTarget::LimitedBroadcast => {
                if !(source.is_unspecified() || has_local_source(devices, source)) {
                    warn!("Dropping IPv4 broadcast packet with source {}", src_addr);
                    return Ipv4DispatchOutcome::default();
                }
                return Self::dispatch_broadcast(devices, packet, src_addr, dst_addr, timestamp);
            }
            OutputTarget::Routed { dev, next_hop } => (dev, next_hop),
        };
        let bound_if = packet.ifindex();
        if bound_if != 0 && bound_if != dev_id.ifindex() {
            warn!(
                "Dropping IPv4 packet bound to ifindex {bound_if} via device {}",
                dev_id.ifindex()
            );
            return Ipv4DispatchOutcome::default();
        }
        if !has_local_source(devices, source) {
            warn!("Dropping IPv4 packet with non-local source {}", src_addr);
            return Ipv4DispatchOutcome::default();
        }
        let Some(dev) = devices.get_mut_by_id(dev_id) else {
            return Ipv4DispatchOutcome::default();
        };
        Ipv4DispatchOutcome {
            has_rx_work: dev.send_ip_packet(
                dev_id.ifindex(),
                next_hop.into(),
                src_addr,
                packet,
                timestamp,
            ),
        }
    }

    fn dispatch_broadcast(
        devices: &mut DeviceRegistry,
        packet: PacketBuf,
        src_addr: IpAddress,
        dst_addr: IpAddress,
        timestamp: MonotonicInstant,
    ) -> Ipv4DispatchOutcome {
        let bound_if = packet.ifindex();
        let mut devices = devices.iter_mut_with_ids();
        let Some((last_id, last_device)) = devices.next_back() else {
            return Ipv4DispatchOutcome::default();
        };
        let mut has_rx_work = false;
        for (id, dev) in devices {
            let ifindex = id.ifindex();
            if bound_if != 0 && bound_if != ifindex {
                continue;
            }
            has_rx_work |=
                dev.send_ip_packet(ifindex, dst_addr, src_addr, packet.clone(), timestamp);
        }
        let last_ifindex = last_id.ifindex();
        if bound_if == 0 || bound_if == last_ifindex {
            has_rx_work |=
                last_device.send_ip_packet(last_ifindex, dst_addr, src_addr, packet, timestamp);
        }
        Ipv4DispatchOutcome { has_rx_work }
    }
}

/// Resolves the route and its device without touching packet queues.
///
/// `oif` is a stable interface index; zero means unbound. Returns `ENETUNREACH`
/// for a missing route, absent or down device, mismatched binding, or unavailable
/// source address. The caller must serialize configuration access and use of
/// the result in the same critical section.
/// An explicit `bound_src` must be local, may belong to another device, and
/// takes precedence over preferred or automatically selected source addresses.
/// Returns `EAFNOSUPPORT` if a resolved route has non-IPv4 addresses.
pub(crate) fn resolve_output_route(
    routes: &RouteTable,
    devices: &DeviceRegistry,
    dst: &IpAddress,
    oif: i32,
    bound_src: Option<Ipv4Address>,
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
    if bound_src.is_some_and(|source| !has_local_source(devices, source)) {
        return Err(LinuxError::ENETUNREACH.into());
    }
    let next_hop = rule.via.unwrap_or(*dst);
    // Linux 6.8 ip_route_output_key_hash_rcu validates an explicit source
    // globally; fib_select_path selects a default only when no source was given.
    let source = match bound_src
        .map(IpAddress::Ipv4)
        .or_else(|| rule.preferred_source())
    {
        Some(source) => source,
        None => devices
            .ipv4(rule.dev)
            .and_then(|addrs| addrs.select_source(&rule.via.unwrap_or(crate::ip::Ipv4Address::UNSPECIFIED.into()), rule.scope))
            .map(IpAddress::Ipv4)
            .ok_or(LinuxError::ENETUNREACH)?,
    };
    let (IpAddress::Ipv4(source), IpAddress::Ipv4(next_hop)) = (source, next_hop) else {
        return Err(LinuxError::EAFNOSUPPORT.into());
    };
    Ok(OutputRoute {
        source,
        dev: rule.dev,
        next_hop,
        mtu: device.mtu().min(u16::MAX as usize),
        link_kind: device.link_kind(),
    })
}

/// Selects a local output source, including bound route-less broadcasts.
///
/// An explicit source is validated without requiring a default source. With no
/// explicit source, bound limited broadcasts may use `0.0.0.0` on an
/// unaddressed device.
pub(crate) fn output_source_address(
    routes: &RouteTable,
    devices: &DeviceRegistry,
    dst: &IpAddress,
    oif: i32,
    bound_src: Option<Ipv4Address>,
) -> KResult<Ipv4Address> {
    if dst.is_broadcast() && oif > 0 {
        let device_id = NetDeviceId::new(oif).ok_or(LinuxError::ENETUNREACH)?;
        let device = devices
            .get_by_id(device_id)
            .ok_or(LinuxError::ENETUNREACH)?;
        if !device.is_link_up() {
            return Err(LinuxError::ENETUNREACH.into());
        }
        if let Some(source) = bound_src {
            if !has_local_source(devices, source) {
                return Err(LinuxError::ENETUNREACH.into());
            }
            return Ok(source);
        }
        return Ok(devices
            .ipv4(device_id)
            .and_then(|addrs| addrs.as_slice().first())
            .map(|entry| entry.addr.address())
            .unwrap_or(Ipv4Address::UNSPECIFIED));
    }
    Ok(resolve_output_route(routes, devices, dst, oif, bound_src)?.source)
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
