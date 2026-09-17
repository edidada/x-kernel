// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! UDP port binding and management.
//!
//! Manages UDP socket bindings, bind-conflict checks, ephemeral-port
//! allocation, binding removal, and receive-side socket lookup.

use alloc::{sync::Arc, vec::Vec};

use ::core::net::{IpAddr, SocketAddr};
use hashbrown::HashMap;
use kerrno::{KResult, k_bail};
use kspin::SpinNoIrq;
use lazyinit::LazyInit;

use super::{output::ipv4_to_core, pcb::UdpPcb, state::UdpSocketState};
use crate::{
    ip::{IpAddress, IpEndpoint, IpListenEndpoint},
    netns::NetNsId,
};

const UDP_REGISTRY_BUCKETS: usize = 256;
const UDP_EPHEMERAL_PORT_START: u16 = 0xc000;
const UDP_EPHEMERAL_PORT_END: u16 = 0xffff;
const UDP_EPHEMERAL_PORT_RANGE: u16 = UDP_EPHEMERAL_PORT_END - UDP_EPHEMERAL_PORT_START + 1;

static UDP_PCB_REGISTRY: LazyInit<UdpPcbRegistry> = LazyInit::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UdpBindKind {
    Auto,
    Explicit,
}

struct UdpBindRecord {
    /// Namespace of the owning socket. Delivery and bind-conflict checks only
    /// ever consider records of one namespace.
    net_ns: NetNsId,
    pcb: Arc<UdpPcb>,
    local_endpoint: IpEndpoint,
    kind: UdpBindKind,
    reuse_address: bool,
}

struct UdpPcbBucket {
    by_port: HashMap<u16, Vec<UdpBindRecord>>,
}

struct UdpPcbRegistry {
    buckets: Vec<SpinNoIrq<UdpPcbBucket>>,
}

impl UdpPcbRegistry {
    fn new() -> Self {
        let mut buckets = Vec::with_capacity(UDP_REGISTRY_BUCKETS);
        for _ in 0..UDP_REGISTRY_BUCKETS {
            buckets.push(SpinNoIrq::new(UdpPcbBucket::new()));
        }
        Self { buckets }
    }

    #[cfg(unittest)]
    fn register(&self, net_ns: NetNsId, pcb: Arc<UdpPcb>) {
        let Some(local_endpoint) = pcb.state.local_endpoint() else {
            return;
        };
        self.bucket(local_endpoint.port)
            .lock()
            .register(net_ns, pcb, local_endpoint);
    }

    fn unregister(&self, pcb: &Arc<UdpPcb>) {
        let Some(local_endpoint) = pcb.state.local_endpoint() else {
            return;
        };
        self.bucket(local_endpoint.port)
            .lock()
            .unregister(pcb, local_endpoint);
    }

    fn lookup_map<T>(
        &self,
        net_ns: NetNsId,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
        ifindex: i32,
        map_pcb_fn: impl Fn(&Arc<UdpPcb>) -> T,
    ) -> Option<T> {
        self.bucket(local_addr.port()).lock().lookup_in_bucket(
            net_ns,
            local_addr,
            remote_addr,
            ifindex,
            map_pcb_fn,
        )
    }

    fn lookup(
        &self,
        net_ns: NetNsId,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
        ifindex: i32,
    ) -> Option<Arc<UdpPcb>> {
        self.lookup_map(net_ns, local_addr, remote_addr, ifindex, |pcb| pcb.clone())
    }

    fn lookup_state(
        &self,
        net_ns: NetNsId,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
        ifindex: i32,
    ) -> Option<Arc<UdpSocketState>> {
        self.lookup_map(net_ns, local_addr, remote_addr, ifindex, |pcb| {
            pcb.state.clone()
        })
    }

    fn bind(
        &self,
        net_ns: NetNsId,
        pcb: Arc<UdpPcb>,
        endpoint: IpEndpoint,
        kind: UdpBindKind,
        reuse_address: bool,
    ) -> KResult {
        self.bucket(endpoint.port).lock().bind(
            net_ns,
            pcb.clone(),
            endpoint,
            kind,
            reuse_address,
        )?;
        // `set_local_endpoint` takes a sleepable lock; keep it outside the
        // IRQ-safe bucket used by `NetRx` lookup.
        pcb.state.set_local_endpoint(Some(endpoint));
        Ok(())
    }

