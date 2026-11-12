// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 send transaction: preparation, transport hand-off, and submission.
//!
//! Preparation validates the send and decides everything that can fail for a
//! reason other than a broken invariant: permission, route, source, size, and
//! queue capacity. Submission only executes those decisions.

use alloc::{vec, vec::Vec};

use kerrno::{KError, KResult, LinuxError};

use super::{
    Ipv4Output,
    xmit::{self, OutputTarget},
};
use crate::{
    device::{DeviceRegistry, LinkKind, NetDeviceId},
    ip::{IpAddress, Ipv4Address},
    network::{
        ipv4,
        output_route::{output_source_address, resolve_output_route},
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
    /// Bound output interface; absence leaves device selection to routing.
    pub(crate) oif: Option<NetDeviceId>,
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

/// Resources one send borrows from the enclosing Router guard.
///
/// Grouping them gives the transaction a single hand-off point and keeps the
/// set that a later, shorter critical section must reserve in one place.
pub(crate) struct Ipv4OutputResources<'a> {
    pub(crate) routes: &'a RouteTable,
    pub(crate) devices: &'a mut DeviceRegistry,
    pub(crate) tx_queue: &'a mut DataTxQueue,
}

/// A send whose configuration and queue capacity stay exclusively borrowed.
///
/// The transport uses the selected addresses for its checksum, then consumes
/// this value with `submit`. Dropping it only releases the packet and borrows;
/// it does not occupy queue slots or advance the identification sequence.
#[must_use = "fill the transport header and submit the prepared IPv4 send"]
pub(crate) struct PreparedIpv4Send<'a> {
    output: &'a mut Ipv4Output,
    resources: Ipv4OutputResources<'a>,
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
        self.decision.destination
    }

    /// Borrows transport bytes while keeping the selected addresses immutable.
    pub(crate) fn transport_mut(&mut self) -> &mut [u8] {
        self.packet.transport_mut()
    }

    /// Completes IPv4 encapsulation and consumes the datagram for submission.
    ///
    /// Capacity remains available under this transaction's exclusive borrow, and
    /// every size decision was made during preparation. Only a malformed packet
    /// or a broken capacity reservation can fail here, and neither is retryable.
    /// Successful loopback submission can still drop on RX overflow.
    pub(crate) fn submit(mut self) -> KResult {
        let dont_fragment = self.decision.dont_fragment;
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
            self.decision.destination,
            self.options.protocol,
            self.options.ttl,
            identification,
            dont_fragment,
        )
        .ok_or(LinuxError::EMSGSIZE)?;
        match self.decision.submission {
            SubmissionTarget::Queued => xmit::queue_packet(
                self.resources.tx_queue,
                self.packet.packet,
                self.options.oif,
                self.decision.mtu,
                identification,
            ),
            SubmissionTarget::Immediate(target) => xmit::transmit_now(
                self.resources.devices,
                self.packet.packet,
                self.decision.mtu,
                identification,
                self.decision.source,
                self.decision.destination,
                target,
            ),
        }
    }
}

#[derive(Clone, Copy)]
enum SubmissionTarget {
    Queued,
    Immediate(OutputTarget),
}

/// Source and submission strategy resolved under one configuration guard.
struct Ipv4SendDecision {
    destination: Ipv4Address,
    source: Ipv4Address,
    mtu: Option<usize>,
    /// Chosen once during preparation so submission cannot reject the datagram.
    dont_fragment: bool,
    submission: SubmissionTarget,
}

impl Ipv4Output {
    /// Selects addresses and checks capacity without modifying the datagram.
    ///
    /// On error, returns the original builder. Returns `EACCES` for a forbidden
    /// broadcast, `ENETUNREACH` for an unreachable route or non-local source,
    /// `EINVAL` for an invalid source, `ENODEV` for a missing bound interface,
    /// `EMSGSIZE` for a DF datagram larger than the MTU or an MTU too small to
    /// fragment, and `WouldBlock` for insufficient queue capacity. Size errors
    /// are checked first because waiting cannot resolve them.
    /// The returned transaction exclusively borrows all resources through
    /// submission, so neither its route nor its capacity can become stale.
    pub(crate) fn prepare<'a>(
        &'a mut self,
        resources: Ipv4OutputResources<'a>,
        options: Ipv4SendOptions,
        packet: Ipv4PacketBuilder,
    ) -> Result<PreparedIpv4Send<'a>, (KError, Ipv4PacketBuilder)> {
        let decision = match prepare_output(&resources, options, packet.packet.len()) {
            Ok(decision) => decision,
            Err(error) => return Err((error, packet)),
        };
        Ok(PreparedIpv4Send {
            output: self,
            resources,
            options,
            packet,
            decision,
        })
    }
}

fn prepare_output(
    resources: &Ipv4OutputResources<'_>,
    options: Ipv4SendOptions,
    packet_len: usize,
) -> KResult<Ipv4SendDecision> {
    let Ipv4OutputResources {
        routes,
        devices,
        tx_queue,
    } = resources;
    if requires_broadcast(devices, options.dst_addr) && !options.allow_broadcast {
        return Err(LinuxError::EACCES.into());
    }
    let (destination, source, mtu, link_kind, target) = if options.dst_addr.is_broadcast() {
        // Bound broadcasts work without a route. Preserve the optional MTU and
        // loopback policy supplied by a route when one exists.
        let device = routes
            .lookup(&IpAddress::Ipv4(options.dst_addr))
            .and_then(|rule| devices.get_by_id(rule.dev));
        (
            options.dst_addr,
            output_source_address(
                routes,
                devices,
                options.dst_addr,
                options.oif,
                options.bound_src,
            )?,
            device.map(|device| device.mtu().min(usize::from(u16::MAX))),
            device.map(|device| device.link_kind()),
            OutputTarget::LimitedBroadcast,
        )
    } else {
        let route = resolve_output_route(
            routes,
            devices,
            options.dst_addr,
            options.oif,
            options.bound_src,
        )?;
        (
            route.destination,
            route.source,
            Some(route.mtu),
            Some(route.link_kind),
            OutputTarget::Routed {
                dev: route.dev,
                next_hop: route.next_hop,
            },
        )
    };
    let dont_fragment = options
        .fragmentation
        .should_set_dont_fragment(packet_len, mtu);
    // A datagram that may neither fragment nor fit is a permanent error.
    // Reporting it before the capacity check keeps a full queue from
    // turning it into a wait that can only end in `EMSGSIZE`.
    if dont_fragment && mtu.is_some_and(|mtu| packet_len > mtu) {
        return Err(LinuxError::EMSGSIZE.into());
    }
    let packet_count = output_packet_count(packet_len, mtu)?;
    let submission = if link_kind == Some(LinkKind::Loopback) {
        SubmissionTarget::Immediate(target)
    } else {
        if tx_queue.is_full() || !tx_queue.can_push(packet_count) {
            return Err(KError::WouldBlock);
        }
        SubmissionTarget::Queued
    };
    Ok(Ipv4SendDecision {
        destination,
        source,
        mtu,
        dont_fragment,
        submission,
    })
}

/// Returns whether a destination requires the socket's broadcast permission.
pub(crate) fn requires_broadcast(devices: &DeviceRegistry, address: Ipv4Address) -> bool {
    address.is_broadcast()
        || devices
            .iter_ipv4_addrs()
            .any(|(_, entry)| entry.addr.is_directed_broadcast(address))
}

fn output_packet_count(packet_len: usize, mtu: Option<usize>) -> KResult<usize> {
    if packet_len > usize::from(u16::MAX) {
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
