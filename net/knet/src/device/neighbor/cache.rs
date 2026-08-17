// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! NUD cache and timer driven by Linux 6.8 `neighbour.c`.

use alloc::{collections::VecDeque, vec::Vec};

use hashbrown::{HashMap, HashSet};
use ktime_types::{MonotonicInstant, TimeSpan};

use super::{
    LladdrPolicy, NeighborLearn, NeighborSnapshot, NeighborTimerWork, ReachabilityHint,
    ResolveResult, SolicitKind,
};
use crate::{
    buf::PacketBuf,
    consts::ETHERNET_MAX_PENDING_PACKETS,
    ip::{IpAddress, Ipv4Address},
    wire::MacAddress,
};

/// RFC 4861 RetransTimer / Linux `ND_RETRANS_TIMER` (`HZ`, 1 s).
const RETRANS_TIME: TimeSpan = TimeSpan::from_secs(1);
/// RFC 4861 BaseReachableTime / Linux `ND_REACHABLE_TIME` (30 s).
const BASE_REACHABLE_TIME: TimeSpan = TimeSpan::from_secs(30);
/// RFC 4861 DelayFirstProbeTime / Linux `DELAY_PROBE_TIME` (5 s).
const DELAY_PROBE_TIME: TimeSpan = TimeSpan::from_secs(5);
/// Linux `GC_STALETIME` for unused FAILED/STALE entries.
const GC_STALE_TIME: TimeSpan = TimeSpan::from_secs(60);
/// Linux `mcast_solicit` / `NEIGH_VAR_MCAST_PROBES`.
const MCAST_PROBES: u8 = 3;
/// Linux `ucast_solicit` / `NEIGH_VAR_UCAST_PROBES`.
const UCAST_PROBES: u8 = 3;

/// NUD states from Linux `include/uapi/linux/neighbour.h`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NudState {
    Incomplete,
    Reachable,
    Stale,
    Delay,
    Probe,
    Failed,
    Permanent,
}

impl NudState {
    const fn as_uapi(self) -> u16 {
        match self {
            Self::Incomplete => 0x01,
            Self::Reachable => 0x02,
            Self::Stale => 0x04,
            Self::Delay => 0x08,
            Self::Probe => 0x10,
            Self::Failed => 0x20,
            Self::Permanent => 0x80,
        }
    }

    const fn has_usable_mac(self) -> bool {
        matches!(
            self,
            Self::Reachable | Self::Stale | Self::Delay | Self::Probe | Self::Permanent
        )
    }

    const fn is_in_timer(self) -> bool {
        matches!(
            self,
            Self::Incomplete | Self::Reachable | Self::Delay | Self::Probe
        )
    }
}

enum TimerOutcome {
    Idle,
    Solicit(SolicitKind),
    Failed { dropped: usize },
}

struct Neighbor {
    state: NudState,
    hardware_address: Option<MacAddress>,
    confirmed_at: MonotonicInstant,
    used_at: MonotonicInstant,
    next_timer: Option<MonotonicInstant>,
    reachable_time: TimeSpan,
    probes: u8,
    pending: VecDeque<PacketBuf>,
}

impl Neighbor {
    fn permanent(mac: MacAddress, now: MonotonicInstant) -> Self {
        Self {
            state: NudState::Permanent,
            hardware_address: Some(mac),
            confirmed_at: now,
            used_at: now,
            next_timer: None,
            reachable_time: BASE_REACHABLE_TIME,
            probes: 0,
            pending: VecDeque::new(),
        }
    }

    fn has_usable_mac(&self) -> bool {
        self.hardware_address.is_some() && self.state.has_usable_mac()
    }

    fn enter_reachable(&mut self, now: MonotonicInstant) {
        self.state = NudState::Reachable;
        self.confirmed_at = now;
        self.probes = 0;
        self.next_timer = Some(now + self.reachable_time);
    }

    fn enter_delay(&mut self, now: MonotonicInstant) {
        self.state = NudState::Delay;
        self.probes = 0;
        self.next_timer = Some(now + DELAY_PROBE_TIME);
    }