    fn bind_ephemeral(
        &self,
        net_ns: NetNsId,
        pcb: Arc<UdpPcb>,
        local_addr: IpAddress,
        kind: UdpBindKind,
        reuse_address: bool,
    ) -> KResult<IpEndpoint> {
        for port in self.ephemeral_port_iter()? {
            let endpoint = IpEndpoint {
                addr: local_addr,
                port,
            };
            let mut bucket = self.bucket(endpoint.port).lock();
            if !bucket.contains_bind_conflict(net_ns, listen_endpoint(endpoint), reuse_address) {
                bucket.bind(net_ns, pcb.clone(), endpoint, kind, reuse_address)?;
                drop(bucket);
                pcb.state.set_local_endpoint(Some(endpoint));
                return Ok(endpoint);
            }
        }

        k_bail!(AddrInUse, "no available ports");
    }

    #[cfg(unittest)]
    fn contains_bind_conflict(
        &self,
        net_ns: NetNsId,
        endpoint: IpListenEndpoint,
        reuse_address: bool,
    ) -> bool {
        self.bucket(endpoint.port)
            .lock()
            .contains_bind_conflict(net_ns, endpoint, reuse_address)
    }

    #[cfg(unittest)]
    fn clear(&self) {
        for bucket in &self.buckets {
            bucket.lock().clear();
        }
    }

    fn is_explicitly_bound(&self, pcb: &Arc<UdpPcb>) -> bool {
        let Some(local_endpoint) = pcb.state.local_endpoint() else {
            return false;
        };
        self.bucket(local_endpoint.port)
            .lock()
            .is_explicitly_bound(pcb, local_endpoint.port)
    }

    fn bucket(&self, port: u16) -> &SpinNoIrq<UdpPcbBucket> {
        &self.buckets[bucket_index_for_port(port)]
    }

    /// Requires sleepable task context; entropy sources may perform I/O.
    /// Returns `EAGAIN` before publishing a bind if quality entropy is unavailable.
    fn ephemeral_port_iter(&self) -> KResult<UdpEphemeralPortIter> {
        // Bootstrap output can be derived from ticks alone. Only use output
        // after the pool has credited a trusted entropy source.
        entropy::try_seed_from_hardware();
        if !entropy::is_ready() {
            return Err(kerrno::KError::WouldBlock);
        }
        let mut bytes = [0u8; 4];
        entropy::fill_random(&mut bytes);
        Ok(UdpEphemeralPortIter::new(u32::from_le_bytes(bytes)))
    }
}

impl UdpPcbBucket {
    fn new() -> Self {
        Self {
            by_port: HashMap::new(),
        }
    }

    #[cfg(unittest)]
    fn register(&mut self, net_ns: NetNsId, pcb: Arc<UdpPcb>, local_endpoint: IpEndpoint) {
        if self.contains(&pcb, local_endpoint.port) {
            return;
        };
        self.insert(net_ns, pcb, local_endpoint, UdpBindKind::Explicit, false);
    }

    fn unregister(&mut self, pcb: &Arc<UdpPcb>, local_endpoint: IpEndpoint) {
        let port = local_endpoint.port;
        let should_remove_port = if let Some(records) = self.by_port.get_mut(&port) {
            records.retain(|record| !Arc::ptr_eq(&record.pcb, pcb));
            records.is_empty()
        } else {
            false
        };
        if should_remove_port {
            self.by_port.remove(&port);
        }
    }

    fn bind(
        &mut self,
        net_ns: NetNsId,
        pcb: Arc<UdpPcb>,
        endpoint: IpEndpoint,
        kind: UdpBindKind,
        reuse_address: bool,
    ) -> KResult {
        if self.contains(&pcb, endpoint.port) {
            return Ok(());
        }

        let listen_endpoint = listen_endpoint(endpoint);
        if self.contains_bind_conflict(net_ns, listen_endpoint, reuse_address) {
            return Err(kerrno::KError::AddrInUse);
        }

        self.insert(net_ns, pcb, endpoint, kind, reuse_address);
        Ok(())
    }

