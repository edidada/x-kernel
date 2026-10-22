// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Output route resolution and IPv4 fragmentation with an explicit MTU.

use alloc::{vec, vec::Vec};

use kerrno::{KError, KResult, LinuxError};

use crate::{
    device::{DeviceRegistry, LinkKind, NetDeviceId},
    ip::IpAddress,
    network::{ipv4, router::RouteTable},
};

/// A single-route output decision, valid within the current Router critical section.
///
/// Queued packets retain their socket's interface constraint and resolve again
/// at dispatch. Limited-broadcast fan-out is handled separately by the caller.
pub(crate) struct OutputRoute {
    /// Default source; an explicitly bound socket source takes precedence.
    pub(crate) source: IpAddress,
    pub(crate) dev: NetDeviceId,
    pub(crate) next_hop: IpAddress,
    pub(crate) mtu: usize,
    pub(crate) link_kind: LinkKind,
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

/// Fragments stack-generated IPv4 output using the MTU chosen during preparation.
///
/// `None` preserves the unfragmented path for limited broadcasts without a route.
/// The caller owns the identification sequence; it advances only when a packet
/// exceeds the MTU, including attempts rejected because DF is set.
pub(crate) fn fragment_ipv4_packet(
    packet: Vec<u8>,
    mtu: Option<usize>,
    next_identification: &mut u16,
) -> KResult<Vec<Vec<u8>>> {
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
    let identification = *next_identification;
    *next_identification = next_identification.wrapping_add(1).max(1);
    ipv4::fragment_output_packet(&packet, mtu, identification).map_err(|err| match err {
        ipv4::Ipv4FragmentError::Malformed => KError::InvalidInput,
        ipv4::Ipv4FragmentError::DontFragment | ipv4::Ipv4FragmentError::MtuTooSmall => {
            LinuxError::EMSGSIZE.into()
        }
    })
}