    fn enter_stale(&mut self) {
        self.state = NudState::Stale;
        self.next_timer = None;
        self.probes = 0;
    }

    fn enter_probe(&mut self, now: MonotonicInstant) -> Option<SolicitKind> {
        let mac = self.hardware_address?;
        self.state = NudState::Probe;
        self.probes = 1;
        self.next_timer = Some(now + RETRANS_TIME);
        Some(SolicitKind::Unicast(mac))
    }

    fn fail(&mut self) {
        self.state = NudState::Failed;
        self.next_timer = None;
    }

    fn retransmit_or_fail(
        &mut self,
        now: MonotonicInstant,
        max_probes: u8,
        solicit: SolicitKind,
    ) -> TimerOutcome {
        if self.probes >= max_probes {
            let dropped = self.pending.len();
            self.fail();
            self.pending.clear();
            return TimerOutcome::Failed { dropped };
        }
        self.probes = self.probes.saturating_add(1);
        self.next_timer = Some(now + RETRANS_TIME);
        TimerOutcome::Solicit(solicit)
    }

    fn on_timer(&mut self, now: MonotonicInstant) -> TimerOutcome {
        if !self.state.is_in_timer() {
            self.next_timer = None;
            return TimerOutcome::Idle;
        }

        match self.state {
            NudState::Reachable => {
                if now.saturating_duration_since(self.confirmed_at) < self.reachable_time {
                    self.next_timer = Some(self.confirmed_at + self.reachable_time);
                    TimerOutcome::Idle
                } else if now.saturating_duration_since(self.used_at) < DELAY_PROBE_TIME {
                    self.enter_delay(now);
                    TimerOutcome::Idle
                } else {
                    self.enter_stale();
                    TimerOutcome::Idle
                }
            }
            NudState::Delay => {
                // Linux DELAY uses `confirmed` for upper-layer hints. Without
                // those hints this comparison expires into PROBE after 5 s.
                if now.saturating_duration_since(self.confirmed_at) < DELAY_PROBE_TIME {
                    self.enter_reachable(now);
                    TimerOutcome::Idle
                } else {
                    self.enter_probe(now)
                        .map(TimerOutcome::Solicit)
                        .unwrap_or(TimerOutcome::Idle)
                }
            }
            NudState::Incomplete => {
                self.retransmit_or_fail(now, MCAST_PROBES, SolicitKind::Multicast)
            }
            NudState::Probe => {
                let solicit = self
                    .hardware_address
                    .map(SolicitKind::Unicast)
                    .unwrap_or(SolicitKind::Multicast);
                self.retransmit_or_fail(now, UCAST_PROBES, solicit)
            }
            NudState::Stale | NudState::Failed | NudState::Permanent => {
                self.next_timer = None;
                TimerOutcome::Idle
            }
        }
    }

    fn apply_learn(&mut self, learn: NeighborLearn, now: MonotonicInstant) -> bool {
        let wanted = match learn.reachability {
            ReachabilityHint::Confirmed => NudState::Reachable,
            ReachabilityHint::Unconfirmed => NudState::Stale,
        };

        let mac_unchanged = match (learn.mac, self.hardware_address) {
            (Some(mac), Some(old)) => mac == old,
            (None, Some(_)) => true,
            (Some(_), None) => false,
            (None, None) => return false,
        };

        if let Some(mac) = learn.mac {
            if !mac_unchanged && learn.lladdr == LladdrPolicy::Weak {
                if self.state == NudState::Reachable {
                    self.enter_stale();
                }
                return false;
            }
            self.hardware_address = Some(mac);
        }

        // Same MAC advertised as STALE does not demote a stronger state.
        // Linux `__neigh_update`: `lladdr == ha && new == STALE` keeps `old`.
        let next_state = if wanted == NudState::Stale
            && mac_unchanged
            && self.has_usable_mac()
            && self.state != NudState::Incomplete
        {
            self.state
        } else {
            wanted
        };

        match next_state {
            NudState::Reachable => self.enter_reachable(now),
            NudState::Stale => self.enter_stale(),
            _ => {}
        }
        true
    }
}