    fn insert(
        &mut self,
        net_ns: NetNsId,
        pcb: Arc<UdpPcb>,
        local_endpoint: IpEndpoint,
        kind: UdpBindKind,
        reuse_address: bool,
    ) {
        self.by_port
            .entry(local_endpoint.port)
            .or_default()
            .push(UdpBindRecord {
                net_ns,
                pcb,
                local_endpoint,
                kind,
                reuse_address,
            });
    }

    fn contains(&self, pcb: &Arc<UdpPcb>, port: u16) -> bool {
        self.by_port
            .get(&port)
            .is_some_and(|records| records.iter().any(|record| Arc::ptr_eq(&record.pcb, pcb)))
    }

    fn is_explicitly_bound(&self, pcb: &Arc<UdpPcb>, port: u16) -> bool {
        self.by_port.get(&port).is_some_and(|records| {
            records
                .iter()
                .any(|record| Arc::ptr_eq(&record.pcb, pcb) && record.kind == UdpBindKind::Explicit)
        })
    }

    fn lookup_in_bucket<T>(
        &self,
        net_ns: NetNsId,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
        ifindex: i32,
        map_pcb_fn: impl Fn(&Arc<UdpPcb>) -> T,
    ) -> Option<T> {
        let records = self.by_port.get(&local_addr.port())?;

        let mut selected = None;
        let mut selected_score = 0;
        for record in records {
            if record.net_ns != net_ns {
                continue;
            }
            let local_endpoint = record.local_endpoint;
            if !local_endpoint_matches(local_endpoint, local_addr) {
                continue;
            }
            if !bound_device_matches(record.pcb.state.bound_dev_if(), ifindex) {
                continue;
            }

            let Some(score) = delivery_score(&record.pcb.state, local_endpoint, remote_addr) else {
                continue;
            };
            if score > selected_score {
                selected_score = score;
                selected = Some(map_pcb_fn(&record.pcb));
            }
        }
        selected
    }

    fn contains_bind_conflict(
        &self,
        net_ns: NetNsId,
        endpoint: IpListenEndpoint,
        reuse_address: bool,
    ) -> bool {
        let Some(records) = self.by_port.get(&endpoint.port) else {
            return false;
        };

        records
            .iter()
            .filter(|record| record.net_ns == net_ns)
            .any(|record| {
                let existing_endpoint = listen_endpoint(record.local_endpoint);
                udp_bind_entries_conflict(
                    existing_endpoint,
                    record.reuse_address,
                    endpoint,
                    reuse_address,
                )
            })
    }

    #[cfg(unittest)]
    fn clear(&mut self) {
        self.by_port.clear();
    }
}

struct UdpEphemeralPortIter {
    start: u16,
    step: u16,
    tries: u16,
}

impl UdpEphemeralPortIter {
    fn new(rand: u32) -> Self {
        Self {
            start: (rand as u16) & (UDP_EPHEMERAL_PORT_RANGE - 1),
            step: (((rand >> 16) as u16) & (UDP_EPHEMERAL_PORT_RANGE - 1)) | 1,
            tries: 0,
        }
    }
}

impl Iterator for UdpEphemeralPortIter {
    type Item = u16;

    fn next(&mut self) -> Option<Self::Item> {
        if self.tries >= UDP_EPHEMERAL_PORT_RANGE {
            return None;
        }

        let offset = self.start.wrapping_add(self.tries.wrapping_mul(self.step))
            & (UDP_EPHEMERAL_PORT_RANGE - 1);
        self.tries += 1;

        Some(UDP_EPHEMERAL_PORT_START + offset)
    }
}

pub(crate) fn init_udp_registry() {
    UDP_PCB_REGISTRY.call_once(UdpPcbRegistry::new);
}

#[cfg(unittest)]
pub(super) fn register_udp_pcb(net_ns: NetNsId, pcb: Arc<UdpPcb>) {
    init_udp_registry();
    UDP_PCB_REGISTRY.register(net_ns, pcb);
}

pub(super) fn bind_udp_pcb(
    net_ns: NetNsId,
    pcb: Arc<UdpPcb>,
    endpoint: IpEndpoint,
    reuse_address: bool,
) -> KResult {
    init_udp_registry();
    UDP_PCB_REGISTRY.bind(net_ns, pcb, endpoint, UdpBindKind::Explicit, reuse_address)
}

