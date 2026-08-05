// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Neighbor cache and link-address resolution state.

use alloc::{collections::VecDeque, vec::Vec};

use hashbrown::HashMap;
use ktime_types::{MonotonicInstant, TimeSpan};

use crate::{
    buf::PacketBuf,
    consts::NEIGHBOR_MAX_PENDING_PACKETS,
    ip::{IpAddress, Ipv4Address, Ipv6Address},
    link::wire::MacAddress,
};

/// Neighbor states accepted by the control-plane API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NeighborState {
    Incomplete,
    Permanent { hardware_addr: [u8; 6] },
}

/// A control-plane neighbor update targeted at one network device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NeighborUpdate {
    pub(crate) dev: usize,
    pub(crate) dst: IpAddress,
    pub(crate) state: NeighborState,
}

/// The trust level assigned to a link-layer observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NeighborObservation {
    Reachable,
    Stale,
}

/// An IP packet waiting for a link-layer address or a retryable TX result.
pub(crate) struct PendingTxPacket {
    pub(crate) next_hop: IpAddress,
    pub(crate) source_addr: IpAddress,
    pub(crate) next_attempt_at: MonotonicInstant,
    pub(crate) is_broadcast: bool,
    pub(crate) packet: PacketBuf,
}

impl PendingTxPacket {
    fn new_unicast(next_hop: IpAddress, source_addr: IpAddress, packet: PacketBuf) -> Self {
        Self {
            next_hop,
            source_addr,
            next_attempt_at: MonotonicInstant::ORIGIN,
            is_broadcast: false,
            packet,
        }
    }