/// Per-device neighbor cache shared by ARP and NDP.
pub(crate) struct NeighborCache {
    entries: HashMap<IpAddress, Neighbor>,
    pending_len: usize,
}

impl NeighborCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            pending_len: 0,
        }
    }

    pub(crate) fn insert_permanent(
        &mut self,
        addr: IpAddress,
        mac: MacAddress,
        now: MonotonicInstant,
    ) {
        self.remove_pending_of(addr);
        self.entries.insert(addr, Neighbor::permanent(mac, now));
    }

    /// Replaces netlink `NUD_PERMANENT` entries without wiping dynamic NUD state.
    pub(crate) fn sync_permanent(
        &mut self,
        neighbors: &[(IpAddress, MacAddress)],
        now: MonotonicInstant,
    ) {
        let keep: HashSet<IpAddress> = neighbors.iter().map(|(addr, _)| *addr).collect();
        let stale: Vec<IpAddress> = self
            .entries
            .iter()
            .filter(|(addr, entry)| entry.state == NudState::Permanent && !keep.contains(*addr))
            .map(|(addr, _)| *addr)
            .collect();
        for addr in stale {
            self.remove_pending_of(addr);
            self.entries.remove(&addr);
        }
        for (addr, mac) in neighbors {
            self.insert_permanent(*addr, *mac, now);
        }
    }

    pub(crate) fn snapshots(&self, ifindex: i32) -> Vec<NeighborSnapshot> {
        self.entries
            .iter()
            .map(|(addr, entry)| NeighborSnapshot {
                ifindex,
                addr: *addr,
                mac: entry.hardware_address,
                nud_state: entry.state.as_uapi(),
            })
            .collect()
    }

    /// Looks up a next hop and either returns a MAC or queues the packet.
    ///
    /// Matches Linux `neigh_event_send` + `__neigh_event_send`: CONNECTED,
    /// DELAY, and PROBE transmit immediately; STALE becomes DELAY then
    /// transmits; missing/FAILED start INCOMPLETE and hold the packet.
    pub(crate) fn resolve(
        &mut self,
        addr: IpAddress,
        packet: PacketBuf,
        now: MonotonicInstant,
    ) -> ResolveResult {
        if let Some(entry) = self.entries.get_mut(&addr) {
            entry.used_at = now;
            match entry.state {
                NudState::Permanent | NudState::Reachable | NudState::Delay | NudState::Probe => {
                    if let Some(mac) = entry.hardware_address {
                        return ResolveResult::Transmit { mac, packet };
                    }
                }
                NudState::Stale => {
                    if let Some(mac) = entry.hardware_address {
                        entry.enter_delay(now);
                        return ResolveResult::Transmit { mac, packet };
                    }
                }
                NudState::Incomplete => {
                    return self.queue_pending(addr, packet);
                }
                NudState::Failed => {}
            }
        }

        self.start_incomplete(addr, packet, now)
    }

    /// Applies an inbound mapping. Returns queued packets once the entry
    /// becomes usable, matching Linux `__neigh_update` flushing `arp_queue`.
    pub(crate) fn learn(
        &mut self,
        addr: IpAddress,
        learn: NeighborLearn,
        now: MonotonicInstant,
    ) -> Vec<PacketBuf> {
        if let Some(entry) = self.entries.get_mut(&addr) {
            if entry.state == NudState::Permanent {
                return Vec::new();
            }

            let old_was_valid = entry.has_usable_mac();
            if !entry.apply_learn(learn, now) {
                return Vec::new();
            }
            if entry.has_usable_mac() && !old_was_valid {
                return self.take_pending(addr);
            }
            return Vec::new();
        }

        let Some(mac) = learn.mac else {
            return Vec::new();
        };
        let mut entry = Neighbor {
            state: NudState::Stale,
            hardware_address: Some(mac),
            confirmed_at: now,
            used_at: now,
            next_timer: None,
            reachable_time: randomized_reachable_time(now, addr),
            probes: 0,
            pending: VecDeque::new(),
        };
        match learn.reachability {
            ReachabilityHint::Confirmed => entry.enter_reachable(now),
            ReachabilityHint::Unconfirmed => entry.enter_stale(),
        }
        self.entries.insert(addr, entry);
        Vec::new()
    }

    pub(crate) fn run_due_timers(&mut self, now: MonotonicInstant) -> Vec<NeighborTimerWork> {
        let due: Vec<IpAddress> = self
            .entries
            .iter()
            .filter_map(|(addr, entry)| {
                entry
                    .next_timer
                    .filter(|deadline| *deadline <= now)
                    .map(|_| *addr)
            })
            .collect();

        let mut work = Vec::new();
        for addr in due {
            if let Some(solicit) = self.handle_timer(addr, now) {
                work.push(NeighborTimerWork { addr, solicit });
            }
        }
        self.gc_unused(now);
        work
    }

    pub(crate) fn mac(&self, addr: IpAddress) -> Option<MacAddress> {
        self.entries
            .get(&addr)
            .and_then(|entry| entry.hardware_address)
    }

    pub(crate) fn next_deadline(&self) -> Option<MonotonicInstant> {
        self.entries
            .values()
            .filter_map(|entry| entry.next_timer)
            .min()
    }

    fn start_incomplete(
        &mut self,
        addr: IpAddress,
        packet: PacketBuf,
        now: MonotonicInstant,
    ) -> ResolveResult {
        self.remove_pending_of(addr);
        if self.pending_len >= ETHERNET_MAX_PENDING_PACKETS {
            return ResolveResult::Dropped;
        }

        let mut pending = VecDeque::new();
        pending.push_back(packet);
        self.pending_len += 1;
        self.entries.insert(
            addr,
            Neighbor {
                state: NudState::Incomplete,
                hardware_address: None,
                confirmed_at: now,
                used_at: now,
                next_timer: Some(now + RETRANS_TIME),
                reachable_time: randomized_reachable_time(now, addr),
                probes: 1,
                pending,
            },
        );
        ResolveResult::Queued(Some(SolicitKind::Multicast))
    }

    fn queue_pending(&mut self, addr: IpAddress, packet: PacketBuf) -> ResolveResult {
        if self.pending_len >= ETHERNET_MAX_PENDING_PACKETS {
            return ResolveResult::Dropped;
        }
        if let Some(entry) = self.entries.get_mut(&addr) {
            entry.pending.push_back(packet);
            self.pending_len += 1;
        }
        ResolveResult::Queued(None)
    }

    fn take_pending(&mut self, addr: IpAddress) -> Vec<PacketBuf> {
        let Some(entry) = self.entries.get_mut(&addr) else {
            return Vec::new();
        };
        let drained: Vec<PacketBuf> = entry.pending.drain(..).collect();
        self.pending_len = self.pending_len.saturating_sub(drained.len());
        drained
    }

    fn remove_pending_of(&mut self, addr: IpAddress) {
        if let Some(entry) = self.entries.get_mut(&addr) {
            self.pending_len = self.pending_len.saturating_sub(entry.pending.len());
            entry.pending.clear();
        }
    }

    pub(crate) fn remove_pending_ipv4_source(&mut self, addr: crate::ip::Ipv4Address) {
        for entry in self.entries.values_mut() {
            let before = entry.pending.len();
            entry.pending.retain(|packet| {
                ipv4_packet_source(packet).is_none_or(|source| source != addr)
            });
            self.pending_len = self.pending_len.saturating_sub(before - entry.pending.len());
        }
    }

    fn handle_timer(&mut self, addr: IpAddress, now: MonotonicInstant) -> Option<SolicitKind> {
        let entry = self.entries.get_mut(&addr)?;
        match entry.on_timer(now) {
            TimerOutcome::Solicit(kind) => Some(kind),
            TimerOutcome::Failed { dropped } => {
                self.pending_len = self.pending_len.saturating_sub(dropped);
                None
            }
            TimerOutcome::Idle => None,
        }
    }

    fn gc_unused(&mut self, now: MonotonicInstant) {
        self.entries.retain(|_, entry| {
            if !matches!(entry.state, NudState::Failed | NudState::Stale) {
                return true;
            }
            now.saturating_duration_since(entry.used_at) < GC_STALE_TIME
        });
    }
}