#[cfg(unittest)]
pub(super) fn bind_udp_auto_pcb_for_test(
    net_ns: NetNsId,
    pcb: Arc<UdpPcb>,
    endpoint: IpEndpoint,
    reuse_address: bool,
) -> KResult {
    init_udp_registry();
    UDP_PCB_REGISTRY.bind(net_ns, pcb, endpoint, UdpBindKind::Auto, reuse_address)
}

pub(super) fn bind_udp_auto_ephemeral_pcb(
    net_ns: NetNsId,
    pcb: Arc<UdpPcb>,
    local_addr: IpAddress,
    reuse_address: bool,
) -> KResult<IpEndpoint> {
    init_udp_registry();
    UDP_PCB_REGISTRY.bind_ephemeral(net_ns, pcb, local_addr, UdpBindKind::Auto, reuse_address)
}

pub(super) fn bind_udp_explicit_ephemeral_pcb(
    net_ns: NetNsId,
    pcb: Arc<UdpPcb>,
    local_addr: IpAddress,
    reuse_address: bool,
) -> KResult<IpEndpoint> {
    init_udp_registry();
    UDP_PCB_REGISTRY.bind_ephemeral(
        net_ns,
        pcb,
        local_addr,
        UdpBindKind::Explicit,
        reuse_address,
    )
}

pub(super) fn unregister_udp_pcb(pcb: &Arc<UdpPcb>) {
    if !UDP_PCB_REGISTRY.is_inited() {
        return;
    }
    UDP_PCB_REGISTRY.unregister(pcb);
}

pub(super) fn is_udp_pcb_explicitly_bound(pcb: &Arc<UdpPcb>) -> bool {
    if !UDP_PCB_REGISTRY.is_inited() {
        return false;
    }
    UDP_PCB_REGISTRY.is_explicitly_bound(pcb)
}

#[cfg(unittest)]
pub(crate) fn clear_udp_registry_for_test() {
    init_udp_registry();
    UDP_PCB_REGISTRY.clear();
}

#[cfg(unittest)]
pub(crate) fn register_udp_state_for_test(net_ns: NetNsId, state: Arc<UdpSocketState>) {
    register_udp_pcb(net_ns, UdpPcb::new(state));
}

#[cfg(unittest)]
pub(super) fn lookup_udp_pcb(
    net_ns: NetNsId,
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
) -> Option<Arc<UdpPcb>> {
    lookup_udp_pcb_on_device(net_ns, local_addr, remote_addr, 0)
}

pub(super) fn lookup_udp_pcb_on_device(
    net_ns: NetNsId,
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
    ifindex: i32,
) -> Option<Arc<UdpPcb>> {
    init_udp_registry();
    UDP_PCB_REGISTRY.lookup(net_ns, local_addr, remote_addr, ifindex)
}

pub(crate) fn lookup_udp_error_state(
    net_ns: NetNsId,
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
    ifindex: i32,
) -> Option<Arc<UdpSocketState>> {
    init_udp_registry();
    UDP_PCB_REGISTRY.lookup_state(net_ns, local_addr, remote_addr, ifindex)
}

fn bound_device_matches(bound_dev_if: i32, packet_ifindex: i32) -> bool {
    bound_dev_if == 0 || bound_dev_if == packet_ifindex
}

fn bucket_index_for_port(port: u16) -> usize {
    let port = port as usize;
    (port ^ (port >> 8)) & (UDP_REGISTRY_BUCKETS - 1)
}

fn delivery_score(
    state: &UdpSocketState,
    local_endpoint: IpEndpoint,
    remote_addr: SocketAddr,
) -> Option<u8> {
    let is_specific_local = !local_endpoint.addr.is_unspecified();
    match state.peer_endpoint() {
        Some((peer, _)) if endpoint_matches_addr(peer, remote_addr) => {
            Some(if is_specific_local { 4 } else { 3 })
        }
        Some(_) => None,
        None => Some(if is_specific_local { 2 } else { 1 }),
    }
}

fn local_endpoint_matches(endpoint: IpEndpoint, addr: SocketAddr) -> bool {
    endpoint.port == addr.port()
        && (endpoint.addr.is_unspecified() || endpoint_addr_matches(endpoint.addr, addr.ip()))
}

pub(super) fn endpoint_matches_addr(endpoint: IpEndpoint, addr: SocketAddr) -> bool {
    endpoint.port == addr.port() && endpoint_addr_matches(endpoint.addr, addr.ip())
}

