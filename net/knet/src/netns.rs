// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network namespace instances.
//!
//! A [`NetNamespace`] owns one complete protocol stack: the [`Service`]
//! (smoltcp interface plus device and route tables), the INET listen table,
//! and the smoltcp socket set. The initial namespace ([`INIT_NET`]) is built
//! by [`crate::init_network`] from the physical NICs and keeps the exact
//! behavior the stack had while this state was global.
//!
//! Fresh namespaces are created by `clone`/`unshare` with `CLONE_NEWNET` via
//! [`create_fresh`] and contain a single loopback device with `127.0.0.1/8`
//! assigned but down, matching Linux `create_new_namespaces`.
//!
//! # Current namespace
//!
//! [`current`] resolves the caller's namespace identifier through the
//! `knetns::NetNsResolver` interface provided by the process subsystem and
//! looks it up in [`NETNS_TABLE`]. Contexts without a process report the
//! initial namespace, and an unknown identifier falls back to the initial
//! namespace with a warning: the stack never silently crosses a namespace
//! boundary, it only degrades to the initial one.

use alloc::{boxed::Box, collections::BTreeMap, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};

use kerrno::{KError, KResult};
use knetns::NetNsResolver;
pub use knetns::{INIT_NET_ID, NetNsId};
use kspin::SpinNoIrq;
use ksync::{Mutex, static_lock};
use lazyinit::LazyInit;
use log::warn;

use crate::{
    LoopbackDevice,
    ip::{Ipv4Address, Ipv4Cidr},
    listen_table::ListenTable,
    router::{Ipv4AddrEntry, ROUTE_SCOPE_HOST, Router},
    service::Service,
    wrapper::SocketSetWrapper,
};

/// Shared smoltcp socket set owned by a namespace.
pub(crate) type SharedSocketSet = Arc<SocketSetWrapper<'static>>;

/// The initial network namespace, created by [`crate::init_network`].
static INIT_NET: LazyInit<Arc<NetNamespace>> = LazyInit::new();

static_lock! {
    /// Live network namespaces keyed by identifier.
    ///
    /// The registry only owns short bookkeeping sections: readers take a
    /// snapshot clone of the `Arc` and drop the lock before touching namespace
    /// state, and the poller iterates the same way. Entries are never removed
    /// while sockets or the registry still reference them, so a namespace can
    /// never be freed under a live user.
    static NETNS_TABLE: Mutex<BTreeMap<NetNsId, Arc<NetNamespace>>> = Mutex::new(BTreeMap::new());
}

/// Next network namespace identifier; the initial namespace owns `1`.
static NEXT_NS_ID: AtomicU64 = AtomicU64::new(INIT_NET_ID.as_u64() + 1);

static_lock! {
    /// Namespaces sampled by the periodic protocol timer.
    ///
    /// The list is append-only and is iterated in place under its IRQ-safe
    /// lock: the timer callback runs in timer IRQ context, so it must neither
    /// allocate nor acquire a sleepable lock. [`Service::handle_timer_tick`]
    /// is IRQ-safe by design, which is what makes this walk legal.
    static TIMER_SAMPLES: SpinNoIrq<Vec<Arc<NetNamespace>>> = SpinNoIrq::new(Vec::new());
}

/// A network namespace: one protocol stack, listen table, and socket set.
pub struct NetNamespace {
    id: NetNsId,
    service: Service,
    listen_table: Arc<ListenTable>,
    socket_set: SharedSocketSet,
}

impl NetNamespace {
    /// Returns the namespace identifier reported by `/proc/<pid>/ns/net`.
    pub fn id(&self) -> NetNsId {
        self.id
    }

    /// Returns whether this is the initial namespace.
    pub fn is_initial(&self) -> bool {
        self.id == INIT_NET_ID
    }

    /// Returns the protocol stack of this namespace.
    pub(crate) fn service(&self) -> &Service {
        &self.service
    }

    /// Returns the INET listen table of this namespace.
    pub(crate) fn listen_table(&self) -> &Arc<ListenTable> {
        &self.listen_table
    }

    /// Returns the smoltcp socket set of this namespace.
    pub(crate) fn socket_set(&self) -> &SharedSocketSet {
        &self.socket_set
    }

    fn new(id: NetNsId, router: Router) -> Arc<Self> {
        let listen_table = Arc::new(ListenTable::new());
        let socket_set = Arc::new(SocketSetWrapper::new());
        let service = Service::new(router, socket_set.clone(), listen_table.clone());
        Arc::new(Self {
            id,
            service,
            listen_table,
            socket_set,
        })
    }
}

/// Returns whether the network subsystem has created the initial namespace.
pub(crate) fn initialized() -> bool {
    INIT_NET.is_inited()
}

/// Returns the initial namespace, if the network subsystem is up.
pub(crate) fn init_net() -> Option<Arc<NetNamespace>> {
    INIT_NET.get().cloned()
}