fn ipv4_packet_source(packet: &PacketBuf) -> Option<Ipv4Address> {
    let bytes = packet.network_packet()?;
    if bytes.len() < 20 || bytes[0] >> 4 != 4 {
        return None;
    }
    Some(Ipv4Address::from_octets(bytes[12..16].try_into().ok()?))
}

/// Linux `neigh_rand_reach_time`: uniform in `[base/2, 3base/2)`.
fn randomized_reachable_time(now: MonotonicInstant, addr: IpAddress) -> TimeSpan {
    let base_ms = BASE_REACHABLE_TIME.as_millis() as u64;
    let mix = now
        .as_nanos_u64_saturating()
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ addr_fingerprint(addr);
    TimeSpan::from_millis(base_ms / 2 + mix % base_ms)
}

fn addr_fingerprint(addr: IpAddress) -> u64 {
    match addr {
        IpAddress::Ipv4(addr) => u32::from_be_bytes(addr.octets()) as u64,
        IpAddress::Ipv6(addr) => {
            let octets = addr.octets();
            let mut hi = [0u8; 8];
            let mut lo = [0u8; 8];
            hi.copy_from_slice(&octets[..8]);
            lo.copy_from_slice(&octets[8..]);
            u64::from_be_bytes(hi) ^ u64::from_be_bytes(lo)
        }
    }
}