fn endpoint_addr_matches(endpoint_addr: IpAddress, addr: IpAddr) -> bool {
    match (endpoint_addr, addr) {
        (IpAddress::Ipv4(endpoint), IpAddr::V4(addr)) => ipv4_to_core(endpoint) == addr,
        (IpAddress::Ipv6(endpoint), IpAddr::V6(addr)) => endpoint.octets() == addr.octets(),
        _ => false,
    }
}

pub(super) fn listen_endpoint(endpoint: IpEndpoint) -> IpListenEndpoint {
    IpListenEndpoint {
        addr: (!endpoint.addr.is_unspecified()).then_some(endpoint.addr),
        port: endpoint.port,
    }
}

#[cfg(unittest)]
pub(super) fn udp_port_available(net_ns: NetNsId, endpoint: IpListenEndpoint) -> bool {
    init_udp_registry();
    !UDP_PCB_REGISTRY.contains_bind_conflict(net_ns, endpoint, false)
}

fn udp_bind_entries_conflict(
    existing_endpoint: IpListenEndpoint,
    existing_reuse_address: bool,
    new_endpoint: IpListenEndpoint,
    new_reuse_address: bool,
) -> bool {
    let same_bind_scope = listen_addrs_overlap(existing_endpoint.addr, new_endpoint.addr);
    let both_allow_reuse = existing_reuse_address && new_reuse_address;

    same_bind_scope && !both_allow_reuse
}

