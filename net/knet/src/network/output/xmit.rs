// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device submission of finished IPv4 datagrams.
//!
//! Sends arrive with their route and source already decided by preparation.
//! Only a packet that waited in the TX queue is routed again here, because the
//! configuration may have changed while it waited.

use alloc::{vec, vec::Vec};

use kerrno::{KError, KResult, LinuxError};
use khal::time::monotonic_time;
use ktime_types::MonotonicInstant;

use crate::{
    buf::{PacketBuf, PacketOwner},
    device::{DeviceRegistry, NetDeviceId},
    ip::{IpAddress, Ipv4Address},
    network::{
        ipv4::{self, Ipv4Header},
        output_route::{has_local_source, resolve_output_route},
        router::RouteTable,
    },
    runtime::tx_queue::DataTxQueue,
};

/// Where a datagram leaves the host.
#[derive(Clone, Copy)]
pub(super) enum OutputTarget {
    Routed {
        dev: NetDeviceId,
        next_hop: Ipv4Address,
    },
    LimitedBroadcast,
}

/// RX work produced by dispatch, independent of device transmit success.
#[derive(Default)]
pub(crate) struct Ipv4DispatchOutcome {
    pub(crate) has_rx_work: bool,
}

/// Dispatches an already queued packet using the current routing state.
///
/// Normalizes the byte-buffer boundary shared with smoltcp, then resolves the
/// current output route through [`resolve_output_route`] with the packet's
/// source and interface binding. The source keeps the value written at
/// preparation because the transport checksum already depends on it.
pub(crate) fn dispatch_queued(
    routes: &RouteTable,
    devices: &mut DeviceRegistry,
    mut packet: PacketBuf,
    timestamp: MonotonicInstant,
) -> Ipv4DispatchOutcome {
    let Some((source, destination)) = normalize_packet(&mut packet) else {
        return Ipv4DispatchOutcome::default();
    };
    if destination.is_broadcast() {
        if !(source.is_unspecified() || has_local_source(devices, source)) {
            warn!("Dropping IPv4 broadcast packet with source {}", source);
            return Ipv4DispatchOutcome::default();
        }
        return send_broadcast(devices, packet, source, destination, timestamp);
    }
    // An already checksummed packet cannot request automatic source selection.
    if source.is_unspecified() {
        warn!("Dropping queued IPv4 packet with unspecified source");
        return Ipv4DispatchOutcome::default();
    }
    let oif = NetDeviceId::new(packet.ifindex());
    let route = match resolve_output_route(routes, devices, destination, oif, Some(source)) {
        Ok(route) => route,
        Err(error) => {
            warn!(
                "Dropping queued IPv4 packet {} -> {}: no usable output route ({error:?})",
                source, destination
            );
            return Ipv4DispatchOutcome::default();
        }
    };
    send_routed(
        devices,
        route.dev,
        route.next_hop,
        route.source,
        packet,
        timestamp,
    )
}

/// Fragments before atomically enqueuing the complete group of packets.
///
/// Capacity was reserved by preparation under the same exclusive borrow, so a
/// failure here is not retryable: the caller's payload is already consumed.
pub(crate) fn queue_packet(
    tx_queue: &mut DataTxQueue,
    packet: Vec<u8>,
    oif: Option<NetDeviceId>,
    mtu: Option<usize>,
    identification: u16,
) -> KResult {
    let packets = fragment(packet, mtu, identification)?;
    let ifindex = oif.map_or(0, NetDeviceId::ifindex);
    tx_queue
        .try_extend(
            packets.into_iter().map(|packet| {
                PacketBuf::from_ip_packet_vec(ifindex, packet, PacketOwner::Ipv4Stack)
            }),
        )
        .map_err(|_| KError::from(LinuxError::ENOBUFS))
}

/// Fragments and transmits through the already selected target.
///
/// Used for loopback, which bypasses the TX queue. Preparation validated the
/// source and route, so neither is looked up or parsed again.
pub(super) fn transmit_now(
    devices: &mut DeviceRegistry,
    packet: Vec<u8>,
    mtu: Option<usize>,
    identification: u16,
    source: Ipv4Address,
    destination: Ipv4Address,
    target: OutputTarget,
) -> KResult {
    let timestamp = monotonic_time();
    for packet in fragment(packet, mtu, identification)? {
        // Immediate loopback submission historically carries no queue binding.
        let mut packet = PacketBuf::from_ip_packet_vec(0, packet, PacketOwner::Ipv4Stack);
        if ipv4::prepare_output_packet(&mut packet).is_err() {
            continue;
        }
        let _outcome = match target {
            OutputTarget::Routed { dev, next_hop } => {
                send_routed(devices, dev, next_hop, source, packet, timestamp)
            }
            OutputTarget::LimitedBroadcast => {
                send_broadcast(devices, packet, source, destination, timestamp)
            }
        };
    }
    Ok(())
}

fn fragment(packet: Vec<u8>, mtu: Option<usize>, identification: u16) -> KResult<Vec<Vec<u8>>> {
    if packet.len() > usize::from(u16::MAX) {
        return Err(KError::InvalidInput);
    }
    let Some(mtu) = mtu else {
        return Ok(vec![packet]);
    };
    if packet.len() <= mtu {
        return Ok(vec![packet]);
    }
    ipv4::fragment_output_packet(&packet, mtu, identification).map_err(|err| match err {
        ipv4::Ipv4FragmentError::Malformed => KError::InvalidInput,
        // Preparation rejects both before any capacity is reserved.
        ipv4::Ipv4FragmentError::DontFragment | ipv4::Ipv4FragmentError::MtuTooSmall => {
            LinuxError::EMSGSIZE.into()
        }
    })
}

fn normalize_packet(packet: &mut PacketBuf) -> Option<(Ipv4Address, Ipv4Address)> {
    ipv4::prepare_output_packet(packet).ok()?;
    let header = Ipv4Header::parse_header_only(packet.network_packet()?).ok()?;
    Some((header.src_addr(), header.dst_addr()))
}

fn send_routed(
    devices: &mut DeviceRegistry,
    dev_id: NetDeviceId,
    next_hop: Ipv4Address,
    source: Ipv4Address,
    packet: PacketBuf,
    timestamp: MonotonicInstant,
) -> Ipv4DispatchOutcome {
    let Some(dev) = devices.get_mut_by_id(dev_id) else {
        return Ipv4DispatchOutcome::default();
    };
    Ipv4DispatchOutcome {
        has_rx_work: dev.send_ip_packet(
            dev_id.ifindex(),
            next_hop.into(),
            IpAddress::Ipv4(source),
            packet,
            timestamp,
        ),
    }
}

/// Replicates a limited broadcast to every device allowed by its binding.
fn send_broadcast(
    devices: &mut DeviceRegistry,
    packet: PacketBuf,
    source: Ipv4Address,
    destination: Ipv4Address,
    timestamp: MonotonicInstant,
) -> Ipv4DispatchOutcome {
    let src_addr = IpAddress::Ipv4(source);
    let dst_addr = IpAddress::Ipv4(destination);
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
        has_rx_work |= dev.send_ip_packet(ifindex, dst_addr, src_addr, packet.clone(), timestamp);
    }
    let last_ifindex = last_id.ifindex();
    if bound_if == 0 || bound_if == last_ifindex {
        has_rx_work |=
            last_device.send_ip_packet(last_ifindex, dst_addr, src_addr, packet, timestamp);
    }
    Ipv4DispatchOutcome { has_rx_work }
}