/// Returns the initial namespace.
///
/// # Panics
///
/// Panics when the network subsystem has not been initialized.
pub(crate) fn expect_init_net() -> Arc<NetNamespace> {
    INIT_NET
        .get()
        .cloned()
        .expect("network namespace requested before init_network()")
}

/// Returns the network namespace of the calling context.
///
/// Resolves through the process-provided [`NetNsResolver`]; contexts without a
/// process (softirq, timer, boot) resolve to the initial namespace. An
/// identifier without a live namespace entry is reported and falls back to the
/// initial namespace instead of being served from another namespace.
pub fn current() -> Arc<NetNamespace> {
    let id = NetNsResolver::current();
    match lookup(id) {
        Some(net_ns) => net_ns,
        None if id == INIT_NET_ID => expect_init_net(),
        None => {
            warn!(
                "netns: namespace {} is not registered; using the initial namespace",
                id.as_u64()
            );
            expect_init_net()
        }
    }
}

fn lookup(id: NetNsId) -> Option<Arc<NetNamespace>> {
    if !initialized() {
        return None;
    }
    NETNS_TABLE.lock().get(&id).cloned()
}

/// Creates a fresh, isolated network namespace.
///
/// The namespace holds only a loopback device with `127.0.0.1/8` assigned and
/// the link down, matching Linux. It shares nothing with the creating
/// namespace: devices, addresses, routes, listen table, and sockets are all
/// new and empty.
///
/// # Errors
///
/// Returns [`KError::Unsupported`] when the network subsystem is not
/// initialized, so callers report a real failure instead of silently staying
/// in the caller's namespace.
pub fn create_fresh() -> KResult<Arc<NetNamespace>> {
    if !initialized() {
        return Err(KError::Unsupported);
    }
    let id = NetNsId::from_u64(NEXT_NS_ID.fetch_add(1, Ordering::AcqRel))
        .expect("network namespace id must not wrap");
    let net_ns = NetNamespace::new(id, loopback_router(id));
    NETNS_TABLE.lock().insert(id, net_ns.clone());
    TIMER_SAMPLES.lock().push(net_ns.clone());
    debug_assert_ne!(id, INIT_NET_ID);
    Ok(net_ns)
}

/// Returns the namespace backing transports that are not namespaced.
///
/// `AF_UNIX` and vsock stay global by design, but their sockets still carry
/// generic options, so they use the initial namespace view when it exists and
/// an unregistered loopback namespace before the network subsystem is up. The
/// fallback is never inserted into [`NETNS_TABLE`], so it cannot be resolved
/// through [`current`].
pub(crate) fn global_options_net_ns() -> Arc<NetNamespace> {
    static FALLBACK: LazyInit<Arc<NetNamespace>> = LazyInit::new();
    if let Some(net_ns) = init_net() {
        return net_ns;
    }
    let id = NetNsId::from_u64(NEXT_NS_ID.fetch_add(1, Ordering::AcqRel))
        .expect("network namespace id must not wrap");
    FALLBACK.call_once(|| NetNamespace::new(id, loopback_router(id)));
    FALLBACK
        .get()
        .cloned()
        .expect("fallback namespace was just initialized")
}

/// Registers the initial namespace built from [`crate::init_network`].
pub(crate) fn init_initial(router: Router) -> Arc<NetNamespace> {
    let net_ns = NetNamespace::new(INIT_NET_ID, router);
    INIT_NET.init_once(net_ns.clone());
    NETNS_TABLE.lock().insert(INIT_NET_ID, net_ns.clone());
    TIMER_SAMPLES.lock().push(net_ns.clone());
    net_ns
}

/// Samples the protocol timers of every live namespace.
///
/// Called from the periodic timer callback in timer IRQ context. The walk runs
/// under the IRQ-safe list lock and only touches atomic deadline state, so it
/// neither allocates nor sleeps.
pub(crate) fn sample_timers() {
    for net_ns in TIMER_SAMPLES.lock().iter() {
        net_ns.service().handle_timer_tick();
    }
}

fn loopback_router(id: NetNsId) -> Router {
    let mut router = Router::new();
    let lo_dev = router.add_device(Box::new(LoopbackDevice::new_in_net_ns(id)));
    router
        .add_ipv4_addr(Ipv4AddrEntry {
            dev: lo_dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        })
        .expect("loopback IPv4 address must fit the interface");
    router
}

/// Returns the namespaces that currently exist, as strong snapshots.
///
/// Used by the poller: the registry lock is released before any namespace is
/// polled, so polling can never run while the registry is locked.
pub(crate) fn snapshot() -> Vec<Arc<NetNamespace>> {
    if !initialized() {
        return Vec::new();
    }
    NETNS_TABLE.lock().values().cloned().collect()
}

/// Initializes the initial namespace for unit tests.
///
/// Test namespaces are loopback-only; tests that need other devices replace
/// the router of the returned namespace.
#[cfg(unittest)]
pub(crate) fn test_init_net() -> Arc<NetNamespace> {
    if let Some(net_ns) = init_net() {
        return net_ns;
    }
    init_initial(loopback_router(INIT_NET_ID))
}