    pub(crate) fn new_broadcast(
        next_hop: IpAddress,
        source_addr: IpAddress,
        packet: PacketBuf,
    ) -> Self {
        Self {
            next_hop,
            source_addr,
            next_attempt_at: MonotonicInstant::ORIGIN,
            is_broadcast: true,
            packet,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NeighborRequest {
    pub(crate) target: IpAddress,
    pub(crate) source_addr: IpAddress,
}

pub(crate) enum NeighborAction {
    Send {
        packet: PendingTxPacket,
        hardware_address: MacAddress,
    },
    Request(NeighborRequest),
}

pub(crate) enum NeighborSendDecision {
    Send {
        packet: PendingTxPacket,
        hardware_address: MacAddress,
    },
    Queued {
        request: Option<NeighborRequest>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NeighborEntry {
    Incomplete {
        next_request_at: MonotonicInstant,
        probes_sent: u8,
    },
    Reachable {
        hardware_address: MacAddress,
        expires_at: MonotonicInstant,
        updated_at: MonotonicInstant,
    },
    Stale {
        hardware_address: MacAddress,
        stale_since: MonotonicInstant,
        updated_at: MonotonicInstant,
    },
    Delay {
        hardware_address: MacAddress,
        probe_at: MonotonicInstant,
        updated_at: MonotonicInstant,
    },
    Probe {
        hardware_address: MacAddress,
        next_request_at: MonotonicInstant,
        probes_sent: u8,
        updated_at: MonotonicInstant,
    },
    Failed {
        failed_at: MonotonicInstant,
    },
    Permanent {
        hardware_address: MacAddress,
    },
}

impl NeighborEntry {
    fn from_control_state(state: NeighborState) -> Self {
        match state {
            NeighborState::Incomplete => Self::Incomplete {
                next_request_at: MonotonicInstant::ORIGIN,
                probes_sent: 0,
            },
            NeighborState::Permanent { hardware_addr } => Self::Permanent {
                hardware_address: MacAddress(hardware_addr),
            },
        }
    }

    fn new_dynamic(
        hardware_address: MacAddress,
        protocol_address: IpAddress,
        now: MonotonicInstant,
        observation: NeighborObservation,
    ) -> Self {
        match observation {
            NeighborObservation::Reachable => Self::Reachable {
                hardware_address,
                expires_at: now
                    + NeighborResolver::dynamic_reachable_time(hardware_address, protocol_address),
                updated_at: now,
            },
            NeighborObservation::Stale => Self::Stale {
                hardware_address,
                stale_since: now,
                updated_at: now,
            },
        }
    }

    fn learn_dynamic(
        &mut self,
        hardware_address: MacAddress,
        protocol_address: IpAddress,
        now: MonotonicInstant,
        observation: NeighborObservation,
    ) {
        if matches!(self, Self::Permanent { .. }) || !self.can_update_dynamic(hardware_address, now)
        {
            return;
        }
        *self = Self::new_dynamic(hardware_address, protocol_address, now, observation);
    }

    fn can_update_dynamic(&self, hardware_address: MacAddress, now: MonotonicInstant) -> bool {
        match self {
            Self::Reachable {
                hardware_address: current,
                updated_at,
                ..
            }
            | Self::Stale {
                hardware_address: current,
                updated_at,
                ..
            }
            | Self::Delay {
                hardware_address: current,
                updated_at,
                ..
            }
            | Self::Probe {
                hardware_address: current,
                updated_at,
                ..
            } => *current == hardware_address || now >= *updated_at + NeighborResolver::LOCK_TIME,
            Self::Incomplete { .. } | Self::Failed { .. } | Self::Permanent { .. } => true,
        }
    }

    fn record_request_attempt(&mut self, now: MonotonicInstant) {
        match self {
            Self::Incomplete {
                next_request_at,
                probes_sent,
            }
            | Self::Probe {
                next_request_at,
                probes_sent,
                ..
            } => {
                *probes_sent = (*probes_sent).saturating_add(1);
                *next_request_at = now + NeighborResolver::REQUEST_RETRY_INTERVAL;
            }
            Self::Reachable { .. }
            | Self::Stale { .. }
            | Self::Delay { .. }
            | Self::Failed { .. }
            | Self::Permanent { .. } => {}
        }
    }

    fn next_poll_at(&self) -> Option<MonotonicInstant> {
        match self {
            Self::Incomplete {
                next_request_at, ..
            } => Some(*next_request_at),
            Self::Reachable { expires_at, .. } => Some(*expires_at),
            Self::Stale { stale_since, .. } => {
                Some(*stale_since + NeighborResolver::NEIGHBOR_GC_TIME)
            }
            Self::Delay { probe_at, .. } => Some(*probe_at),
            Self::Probe {
                next_request_at, ..
            } => Some(*next_request_at),
            Self::Failed { failed_at } => Some(*failed_at + NeighborResolver::NEIGHBOR_GC_TIME),
            Self::Permanent { .. } => None,
        }
    }

    fn transmit_hardware_address(&mut self, now: MonotonicInstant) -> Option<MacAddress> {
        match *self {
            Self::Permanent { hardware_address }
            | Self::Delay {
                hardware_address, ..
            }
            | Self::Probe {
                hardware_address, ..
            } => Some(hardware_address),
            Self::Reachable {
                hardware_address,
                expires_at,
                ..
            } if expires_at > now => Some(hardware_address),
            Self::Reachable {
                hardware_address,
                updated_at,
                ..
            } => {
                *self = Self::Delay {
                    hardware_address,
                    probe_at: now + NeighborResolver::DELAY_PROBE_TIME,
                    updated_at,
                };
                Some(hardware_address)
            }
            Self::Stale {
                hardware_address,
                stale_since,
                updated_at,
            } if now < stale_since + NeighborResolver::NEIGHBOR_GC_TIME => {
                *self = Self::Delay {
                    hardware_address,
                    probe_at: now + NeighborResolver::DELAY_PROBE_TIME,
                    updated_at,
                };
                Some(hardware_address)
            }
            Self::Stale { .. } => {
                *self = Self::Failed { failed_at: now };
                None
            }
            Self::Incomplete { .. } | Self::Failed { .. } => None,
        }
    }

    fn needs_request(&self, now: MonotonicInstant) -> bool {
        match self {
            Self::Incomplete {
                next_request_at, ..
            }
            | Self::Probe {
                next_request_at, ..
            } => *next_request_at <= now,
            Self::Reachable { .. }
            | Self::Stale { .. }
            | Self::Delay { .. }
            | Self::Failed { .. }
            | Self::Permanent { .. } => false,
        }
    }

    fn should_fail(&self, now: MonotonicInstant) -> bool {
        match self {
            Self::Incomplete {
                next_request_at,
                probes_sent,
            }
            | Self::Probe {
                next_request_at,
                probes_sent,
                ..
            } => *probes_sent >= NeighborResolver::MAX_PROBES && *next_request_at <= now,
            Self::Reachable { .. }
            | Self::Stale { .. }
            | Self::Delay { .. }
            | Self::Failed { .. }
            | Self::Permanent { .. } => false,
        }
    }

    fn is_failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }

    fn restart_resolution(&mut self, now: MonotonicInstant) {
        if self.is_failed() {
            *self = Self::Incomplete {
                next_request_at: now,
                probes_sent: 0,
            };
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NeighborMaintenance {
    Request(IpAddress),
    Failed(IpAddress),
    Remove(IpAddress),
}

struct NeighborCache {
    entries: HashMap<IpAddress, NeighborEntry>,
}

impl NeighborCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    fn observe(
        &mut self,
        protocol_address: IpAddress,
        hardware_address: MacAddress,
        now: MonotonicInstant,
        observation: NeighborObservation,
    ) {
        let target = protocol_address;
        self.entries
            .entry(target)
            .and_modify(|entry| {
                entry.learn_dynamic(hardware_address, protocol_address, now, observation)
            })
            .or_insert_with(|| {
                NeighborEntry::new_dynamic(hardware_address, protocol_address, now, observation)
            });
    }

    fn apply_control_state(&mut self, target: IpAddress, state: NeighborState) {
        self.entries
            .insert(target, NeighborEntry::from_control_state(state));
    }

    fn transmit_hardware_address(
        &mut self,
        target: IpAddress,
        now: MonotonicInstant,
    ) -> Option<MacAddress> {
        self.entries
            .get_mut(&target)
            .and_then(|entry| entry.transmit_hardware_address(now))
    }

    fn needs_request(&self, target: IpAddress, now: MonotonicInstant) -> bool {
        self.entries
            .get(&target)
            .is_some_and(|entry| entry.needs_request(now))
    }

    fn is_failed(&self, target: IpAddress) -> bool {
        self.entries
            .get(&target)
            .is_some_and(NeighborEntry::is_failed)
    }

    fn restart_resolution(&mut self, target: IpAddress, now: MonotonicInstant) {
        if let Some(entry) = self.entries.get_mut(&target) {
            entry.restart_resolution(now);
        }
    }

    fn record_request_attempt(&mut self, target: IpAddress, now: MonotonicInstant) {
        self.entries
            .entry(target)
            .or_insert(NeighborEntry::Incomplete {
                next_request_at: MonotonicInstant::ORIGIN,
                probes_sent: 0,
            })
            .record_request_attempt(now);
    }

    fn maintain(&mut self, now: MonotonicInstant) -> Vec<NeighborMaintenance> {
        let mut maintenance = Vec::new();
        for (target, entry) in &mut self.entries {
            match entry {
                NeighborEntry::Reachable {
                    hardware_address,
                    expires_at,
                    updated_at,
                } if *expires_at <= now => {
                    *entry = NeighborEntry::Stale {
                        hardware_address: *hardware_address,
                        stale_since: now,
                        updated_at: *updated_at,
                    };
                }
                NeighborEntry::Stale { stale_since, .. }
                    if now >= *stale_since + NeighborResolver::NEIGHBOR_GC_TIME =>
                {
                    maintenance.push(NeighborMaintenance::Remove(*target));
                }
                NeighborEntry::Delay {
                    hardware_address,
                    probe_at,
                    updated_at,
                } if *probe_at <= now => {
                    *entry = NeighborEntry::Probe {
                        hardware_address: *hardware_address,
                        next_request_at: now,
                        probes_sent: 0,
                        updated_at: *updated_at,
                    };
                    maintenance.push(NeighborMaintenance::Request(*target));
                }
                NeighborEntry::Failed { failed_at }
                    if now >= *failed_at + NeighborResolver::NEIGHBOR_GC_TIME =>
                {
                    maintenance.push(NeighborMaintenance::Remove(*target));
                }
                _ if entry.should_fail(now) => {
                    // Match Linux neigh_timer_handler: write NUD_FAILED before
                    // invalidating the arp_queue, otherwise next_poll_at stays in
                    // the past and maintain re-emits Failed every poll.
                    *entry = NeighborEntry::Failed { failed_at: now };
                    maintenance.push(NeighborMaintenance::Failed(*target));
                }
                _ if entry.needs_request(now) => {
                    maintenance.push(NeighborMaintenance::Request(*target));
                }
                _ => {}
            }
        }

        for event in &maintenance {
            if let NeighborMaintenance::Remove(target) = event {
                self.entries.remove(target);
            }
        }
        maintenance
    }

    fn next_poll_at(&self) -> Option<MonotonicInstant> {
        self.entries
            .values()
            .filter_map(NeighborEntry::next_poll_at)
            .min()
    }

    fn contains(&self, target: IpAddress) -> bool {
        self.entries.contains_key(&target)
    }

    fn next_poll_at_for(&self, target: IpAddress) -> Option<MonotonicInstant> {
        self.entries
            .get(&target)
            .and_then(NeighborEntry::next_poll_at)
    }
}

/// Resolves network-layer next hops into link-layer addresses.
pub(crate) struct NeighborResolver {
    cache: NeighborCache,
    pending_tx: VecDeque<PendingTxPacket>,
}

impl NeighborResolver {
    const DELAY_PROBE_TIME: TimeSpan = TimeSpan::from_secs(5);
    const LOCK_TIME: TimeSpan = TimeSpan::from_secs(1);
    const MAX_PROBES: u8 = 6;
    const NEIGHBOR_GC_TIME: TimeSpan = TimeSpan::from_secs(60);
    const NEIGHBOR_REACHABLE_TIME: TimeSpan = TimeSpan::from_secs(300);
    const REQUEST_RETRY_INTERVAL: TimeSpan = TimeSpan::from_secs(1);
    const TX_RETRY_INTERVAL: TimeSpan = TimeSpan::from_secs(1);

    pub(crate) fn new() -> Self {
        Self {
            cache: NeighborCache::new(),
            pending_tx: VecDeque::with_capacity(NEIGHBOR_MAX_PENDING_PACKETS),
        }
    }

    pub(crate) fn prepare_unicast(
        &mut self,
        next_hop: IpAddress,
        source_addr: IpAddress,
        packet: PacketBuf,
        now: MonotonicInstant,
    ) -> NeighborSendDecision {
        let hardware_address = self.cache.transmit_hardware_address(next_hop, now);
        if let Some(hardware_address) = hardware_address {
            return NeighborSendDecision::Send {
                packet: PendingTxPacket::new_unicast(next_hop, source_addr, packet),
                hardware_address,
            };
        }

        let should_request = if self.cache.is_failed(next_hop) {
            self.cache.restart_resolution(next_hop, now);
            true
        } else {
            self.cache.needs_request(next_hop, now) || !self.cache.contains(next_hop)
        };
        let queued =
            self.enqueue_pending(PendingTxPacket::new_unicast(next_hop, source_addr, packet));
        NeighborSendDecision::Queued {
            request: queued.then_some(should_request).and_then(|should_request| {
                should_request.then_some(NeighborRequest {
                    target: next_hop,
                    source_addr,
                })
            }),
        }
    }

    pub(crate) fn new_broadcast_packet(
        &self,
        next_hop: IpAddress,
        source_addr: IpAddress,
        packet: PacketBuf,
    ) -> PendingTxPacket {
        PendingTxPacket::new_broadcast(next_hop, source_addr, packet)
    }

    pub(crate) fn requeue(&mut self, mut packet: PendingTxPacket, now: MonotonicInstant) {
        packet.next_attempt_at = now + Self::TX_RETRY_INTERVAL;
        self.enqueue_pending(packet);
    }

    pub(crate) fn record_request_attempt(&mut self, target: IpAddress, now: MonotonicInstant) {
        self.cache.record_request_attempt(target, now);
    }

    pub(crate) fn observe(
        &mut self,
        protocol_address: IpAddress,
        hardware_address: MacAddress,
        now: MonotonicInstant,
        observation: NeighborObservation,
        fallback_source_addr: Option<IpAddress>,
    ) -> Vec<NeighborAction> {
        self.cache
            .observe(protocol_address, hardware_address, now, observation);
        self.mark_pending_due(protocol_address);
        self.collect_actions(now, fallback_source_addr)
    }

    pub(crate) fn apply_control_update(
        &mut self,
        target: IpAddress,
        state: NeighborState,
        now: MonotonicInstant,
        fallback_source_addr: Option<IpAddress>,
    ) -> Vec<NeighborAction> {
        self.cache.apply_control_state(target, state);
        self.mark_pending_due(target);
        self.collect_actions(now, fallback_source_addr)
    }

    pub(crate) fn poll(
        &mut self,
        now: MonotonicInstant,
        fallback_source_addr: Option<IpAddress>,
    ) -> Vec<NeighborAction> {
        self.collect_actions(now, fallback_source_addr)
    }

    pub(crate) fn next_poll_at(&self) -> Option<MonotonicInstant> {
        match (self.earliest_pending_poll_at(), self.cache.next_poll_at()) {
            (Some(pending), Some(neighbor)) => Some(pending.min(neighbor)),
            (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
            (None, None) => None,
        }
    }

    pub(crate) fn remove_pending_source(&mut self, source_addr: IpAddress) {
        self.pending_tx
            .retain(|pending| pending.source_addr != source_addr);
    }

    pub(crate) fn has_neighbor(&self, target: IpAddress) -> bool {
        self.cache.contains(target)
    }

    fn collect_actions(
        &mut self,
        now: MonotonicInstant,
        fallback_source_addr: Option<IpAddress>,
    ) -> Vec<NeighborAction> {
        let mut request_targets = Vec::new();
        let mut actions = Vec::new();
        for maintenance in self.cache.maintain(now) {
            match maintenance {
                NeighborMaintenance::Request(target) => {
                    Self::push_request_target(&mut request_targets, target);
                }
                NeighborMaintenance::Failed(target) => {
                    self.pending_tx.retain(|pending| pending.next_hop != target);
                    warn!("Neighbor resolution failed for {}", target);
                }
                NeighborMaintenance::Remove(_) => {}
            }
        }

        let pending_count = self.pending_tx.len();
        for _ in 0..pending_count {
            let pending = self.pending_tx.pop_front().unwrap();
            if pending.next_attempt_at > now {
                self.pending_tx.push_back(pending);
                continue;
            }

            let action = if pending.is_broadcast {
                Some(NeighborAction::Send {
                    packet: pending,
                    hardware_address: MacAddress::BROADCAST,
                })
            } else {
                match self.cache.transmit_hardware_address(pending.next_hop, now) {
                    Some(hardware_address) => Some(NeighborAction::Send {
                        packet: pending,
                        hardware_address,
                    }),
                    None if self.cache.is_failed(pending.next_hop) => None,
                    None if self.cache.needs_request(pending.next_hop, now) => {
                        Self::push_request_target(&mut request_targets, pending.next_hop);
                        self.pending_tx.push_back(pending);
                        None
                    }
                    None => {
                        self.pending_tx.push_back(pending);
                        None
                    }
                }
            };

            if let Some(action) = action {
                actions.push(action);
            }
        }

        actions.extend(request_targets.into_iter().map(|target| {
            NeighborAction::Request(NeighborRequest {
                target,
                source_addr: self.source_addr_for(target, fallback_source_addr),
            })
        }));
        actions
    }

    fn push_request_target(request_targets: &mut Vec<IpAddress>, target: IpAddress) {
        if !request_targets.contains(&target) {
            request_targets.push(target);
        }
    }

    fn enqueue_pending(&mut self, pending: PendingTxPacket) -> bool {
        if self.pending_tx.len() >= NEIGHBOR_MAX_PENDING_PACKETS {
            warn!("Pending packets buffer is full, dropping packet");
            return false;
        }
        self.pending_tx.push_back(pending);
        true
    }

    fn mark_pending_due(&mut self, target: IpAddress) {
        for pending in &mut self.pending_tx {
            if pending.next_hop == target {
                pending.next_attempt_at = MonotonicInstant::ORIGIN;
            }
        }
    }

    fn source_addr_for(
        &self,
        target: IpAddress,
        fallback_source_addr: Option<IpAddress>,
    ) -> IpAddress {
        self.pending_tx
            .iter()
            .find(|pending| pending.next_hop == target)
            .map(|pending| pending.source_addr)
            .or(fallback_source_addr)
            .unwrap_or_else(|| Self::unspecified_address(target))
    }

    fn earliest_pending_poll_at(&self) -> Option<MonotonicInstant> {
        self.pending_tx
            .iter()
            .map(|pending| {
                let neighbor_deadline = if pending.is_broadcast {
                    None
                } else {
                    self.cache.next_poll_at_for(pending.next_hop)
                };
                if pending.next_attempt_at == MonotonicInstant::ORIGIN {
                    neighbor_deadline.unwrap_or(MonotonicInstant::ORIGIN)
                } else {
                    neighbor_deadline
                        .map(|deadline| deadline.min(pending.next_attempt_at))
                        .unwrap_or(pending.next_attempt_at)
                }
            })
            .min()
    }

    fn dynamic_reachable_time(
        hardware_address: MacAddress,
        protocol_address: IpAddress,
    ) -> TimeSpan {
        // A stable per-neighbor spread avoids synchronized expirations without
        // making early boot depend on a random source.
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        match protocol_address {
            IpAddress::Ipv4(address) => {
                hash ^= 4;
                for byte in address.octets() {
                    hash ^= u64::from(byte);
                    hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
                }
            }
            IpAddress::Ipv6(address) => {
                hash ^= 6;
                for byte in address.octets() {
                    hash ^= u64::from(byte);
                    hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
                }
            }
        }
        for byte in hardware_address.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        let base_secs = Self::NEIGHBOR_REACHABLE_TIME.as_secs();
        TimeSpan::from_secs(base_secs / 2 + hash % base_secs)
    }

    fn unspecified_address(target: IpAddress) -> IpAddress {
        match target {
            IpAddress::Ipv4(_) => IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
            IpAddress::Ipv6(_) => IpAddress::Ipv6(Ipv6Address::UNSPECIFIED),
        }
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;
    use crate::buf::PacketOwner;

    #[def_test]
    fn incomplete_control_neighbor_requires_arp_request() {
        let mut neighbor = NeighborEntry::from_control_state(NeighborState::Incomplete);
        let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(10));

        assert!(neighbor.needs_request(now));
        neighbor.record_request_attempt(now);

        assert!(!neighbor.needs_request(
            now + NeighborResolver::REQUEST_RETRY_INTERVAL - TimeSpan::from_nanos(1)
        ));
        assert!(neighbor.needs_request(now + NeighborResolver::REQUEST_RETRY_INTERVAL));
        assert_eq!(neighbor.transmit_hardware_address(now), None);
    }

    #[def_test]
    fn expired_dynamic_neighbor_keeps_mac_during_delay() {
        let expires_at = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(10));
        let hardware_address = MacAddress([1, 2, 3, 4, 5, 6]);
        let mut neighbor = NeighborEntry::Reachable {
            hardware_address,
            expires_at,
            updated_at: MonotonicInstant::ORIGIN,
        };

        assert!(!neighbor.needs_request(expires_at));
        assert_eq!(
            neighbor.transmit_hardware_address(expires_at),
            Some(hardware_address)
        );
        assert!(matches!(neighbor, NeighborEntry::Delay { .. }));

        assert!(!neighbor.needs_request(
            expires_at + NeighborResolver::DELAY_PROBE_TIME - TimeSpan::from_nanos(1)
        ));
        assert_eq!(
            neighbor.next_poll_at(),
            Some(expires_at + NeighborResolver::DELAY_PROBE_TIME)
        );
    }

    #[def_test]
    fn arp_request_attempts_reach_failed_state() {
        let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(10));
        let target = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 1));
        let source_addr = Ipv4Address::new(192, 0, 2, 99);
        let mut resolver = NeighborResolver::new();
        resolver.cache.entries.insert(
            target,
            NeighborEntry::Incomplete {
                next_request_at: now,
                probes_sent: 0,
            },
        );
        resolver.pending_tx = VecDeque::from([pending_packet(target, source_addr, now)]);

        for attempt in 0..NeighborResolver::MAX_PROBES {
            let attempt_at = now + TimeSpan::from_secs(u64::from(attempt));
            assert!(resolver.cache.needs_request(target, attempt_at));
            resolver.cache.record_request_attempt(target, attempt_at);
        }

        let failed_at = now + TimeSpan::from_secs(u64::from(NeighborResolver::MAX_PROBES));
        // Drive the transition through maintain()/poll, not by hand-writing Failed.
        let actions = resolver.poll(failed_at, None);
        assert!(
            actions
                .iter()
                .all(|action| !matches!(action, NeighborAction::Request(_))),
            "exhausted probes must not emit another ARP request"
        );
        assert!(resolver.cache.is_failed(target));
        assert!(resolver.pending_tx.is_empty());
        assert_eq!(
            resolver.cache.next_poll_at_for(target),
            Some(failed_at + NeighborResolver::NEIGHBOR_GC_TIME)
        );
        // Re-polling at the same time must not re-emit Failed / clear again.
        assert!(resolver.poll(failed_at, None).is_empty());
        assert!(resolver.cache.is_failed(target));
    }

    #[def_test]
    fn dynamic_arp_does_not_replace_permanent_neighbor() {
        let permanent_addr = MacAddress([1, 2, 3, 4, 5, 6]);
        let mut neighbor = NeighborEntry::from_control_state(NeighborState::Permanent {
            hardware_addr: permanent_addr.0,
        });

        neighbor.learn_dynamic(
            MacAddress([6, 5, 4, 3, 2, 1]),
            IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 1)),
            MonotonicInstant::ORIGIN,
            NeighborObservation::Reachable,
        );

        assert_eq!(
            neighbor.transmit_hardware_address(MonotonicInstant::ORIGIN),
            Some(permanent_addr)
        );
    }

    #[def_test]
    fn non_host_directed_arp_learning_is_stale_and_uses_old_mac() {
        let hardware_address = MacAddress([1, 2, 3, 4, 5, 6]);
        let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(10));
        let protocol_address = Ipv4Address::new(192, 0, 2, 1);
        let mut neighbor = NeighborEntry::new_dynamic(
            hardware_address,
            IpAddress::Ipv4(protocol_address),
            now,
            NeighborObservation::Stale,
        );

        assert!(!neighbor.needs_request(now));
        assert_eq!(
            neighbor.transmit_hardware_address(now),
            Some(hardware_address)
        );
        assert!(matches!(neighbor, NeighborEntry::Delay { .. }));
    }