fn listen_addrs_overlap(a: Option<IpAddress>, b: Option<IpAddress>) -> bool {
    match (a, b) {
        (None, _) | (_, None) => true,
        (Some(a), Some(b)) => a == b,
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::{sync::Arc, vec};

    use ::core::net::{SocketAddr, SocketAddrV4};
    use unittest::def_test;

    use super::*;
    use crate::ip::Ipv4Address;

    fn endpoint(addr: Ipv4Address, port: u16) -> IpEndpoint {
        SocketAddrV4::new(ipv4_to_core(addr), port).into()
    }

    fn socket_addr(addr: Ipv4Address, port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(ipv4_to_core(addr), port))
    }

    fn clear_registry() {
        clear_udp_registry_for_test();
    }

    fn test_ns() -> NetNsId {
        crate::netns::INIT_NET_ID
    }

    fn test_init_net() -> Arc<crate::netns::NetNamespace> {
        crate::netns::test_init_net()
    }

    fn new_pcb() -> Arc<UdpPcb> {
        UdpPcb::new(UdpSocketState::new(test_init_net()))
    }

    fn bind_test_pcb(local: IpEndpoint, reuse_address: bool) -> Arc<UdpPcb> {
        let pcb = new_pcb();
        bind_udp_pcb(test_ns(), pcb.clone(), local, reuse_address).unwrap();
        pcb
    }

    #[def_test]
    fn bucket_index_uses_high_port_bits() {
        let first = 40000;
        let second = first + UDP_REGISTRY_BUCKETS as u16;

        assert_ne!(bucket_index_for_port(first), bucket_index_for_port(second));
    }

    #[def_test]
    fn ephemeral_port_iter_visits_every_candidate_once() {
        let mut seen = vec![false; UDP_EPHEMERAL_PORT_RANGE as usize];
        let mut count = 0;

        for port in UdpEphemeralPortIter::new(0x1234_5679) {
            assert!((UDP_EPHEMERAL_PORT_START..=UDP_EPHEMERAL_PORT_END).contains(&port));
            let index = (port - UDP_EPHEMERAL_PORT_START) as usize;
            assert!(!seen[index]);
            seen[index] = true;
            count += 1;
        }

        assert_eq!(count, UDP_EPHEMERAL_PORT_RANGE as usize);
    }

    #[def_test(serial)]
    fn bind_registry_rejects_wildcard_conflict() {
        clear_registry();

        bind_test_pcb(endpoint(Ipv4Address::UNSPECIFIED, 8080), false);
        assert!(
            bind_udp_pcb(
                test_ns(),
                new_pcb(),
                endpoint(Ipv4Address::new(10, 0, 0, 2), 8080),
                false
            )
            .is_err()
        );

        clear_registry();
    }

    #[def_test(serial)]
    fn reuse_bind_is_recorded_for_later_nonreuse_conflict() {
        clear_registry();

        bind_test_pcb(endpoint(Ipv4Address::new(10, 0, 0, 2), 8081), true);
        assert!(
            bind_udp_pcb(
                test_ns(),
                new_pcb(),
                endpoint(Ipv4Address::new(10, 0, 0, 2), 8081),
                true
            )
            .is_ok()
        );
        assert!(
            bind_udp_pcb(
                test_ns(),
                new_pcb(),
                endpoint(Ipv4Address::new(10, 0, 0, 2), 8081),
                false
            )
            .is_err()
        );

        clear_registry();
    }

    #[def_test(serial)]
    fn reuse_bind_rejects_reuse_after_nonreuse() {
        clear_registry();

        bind_test_pcb(endpoint(Ipv4Address::new(10, 0, 0, 2), 8082), false);
        assert!(
            bind_udp_pcb(
                test_ns(),
                new_pcb(),
                endpoint(Ipv4Address::new(10, 0, 0, 2), 8082),
                true
            )
            .is_err()
        );

        clear_registry();
    }

    #[def_test(serial)]
    fn ephemeral_bind_assigns_distinct_registered_ports() {
        clear_registry();

        let first = bind_udp_auto_ephemeral_pcb(
            test_ns(),
            new_pcb(),
            IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
            false,
        )
        .unwrap();
        let second = bind_udp_auto_ephemeral_pcb(
            test_ns(),
            new_pcb(),
            IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
            false,
        )
        .unwrap();

        assert_ne!(first.port, second.port);

        clear_registry();
    }

    #[def_test(serial)]
    fn connected_pcb_takes_lookup_precedence() {
        clear_registry();

        let local = endpoint(Ipv4Address::new(10, 0, 0, 2), 8080);
        let remote = endpoint(Ipv4Address::new(192, 0, 2, 1), 5353);

        let bound_state = UdpSocketState::new(test_init_net());
        bound_state.set_local_endpoint(Some(local));
        let bound = UdpPcb::new(bound_state);
        register_udp_pcb(test_ns(), bound);

        let connected_state = UdpSocketState::new(test_init_net());
        connected_state.set_local_endpoint(Some(local));
        connected_state.set_peer_endpoint(Some((remote, local.addr)));
        let connected = UdpPcb::new(connected_state);
        register_udp_pcb(test_ns(), connected.clone());

        let selected = lookup_udp_pcb(
            test_ns(),
            socket_addr(Ipv4Address::new(10, 0, 0, 2), 8080),
            socket_addr(Ipv4Address::new(192, 0, 2, 1), 5353),
        )
        .expect("connected PCB should match");

        assert!(Arc::ptr_eq(&selected, &connected));
        clear_registry();
    }

    #[def_test(serial)]
    fn lookup_scans_only_matching_local_port_bucket() {
        clear_registry();

        let first_state = UdpSocketState::new(test_init_net());
        first_state.set_local_endpoint(Some(endpoint(Ipv4Address::new(10, 0, 0, 2), 8080)));
        let first = UdpPcb::new(first_state);
        register_udp_pcb(test_ns(), first);

        let second_state = UdpSocketState::new(test_init_net());
        second_state.set_local_endpoint(Some(endpoint(Ipv4Address::new(10, 0, 0, 2), 5353)));
        let second = UdpPcb::new(second_state);
        register_udp_pcb(test_ns(), second.clone());

        let selected = lookup_udp_pcb(
            test_ns(),
            socket_addr(Ipv4Address::new(10, 0, 0, 2), 5353),
            socket_addr(Ipv4Address::new(192, 0, 2, 1), 49152),
        )
        .expect("same-port UDP PCB should match");

        assert!(Arc::ptr_eq(&selected, &second));
        clear_registry();
    }

    #[def_test(serial)]
    fn lookup_scores_specific_and_wildcard_addrs() {
        clear_registry();

        let local = endpoint(Ipv4Address::new(10, 0, 0, 2), 8083);
        let wildcard = endpoint(Ipv4Address::UNSPECIFIED, 8083);
        let remote = endpoint(Ipv4Address::new(192, 0, 2, 1), 5353);

        let specific_state = UdpSocketState::new(test_init_net());
        specific_state.set_local_endpoint(Some(local));
        let specific = UdpPcb::new(specific_state);
        register_udp_pcb(test_ns(), specific);

        let wildcard_state = UdpSocketState::new(test_init_net());
        wildcard_state.set_local_endpoint(Some(wildcard));
        wildcard_state.set_peer_endpoint(Some((remote, wildcard.addr)));
        let wildcard_connected = UdpPcb::new(wildcard_state);
        register_udp_pcb(test_ns(), wildcard_connected.clone());

        let selected = lookup_udp_pcb(
            test_ns(),
            socket_addr(Ipv4Address::new(10, 0, 0, 2), 8083),
            socket_addr(Ipv4Address::new(192, 0, 2, 1), 5353),
        )
        .expect("wildcard connected PCB should match");

        assert!(Arc::ptr_eq(&selected, &wildcard_connected));
        clear_registry();
    }

    #[def_test(serial)]
    fn lookup_skips_pcb_bound_to_another_device() {
        clear_registry();

        let local = endpoint(Ipv4Address::new(10, 0, 0, 2), 8068);
        let bound_state = UdpSocketState::new(test_init_net());
        bound_state.set_local_endpoint(Some(local));
        bound_state.set_bound_dev_if_for_test(2);
        let bound = UdpPcb::new(bound_state);
        register_udp_pcb(test_ns(), bound.clone());

        assert!(
            lookup_udp_pcb_on_device(
                test_ns(),
                socket_addr(Ipv4Address::new(10, 0, 0, 2), 8068),
                socket_addr(Ipv4Address::new(192, 0, 2, 1), 67),
                1,
            )
            .is_none()
        );
        let selected = lookup_udp_pcb_on_device(
            test_ns(),
            socket_addr(Ipv4Address::new(10, 0, 0, 2), 8068),
            socket_addr(Ipv4Address::new(192, 0, 2, 1), 67),
            2,
        )
        .expect("bound PCB should match its device");
        assert!(Arc::ptr_eq(&selected, &bound));
        clear_registry();
    }

    #[def_test(serial)]
    fn unregister_removes_pcb_and_releases_port() {
        clear_registry();

        let local = endpoint(Ipv4Address::new(10, 0, 0, 2), 8084);
        let pcb = bind_test_pcb(local, false);

        unregister_udp_pcb(&pcb);

        assert!(
            lookup_udp_pcb(
                test_ns(),
                socket_addr(Ipv4Address::new(10, 0, 0, 2), 8084),
                socket_addr(Ipv4Address::new(192, 0, 2, 1), 49152)
            )
            .is_none()
        );
        assert!(udp_port_available(test_ns(), listen_endpoint(local)));
        clear_registry();
    }
    /// Serial: bind and delivery are scoped per namespace.
    #[def_test(serial)]
    fn udp_registry_is_isolated_between_namespaces() {
        clear_registry();
        let init_net = crate::netns::test_init_net();
        let fresh = crate::netns::fresh_unregistered();
        let local = endpoint(Ipv4Address::new(127, 0, 0, 1), 8085);

        // The same endpoint can be bound in both namespaces independently.
        let init_pcb = new_pcb();
        let fresh_pcb = new_pcb();
        bind_udp_pcb(init_net.id(), init_pcb.clone(), local, false).unwrap();
        bind_udp_pcb(fresh.id(), fresh_pcb.clone(), local, false).unwrap();

        // Delivery of a packet that belongs to the fresh namespace must never
        // reach the initial namespace's PCB.
        let local_addr = socket_addr(Ipv4Address::new(127, 0, 0, 1), 8085);
        let remote_addr = socket_addr(Ipv4Address::new(127, 0, 0, 1), 40000);
        assert!(Arc::ptr_eq(
            &lookup_udp_pcb(fresh.id(), local_addr, remote_addr).unwrap(),
            &fresh_pcb
        ));
        assert!(Arc::ptr_eq(
            &lookup_udp_pcb(init_net.id(), local_addr, remote_addr).unwrap(),
            &init_pcb
        ));
        // The binding is visible in its own namespace...
        assert!(!udp_port_available(init_net.id(), listen_endpoint(local)));
        // ... and still free in a namespace that did not bind it.
        let third = crate::netns::fresh_unregistered();
        assert!(udp_port_available(third.id(), listen_endpoint(local)));

        clear_registry();
    }
}