/// Creates a fresh namespace without registering it.
///
/// Isolation tests use this to build a second namespace that cannot see the
/// initial namespace's state.
#[cfg(unittest)]
pub(crate) fn fresh_unregistered() -> Arc<NetNamespace> {
    test_init_net();
    let id = NetNsId::from_u64(NEXT_NS_ID.fetch_add(1, Ordering::AcqRel)).unwrap();
    NetNamespace::new(id, loopback_router(id))
}

#[cfg(unittest)]
mod tests {
    use alloc::{string::ToString, vec::Vec};

    use smoltcp::wire::{IpAddress, IpListenEndpoint};
    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::device::LinkKind;

    fn interface_names(net_ns: &NetNamespace) -> Vec<alloc::string::String> {
        net_ns
            .service()
            .link_snapshots()
            .into_iter()
            .map(|link| link.name)
            .collect()
    }

    fn loopback_endpoint(port: u16) -> IpListenEndpoint {
        IpListenEndpoint {
            addr: Some(IpAddress::Ipv4(smoltcp::wire::Ipv4Address::new(
                127, 0, 0, 1,
            ))),
            port,
        }
    }

    /// Serial: mutates the process-wide namespace registry.
    #[def_test(serial)]
    fn init_namespace_has_id_one_and_is_registered() {
        test_init_net();

        assert_eq!(INIT_NET_ID.as_u64(), 1);
        let init_net = expect_init_net();
        assert_eq!(init_net.id().as_u64(), 1);
        assert!(init_net.is_initial());
        assert!(NETNS_TABLE.lock().contains_key(&INIT_NET_ID));
    }

    /// Serial: allocates namespace identifiers.
    #[def_test(serial)]
    fn fresh_namespaces_get_distinct_ids_above_init() {
        let first = create_fresh().unwrap();
        let second = create_fresh().unwrap();

        assert_ne!(first.id(), second.id());
        assert_ne!(first.id(), INIT_NET_ID);
        assert!(first.id().as_u64() > 1);
        assert!(second.id().as_u64() > first.id().as_u64());
    }

    /// Serial: rebuilds the shared test router and creates a fresh namespace.
    #[def_test(serial)]
    fn fresh_namespace_only_sees_loopback_without_routes() {
        test_init_net();
        let fresh = fresh_unregistered();

        // A fresh namespace has exactly one device, `lo`, and no route: it
        // cannot see the initial namespace's eth0 or its default route.
        let names = interface_names(&fresh);
        assert_eq!(names.len(), 1);
        assert_eq!(names[0], "lo");
        assert_eq!(fresh.service().route_snapshot().len(), 0);
        assert!(fresh.service().link_snapshot_for_ifindex(2).is_none());

        let addresses = fresh.service().ipv4_addr_snapshots();
        assert_eq!(addresses.len(), 1);
        assert_eq!(addresses[0].entry.addr.address().to_string(), "127.0.0.1");

        // The initial namespace keeps its own, unrelated view.
        let init_net = expect_init_net();
        assert!(interface_names(&init_net).iter().any(|name| name == "lo"));
        assert!(
            init_net
                .service()
                .link_snapshots()
                .iter()
                .any(|link| link.kind == LinkKind::Loopback)
        );
    }

    /// Serial: binds a listener in one namespace and checks the other.
    #[def_test(serial)]
    fn listen_tables_are_isolated_between_namespaces() {
        test_init_net();
        let fresh = fresh_unregistered();
        let endpoint = loopback_endpoint(45_001);

        fresh.listen_table().listen(endpoint, 1).unwrap();
        assert!(fresh.listen_table().can_listen(endpoint));
        // The initial namespace owns a separate listen table, so the same
        // address and port are still free there.
        assert!(!expect_init_net().listen_table().can_listen(endpoint));
    }

    /// Serial: socket sets are per namespace.
    #[def_test(serial)]
    fn socket_sets_start_empty_and_are_not_shared() {
        test_init_net();
        let fresh = fresh_unregistered();

        let fresh_sockets = fresh.socket_set().inner.lock();
        assert_eq!(fresh_sockets.iter().count(), 0);
        drop(fresh_sockets);
        let init_net = expect_init_net();
        let init_sockets = init_net.socket_set().inner.lock();
        assert_eq!(init_sockets.iter().count(), 0);
    }

    /// Serial: lookups of unregistered identifiers fall back to init.
    #[def_test(serial)]
    fn snapshot_never_contains_an_unregistered_namespace() {
        test_init_net();
        let fresh = fresh_unregistered();

        let registered = snapshot();
        assert!(registered.iter().any(|net_ns| net_ns.id() == INIT_NET_ID));
        assert!(
            !registered.iter().any(|net_ns| net_ns.id() == fresh.id()),
            "an unregistered namespace must not be resolvable"
        );
    }
}
