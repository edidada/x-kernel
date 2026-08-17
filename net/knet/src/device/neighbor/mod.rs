// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Neighbor cache with RFC 4861 / Linux NUD states.
//!
//! Protocol bits (NA S/O, ARP opcode) stay in the NDP/ARP parsers.
//! This module owns the NUD state machine and pending transmit queue.

mod cache;

pub(crate) use cache::NeighborCache;

use crate::{buf::PacketBuf, ip::IpAddress, wire::MacAddress};

/// Userspace-visible neighbor snapshot for `RTM_GETNEIGH`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NeighborSnapshot {
    pub(crate) ifindex: i32,
    pub(crate) addr: IpAddress,
    pub(crate) mac: Option<MacAddress>,
    pub(crate) nud_state: u16,
}

/// How an inbound ARP/NDP mapping should update the cache.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NeighborLearn {
    pub(crate) mac: Option<MacAddress>,
    pub(crate) reachability: ReachabilityHint,
    pub(crate) lladdr: LladdrPolicy,
}

/// Whether the mapping also confirms two-way reachability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReachabilityHint {
    /// Solicited NA / ARP reply: `NUD_REACHABLE`.
    Confirmed,
    /// NS SLLA / unsolicited NA / ARP request: `NUD_STALE`.
    Unconfirmed,
}

/// Whether a different link-layer address may replace the cached MAC.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LladdrPolicy {
    /// NA Override, ARP, and NS SLLA replace a different MAC.
    Override,
    /// Linux `NEIGH_UPDATE_F_WEAK_OVERRIDE`: suspect a connected mapping.
    Weak,
}

impl NeighborLearn {
    pub(crate) fn confirmed_override(mac: MacAddress) -> Self {
        Self {
            mac: Some(mac),
            reachability: ReachabilityHint::Confirmed,
            lladdr: LladdrPolicy::Override,
        }
    }

    pub(crate) fn unconfirmed_override(mac: MacAddress) -> Self {
        Self {
            mac: Some(mac),
            reachability: ReachabilityHint::Unconfirmed,
            lladdr: LladdrPolicy::Override,
        }
    }

    pub(crate) fn from_na_flags(
        mac: Option<MacAddress>,
        is_solicited: bool,
        is_override: bool,
    ) -> Self {
        Self {
            mac,
            reachability: if is_solicited {
                ReachabilityHint::Confirmed
            } else {
                ReachabilityHint::Unconfirmed
            },
            lladdr: if is_override {
                LladdrPolicy::Override
            } else {
                LladdrPolicy::Weak
            },
        }
    }
}

/// Solicitation to emit after a cache transition or timer expiry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SolicitKind {
    /// Address resolution: multicast/broadcast NS or ARP request.
    Multicast,
    /// NUD probe: unicast NS or ARP request to the cached MAC.
    Unicast(MacAddress),
}

/// Result of resolving a unicast next hop.
pub(crate) enum ResolveResult {
    /// Cached MAC is usable; send this packet now.
    Transmit { mac: MacAddress, packet: PacketBuf },
    /// Packet was queued. `Some` means the caller should send a solicitation.
    Queued(Option<SolicitKind>),
    /// The unresolved queue is full; the packet was dropped.
    Dropped,
}

/// Timer-driven work for the Ethernet transmitter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NeighborTimerWork {
    pub(crate) addr: IpAddress,
    pub(crate) solicit: SolicitKind,
}