    #[def_test]
    fn dynamic_mac_update_obeys_locktime() {
        let old_hardware_address = MacAddress([1, 2, 3, 4, 5, 6]);
        let new_hardware_address = MacAddress([6, 5, 4, 3, 2, 1]);
        let updated_at = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(10));
        let protocol_address = Ipv4Address::new(192, 0, 2, 1);
        let mut neighbor = NeighborEntry::new_dynamic(
            old_hardware_address,
            IpAddress::Ipv4(protocol_address),
            updated_at,
            NeighborObservation::Reachable,
        );

        neighbor.learn_dynamic(
            new_hardware_address,
            IpAddress::Ipv4(protocol_address),
            updated_at + NeighborResolver::LOCK_TIME - TimeSpan::from_nanos(1),
            NeighborObservation::Reachable,
        );
        assert_eq!(
            neighbor.transmit_hardware_address(updated_at),
            Some(old_hardware_address)
        );

        neighbor.learn_dynamic(
            new_hardware_address,
            IpAddress::Ipv4(protocol_address),
            updated_at + NeighborResolver::LOCK_TIME,
            NeighborObservation::Reachable,
        );
        assert_eq!(
            neighbor.transmit_hardware_address(updated_at + NeighborResolver::LOCK_TIME),
            Some(new_hardware_address)
        );
    }

    #[def_test]
    fn dynamic_reachable_time_stays_within_jitter_range() {
        let protocol_address = Ipv4Address::new(192, 0, 2, 1);
        let reachable_time = NeighborResolver::dynamic_reachable_time(
            MacAddress([1, 2, 3, 4, 5, 6]),
            IpAddress::Ipv4(protocol_address),
        );
        let base = NeighborResolver::NEIGHBOR_REACHABLE_TIME.as_secs();

        assert!(reachable_time >= TimeSpan::from_secs(base / 2));
        assert!(reachable_time < TimeSpan::from_secs(base + base / 2));
    }

    #[def_test]
    fn pending_packet_preserves_ipv4_source() {
        let target = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 1));
        let source_addr = Ipv4Address::new(192, 0, 2, 99);
        let pending = pending_packet(target, source_addr, MonotonicInstant::ORIGIN);

        assert_eq!(pending.next_hop, target);
        assert_eq!(pending.source_addr, IpAddress::Ipv4(source_addr));
    }

    #[def_test]
    fn pending_deadline_uses_earliest_arp_or_tx_retry() {
        let target = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 1));
        let source_addr = Ipv4Address::new(192, 0, 2, 99);
        let mut resolver = NeighborResolver::new();
        resolver.pending_tx = VecDeque::from([pending_packet(
            target,
            source_addr,
            MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(30)),
        )]);
        resolver.cache.entries.insert(
            target,
            NeighborEntry::Incomplete {
                next_request_at: MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(20)),
                probes_sent: 1,
            },
        );

        assert_eq!(
            resolver.earliest_pending_poll_at(),
            Some(MonotonicInstant::from_span_since_origin(
                TimeSpan::from_secs(20)
            ))
        );

        resolver.pending_tx.front_mut().unwrap().next_attempt_at =
            MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(10));
        assert_eq!(
            resolver.earliest_pending_poll_at(),
            Some(MonotonicInstant::from_span_since_origin(
                TimeSpan::from_secs(10)
            ))
        );
    }

    #[def_test]
    fn pending_packet_survives_before_tx_retry_deadline() {
        let target = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 1));
        let source_addr = Ipv4Address::new(192, 0, 2, 99);
        let mut resolver = NeighborResolver::new();
        resolver.pending_tx = VecDeque::from([pending_packet(
            target,
            source_addr,
            MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(20)),
        )]);

        assert!(
            resolver
                .poll(
                    MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(10)),
                    None
                )
                .is_empty()
        );
        assert_eq!(resolver.pending_tx.len(), 1);
        assert_eq!(
            resolver.pending_tx.front().unwrap().next_attempt_at,
            MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(20))
        );
    }

    #[def_test]
    fn removing_ipv4_source_filters_only_matching_pending_packets() {
        let removed_source = Ipv4Address::new(192, 0, 2, 10);
        let retained_source = Ipv4Address::new(192, 0, 2, 20);
        let retained_target = IpAddress::Ipv4(Ipv4Address::new(198, 51, 100, 2));
        let mut resolver = NeighborResolver::new();
        resolver.pending_tx = VecDeque::from([
            pending_packet(
                IpAddress::Ipv4(Ipv4Address::new(198, 51, 100, 1)),
                removed_source,
                MonotonicInstant::ORIGIN,
            ),
            pending_packet(retained_target, retained_source, MonotonicInstant::ORIGIN),
            pending_packet(
                IpAddress::Ipv4(Ipv4Address::new(198, 51, 100, 3)),
                removed_source,
                MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(10)),
            ),
        ]);

        resolver.remove_pending_source(IpAddress::Ipv4(removed_source));

        assert_eq!(resolver.pending_tx.len(), 1);
        let retained = resolver.pending_tx.front().unwrap();
        assert_eq!(retained.next_hop, retained_target);
        assert_eq!(retained.source_addr, IpAddress::Ipv4(retained_source));
    }

    #[def_test]
    fn permanent_and_dynamic_neighbors_expose_expected_deadlines() {
        let permanent = NeighborEntry::Permanent {
            hardware_address: MacAddress([1, 2, 3, 4, 5, 6]),
        };
        let dynamic_expiry = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(30));
        let dynamic = NeighborEntry::Reachable {
            hardware_address: MacAddress([6, 5, 4, 3, 2, 1]),
            expires_at: dynamic_expiry,
            updated_at: MonotonicInstant::ORIGIN,
        };
        let incomplete_deadline = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(20));
        let incomplete = NeighborEntry::Incomplete {
            next_request_at: incomplete_deadline,
            probes_sent: 1,
        };

        assert_eq!(permanent.next_poll_at(), None);
        assert_eq!(dynamic.next_poll_at(), Some(dynamic_expiry));
        assert_eq!(incomplete.next_poll_at(), Some(incomplete_deadline));
    }

    fn pending_packet(
        next_hop: IpAddress,
        source_addr: Ipv4Address,
        next_attempt_at: MonotonicInstant,
    ) -> PendingTxPacket {
        PendingTxPacket {
            next_hop,
            source_addr: IpAddress::Ipv4(source_addr),
            next_attempt_at,
            is_broadcast: false,
            packet: PacketBuf::from_ip_packet_vec(1, alloc::vec![0x45], PacketOwner::DeviceTx),
        }
    }
}