fn ipv4_packet_source(packet: &PacketBuf) -> Option<Ipv4Address> {
    let bytes = packet.network_packet()?;
    if bytes.len() < 20 || bytes[0] >> 4 != 4 {
        return None;
    }
    Some(Ipv4Address::from_octets(bytes[12..16].try_into().ok()?))
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::{buf::PacketOwner, ip::Ipv6Address};

    fn now_at(secs: u64) -> MonotonicInstant {
        MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(secs))
    }

    fn sample_addr() -> IpAddress {
        IpAddress::Ipv6(Ipv6Address::link_local_from_mac([
            0x02, 0x00, 0x00, 0x00, 0x00, 0x02,
        ]))
    }

    fn sample_mac() -> MacAddress {
        MacAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x02])
    }

    fn packet() -> PacketBuf {
        PacketBuf::from_ip_packet_vec(1, alloc::vec![0u8; 40], PacketOwner::Ipv6Stack)
    }

    #[def_test]
    fn resolve_starts_incomplete_and_retries_until_failed() {
        let mut cache = NeighborCache::new();
        let addr = sample_addr();
        let t0 = now_at(1);

        match cache.resolve(addr, packet(), t0) {
            ResolveResult::Queued(Some(SolicitKind::Multicast)) => {}
            _ => panic!("expected first multicast NS"),
        }
        assert_eq!(cache.entries[&addr].state, NudState::Incomplete);
        assert_eq!(cache.entries[&addr].probes, 1);

        match cache.resolve(addr, packet(), t0) {
            ResolveResult::Queued(None) => {}
            _ => panic!("later packets should queue without another NS"),
        }

        let t1 = t0 + RETRANS_TIME;
        let work = cache.run_due_timers(t1);
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].solicit, SolicitKind::Multicast);
        assert_eq!(cache.entries[&addr].probes, 2);

        let t2 = t1 + RETRANS_TIME;
        assert_eq!(cache.run_due_timers(t2).len(), 1);
        assert_eq!(cache.entries[&addr].probes, 3);

        let t3 = t2 + RETRANS_TIME;
        assert!(cache.run_due_timers(t3).is_empty());
        assert_eq!(cache.entries[&addr].state, NudState::Failed);
        assert!(cache.entries[&addr].pending.is_empty());
    }

    #[def_test]
    fn solicited_na_flushes_pending_and_marks_reachable() {
        let mut cache = NeighborCache::new();
        let addr = sample_addr();
        let mac = sample_mac();
        let t0 = now_at(1);
        let _ = cache.resolve(addr, packet(), t0);

        let flushed = cache.learn(addr, NeighborLearn::confirmed_override(mac), t0);
        assert_eq!(flushed.len(), 1);
        assert_eq!(cache.entries[&addr].state, NudState::Reachable);
        assert_eq!(cache.entries[&addr].hardware_address, Some(mac));

        match cache.resolve(addr, packet(), t0) {
            ResolveResult::Transmit { mac: got, .. } => assert_eq!(got, mac),
            _ => panic!("reachable neighbor should transmit"),
        }
    }

    #[def_test]
    fn stale_send_enters_delay_then_probe_unicast() {
        let mut cache = NeighborCache::new();
        let addr = sample_addr();
        let mac = sample_mac();
        let t0 = now_at(1);
        let _ = cache.learn(addr, NeighborLearn::unconfirmed_override(mac), t0);
        assert_eq!(cache.entries[&addr].state, NudState::Stale);

        match cache.resolve(addr, packet(), t0) {
            ResolveResult::Transmit { mac: got, .. } => assert_eq!(got, mac),
            _ => panic!("stale neighbor still has a usable MAC"),
        }
        assert_eq!(cache.entries[&addr].state, NudState::Delay);

        let work = cache.run_due_timers(t0 + DELAY_PROBE_TIME);
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].solicit, SolicitKind::Unicast(mac));
        assert_eq!(cache.entries[&addr].state, NudState::Probe);
    }

    #[def_test]
    fn delay_solicited_na_returns_to_reachable() {
        let mut cache = NeighborCache::new();
        let addr = sample_addr();
        let mac = sample_mac();
        let t0 = now_at(1);
        let _ = cache.learn(addr, NeighborLearn::unconfirmed_override(mac), t0);
        let _ = cache.resolve(addr, packet(), t0);
        assert_eq!(cache.entries[&addr].state, NudState::Delay);

        let _ = cache.learn(
            addr,
            NeighborLearn::confirmed_override(mac),
            t0 + TimeSpan::from_secs(1),
        );
        assert_eq!(cache.entries[&addr].state, NudState::Reachable);
    }

    #[def_test]
    fn reachable_expires_to_stale_when_idle() {
        let mut cache = NeighborCache::new();
        let addr = sample_addr();
        let mac = sample_mac();
        let t0 = now_at(10);
        let _ = cache.learn(addr, NeighborLearn::confirmed_override(mac), t0);
        let reachable_time = cache.entries[&addr].reachable_time;
        cache.entries.get_mut(&addr).unwrap().used_at = t0 - DELAY_PROBE_TIME;

        let work = cache.run_due_timers(t0 + reachable_time);
        assert!(work.is_empty());
        assert_eq!(cache.entries[&addr].state, NudState::Stale);
    }

    #[def_test]
    fn permanent_entry_ignores_dynamic_updates() {
        let mut cache = NeighborCache::new();
        let addr = sample_addr();
        let mac = sample_mac();
        let other = MacAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x03]);
        let t0 = now_at(1);
        cache.insert_permanent(addr, mac, t0);
        let flushed = cache.learn(addr, NeighborLearn::confirmed_override(other), t0);
        assert!(flushed.is_empty());
        assert_eq!(cache.entries[&addr].state, NudState::Permanent);
        assert_eq!(cache.entries[&addr].hardware_address, Some(mac));
    }

    #[def_test]
    fn next_deadline_tracks_incomplete_retransmit() {
        let mut cache = NeighborCache::new();
        let addr = sample_addr();
        let t0 = now_at(5);
        let _ = cache.resolve(addr, packet(), t0);
        assert_eq!(cache.next_deadline(), Some(t0 + RETRANS_TIME));
    }

    #[def_test]
    fn sync_permanent_preserves_dynamic_entries() {
        let mut cache = NeighborCache::new();
        let dynamic = sample_addr();
        let static_addr = IpAddress::Ipv6(Ipv6Address::link_local_from_mac([
            0x02, 0x00, 0x00, 0x00, 0x00, 0x03,
        ]));
        let mac = sample_mac();
        let t0 = now_at(1);
        let _ = cache.learn(dynamic, NeighborLearn::confirmed_override(mac), t0);
        cache.sync_permanent(&[(static_addr, mac)], t0);
        assert_eq!(cache.entries[&dynamic].state, NudState::Reachable);
        assert_eq!(cache.entries[&static_addr].state, NudState::Permanent);
    }
}
