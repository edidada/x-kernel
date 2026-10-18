// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The userland device-node projection: one registry behind devtmpfs,
//! sysfs, and uevent.
//!
//! Architecturally this is the *model-agnostic* identity that both device
//! models publish into:
//!
//! * a [`DevNodeType`] is **data** — subsystem name, major, index count,
//!   devname format — registered by class crates instead of a closed enum
//!   in the device model. Adding a subsystem (block, tty, netdev, ...)
//!   touches no framework code;
//! * a [`DevNode`] is one published node: devname, `dev_t`, physical path,
//!   optional `MODALIAS`, and class-specific uevent properties;
//! * publications are keyed by an opaque `u64` handle (the class
//!   publication id), so neither device model's id space leaks in.
//!
//! The uevent payload format produced from these nodes is a compatibility
//! surface (udev-visible); its property order and bitmap word order are
//! load-bearing.

#![no_std]

extern crate alloc;

use alloc::{format, string::String, sync::Arc, vec::Vec};

use kspin::SpinNoPreempt;

pub mod uevent;

/// Device topology location — owned by the device model
/// (`driver_base::BusDeviceData`), re-exported here so devnode consumers
/// keep a single import path. There is exactly ONE path derivation in the
/// tree: the pre-refactor duplicate (kept so the projection stayed
/// model-agnostic across two device models) died with the old model.
pub use uevent::bitmap_value;

/// Packed `dev_t`: the same layout the block core and devfs use
/// (block and kvfs re-export this type as their DeviceId).
#[derive(Default, Clone, PartialEq, Eq, PartialOrd, Ord, Copy, Hash)]
pub struct DeviceNumber(pub u64);

impl DeviceNumber {
    pub const fn new(major: u32, minor: u32) -> Self {
        let major = major as u64;
        let minor = minor as u64;
        Self(
            (major & 0xffff_f000) << 32
                | (major & 0x0000_0fff) << 8
                | (minor & 0xffff_ff00) << 12
                | (minor & 0x0000_00ff),
        )
    }

    pub const fn major(self) -> u32 {
        ((self.0 >> 32) & 0xffff_f000 | (self.0 >> 8) & 0x0000_0fff) as u32
    }

    pub const fn minor(self) -> u32 {
        ((self.0 >> 12) & 0xffff_ff00 | self.0 & 0x0000_00ff) as u32
    }
}

impl core::fmt::Debug for DeviceNumber {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DeviceNumber")
            .field("major", &self.major())
            .field("minor", &self.minor())
            .finish()
    }
}

/// One class's node type: how index → devname and `dev_t` are formed.
///
/// Registered by class crates (input, drm, block, ...); the framework never
/// enumerates subsystems.
#[derive(Clone)]
pub struct DevNodeType {
    /// Subsystem name (`"input"`, `"drm"`, `"block"`, ...).
    pub subsystem: &'static str,
    /// Registered major for this subsystem's nodes.
    pub major: u32,
    /// Minor base plus per-index count (input reserves 32 event minors).
    pub minor_base: u32,
    pub index_count: u32,
    /// devname for `index` (`input/event{index}`, `dri/card{index}`) — the
    /// /dev projection path.
    pub devname_for: fn(index: u32) -> String,
    /// Canonical sysfs device path for `index` under `physical_path`
    /// (`{phys}/input/input{index}/event{index}`, `{phys}/drm/card{index}`).
    /// The uevent `DEVPATH` and the sysfs tree share this one derivation —
    /// two independent path constructions WILL drift apart (they did).
    pub sysfs_path_for: fn(physical_path: &str, index: u32) -> String,
}

impl DevNodeType {
    pub const fn new(
        subsystem: &'static str,
        major: u32,
        minor_base: u32,
        index_count: u32,
        devname_for: fn(index: u32) -> String,
        sysfs_path_for: fn(physical_path: &str, index: u32) -> String,
    ) -> Self {
        Self {
            subsystem,
            major,
            minor_base,
            index_count,
            devname_for,
            sysfs_path_for,
        }
    }

    pub fn device_number(&self, index: u32) -> DeviceNumber {
        DeviceNumber::new(self.major, self.minor_base + index)
    }

    pub fn devname(&self, index: u32) -> String {
        (self.devname_for)(index)
    }
}

/// One published userland device node.
#[derive(Clone)]
pub struct DevNode {
    id: u64,
    subsystem: &'static str,
    /// The allocated per-class index when this node came from
    /// [`publish_auto_indexed`] (released back to the pool on withdraw).
    pub(crate) allocated_index: Option<u32>,
    /// Canonical sysfs-relative device path (no `/devices` prefix). The
    /// uevent `DEVPATH` and the sysfs tree both consume exactly this.
    sysfs_path: String,
    devname: String,
    device_number: DeviceNumber,
    /// Physical device path (`devices/...` prefix source for DEVPATH).
    physical_path: String,
    /// `MODALIAS` derived from the device's id-table match, when known.
    modalias: Option<String>,
    /// Class-specific uevent properties (`KEY=..`, `NAME=..`, ...).
    properties: Vec<(String, String)>,
    driver_name: Option<String>,
    available: bool,
}

impl DevNode {
    /// Opaque publication handle (class publication id).
    pub const fn id(&self) -> u64 {
        self.id
    }

    pub fn subsystem(&self) -> &'static str {
        self.subsystem
    }

    pub fn devname(&self) -> &str {
        &self.devname
    }

    pub const fn device_number(&self) -> DeviceNumber {
        self.device_number
    }

    /// Canonical sysfs-relative device path (shared by DEVPATH and the
    /// sysfs tree — the single path derivation).
    pub fn sysfs_path(&self) -> &str {
        &self.sysfs_path
    }

    pub fn physical_path(&self) -> &str {
        &self.physical_path
    }

    pub fn modalias(&self) -> Option<&str> {
        self.modalias.as_deref()
    }

    pub fn properties(&self) -> &[(String, String)] {
        &self.properties
    }

    pub fn is_available(&self) -> bool {
        self.available
    }

    pub fn driver_name(&self) -> Option<&str> {
        self.driver_name.as_deref()
    }
}

impl PartialEq for DevNode {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.subsystem == other.subsystem && self.devname == other.devname
    }
}

impl Eq for DevNode {}

/// Lifecycle events, in the order !796 established:
/// `projection add -> add uevent` / `mark removing -> projection quiesce ->
/// remove uevent -> projection delete`.
#[derive(Clone)]
pub enum DevNodeEvent {
    Added(DevNode),
    Removing(DevNode),
    Removed(DevNode),
}

pub type DevNodeCallback = Arc<dyn Fn(DevNodeEvent) + Send + Sync>;

struct Registry {
    nodes: Vec<RegistryEntry>,
    subscribers: Vec<DevNodeCallback>,
    lifecycle_publisher: Option<DevNodeCallback>,
}

/// Registry entry plus the dispatch handshake flag (see [`DISPATCH_ORDER`]).
struct RegistryEntry {
    node: DevNode,
    /// Whether the `Added` event for this node has been dispatched (by its
    /// registering thread, or taken over by a withdraw). Lives ONLY here —
    /// never on the public `DevNode` — so it cannot leak into subscriber
    /// comparisons.
    added_dispatched: bool,
}

/// Serializes EVENT DISPATCH ORDER (not registry mutation): `Added`,
/// `Removing`, `Removed` for one id become observable in that order while
/// NO dispatch runs under the registry spinlock.
///
/// Why not dispatch under `REGISTRY`: the registry lock is
/// `SpinNoPreempt` (preemption disabled), and the subscribers are real
/// projection layers that may block — devfs takes a task-context `Mutex`
/// (real sleep on contention), sysfs rebuilds its tree, uevent allocates
/// and sends. Sleeping with preemption disabled can wedge a core. The
/// same reasoning as `withdraw`'s lock-free dispatch, now applied to
/// `Added`.
///
/// Why the order lock at all: inserting and dispatching `Added` in two
/// steps reopens the historical reorder — a concurrent `withdraw` could
/// finish its whole Removing → Removed → delete sequence before the
/// `Added` dispatch, showing subscribers a device the registry no longer
/// had. Every dispatch path takes this lock first and re-checks the
/// entry's `added_dispatched` under it, so an `Added` is dispatched
/// exactly once, always before that node's `Removing`. A withdraw that
/// finds the `Added` still pending takes it over itself, in order.
///
/// Contract: subscriber callbacks must NOT re-enter devnode
/// register/withdraw — they would self-deadlock on this mutex. (Verified
/// today: subscribers are devfs/sysfs/uevent projections.)
static DISPATCH_ORDER: ksync::Mutex<()> = ksync::Mutex::new(());

static REGISTRY: SpinNoPreempt<Registry> = SpinNoPreempt::new(Registry {
    nodes: Vec::new(),
    subscribers: Vec::new(),
    lifecycle_publisher: None,
});

/// Per-`DevNodeType` min-free index pool with release-on-withdraw.
///
/// Linux equivalence: each class hands out the lowest free minor range
/// (`input/inputN`, `dri/cardN`) and returns it when the device goes away.
/// Exhaustion is [`PublishError::ResourceBusy`] — never a silent alias.
struct IndexPools {
    /// (subsystem, allocated-sorted set); the pool reuses the lowest hole.
    pools: Vec<(&'static str, Vec<u32>)>,
}

static INDEX_POOLS: SpinNoPreempt<IndexPools> =
    SpinNoPreempt::new(IndexPools { pools: Vec::new() });

/// Errors from [`publish_auto_indexed`].
#[derive(Debug)]
pub enum PublishError {
    /// All indices of this node type are taken (`index_count` exhausted).
    ResourceBusy,
    /// A node with this id already exists.
    AlreadyExists,
}

impl IndexPools {
    fn alloc(&mut self, subsystem: &'static str, limit: u32) -> Option<u32> {
        let entry = self.pools.iter_mut().find(|(s, _)| *s == subsystem);
        let taken = match entry {
            Some((_, v)) => v,
            None => {
                self.pools.push((subsystem, Vec::new()));
                #[expect(
                    clippy::unwrap_used,
                    reason = "CG-021-no-panic-on-reachable-failure: the push above guarantees                               the pool exists"
                )]
                {
                    &mut self.pools.last_mut().unwrap().1
                }
            }
        };
        // Min-free: the first gap in the sorted used set.
        let mut candidate = 0;
        for &used in taken.iter() {
            if used == candidate {
                candidate += 1;
            } else {
                break;
            }
        }
        if candidate >= limit {
            return None;
        }
        taken.push(candidate);
        taken.sort_unstable();
        Some(candidate)
    }

    fn release(&mut self, subsystem: &'static str, index: u32) {
        if let Some((_, taken)) = self.pools.iter_mut().find(|(s, _)| *s == subsystem) {
            taken.retain(|&i| i != index);
        }
    }
}

/// Publish with an automatically allocated per-class index: the lowest
/// free index of `node_type`, released when the node is withdrawn.
/// Distinct devices never share a devname or dev_t.
pub fn publish_auto_indexed(
    id: u64,
    node_type: &DevNodeType,
    physical_path: &str,
    modalias: Option<String>,
    properties: Vec<(String, String)>,
    driver_name: Option<String>,
) -> Result<DevNode, PublishError> {
    // The duplicate check must run BEFORE registering: checking afterwards
    // would always find the id we just inserted (and hand the freshly
    // allocated index straight back).
    if REGISTRY.lock().nodes.iter().any(|e| e.node.id == id) {
        return Err(PublishError::AlreadyExists);
    }
    let index = INDEX_POOLS
        .lock()
        .alloc(node_type.subsystem, node_type.index_count);
    let Some(index) = index else {
        return Err(PublishError::ResourceBusy);
    };
    let node = DevNode {
        id,
        allocated_index: Some(index),
        sysfs_path: (node_type.sysfs_path_for)(physical_path, index),
        subsystem: node_type.subsystem,
        devname: node_type.devname(index),
        device_number: node_type.device_number(index),
        physical_path: String::from(physical_path),
        modalias,
        properties,
        driver_name,
        available: true,
    };
    let allocated = node.allocated_index;
    match register_and_dispatch(node) {
        RegisterOutcome::Registered(node) => Ok(node),
        // Duplicate AFTER the pre-check: a concurrent publisher won the
        // race. Its index stays with the winner; ours must be returned.
        RegisterOutcome::DuplicateId(_) => {
            if let Some(allocated) = allocated {
                INDEX_POOLS.lock().release(node_type.subsystem, allocated);
            }
            Err(PublishError::AlreadyExists)
        }
    }
}

/// Publish a node with an explicit devname and `dev_t` (block disks name
/// themselves: `vda`, `sda`, ...).
/// The wide parameter list mirrors Linux's `device_add`-era descriptor
/// fields (identity, naming, dev_t, topology path, modalias, uevent
/// properties); grouping them into a struct would hide which fields are
/// optional at a glance.
#[expect(
    clippy::too_many_arguments,
    reason = "CG-024-lint-exceptions: the publication carries the node's full projection identity \
              (devname, dev_t, paths, modalias, properties, driver) — one struct- like call \
              instead of a builder for a boot-time path"
)]
pub fn publish(
    id: u64,
    subsystem: &'static str,
    devname: String,
    device_number: DeviceNumber,
    physical_path: &str,
    modalias: Option<String>,
    properties: Vec<(String, String)>,
    driver_name: Option<String>,
) -> DevNode {
    let node = DevNode {
        id,
        allocated_index: None,
        sysfs_path: format!("{}/{}", physical_path, devname),
        subsystem,
        devname,
        device_number,
        physical_path: String::from(physical_path),
        modalias,
        properties,
        driver_name,
        available: true,
    };
    match register_and_dispatch(node) {
        RegisterOutcome::Registered(node) => node,
        // Explicit-publish duplicate: keep the ORIGINAL registration and
        // its dispatched state (the old replace-in-place desynced every
        // subscriber). Log loudly; the caller's node is dropped.
        RegisterOutcome::DuplicateId(existing) => {
            log::warn!(
                "devnode: publish id {} ({}) duplicated an existing node; keeping the original",
                id,
                subsystem
            );
            existing
        }
    }
}

/// Register one fully-built node and dispatch its Added event. Registry
/// entry, dispatched copies and the returned handle are the SAME node —
/// callers that patch fields after `publish()` would race every subscriber
/// with a stale copy (the indexed path hit exactly that with sysfs_path).
/// Registration result: a duplicate id is a caller error, not a silent
/// replace. The old code swapped the existing node in place — the first
/// publication's Added event consumers kept stale copies and its class
/// index leaked (two concurrent publishes could each pass the pre-check).
pub(crate) enum RegisterOutcome {
    Registered(DevNode),
    /// Duplicate id: carries the EXISTING node, captured in the same
    /// registry hold that detected the duplicate (a second lock lookup
    /// could race a withdraw and panic).
    DuplicateId(DevNode),
}

fn register_and_dispatch(node: DevNode) -> RegisterOutcome {
    // Insert under the registry lock, dispatch Added OUTSIDE it (see
    // DISPATCH_ORDER). The handshake: the dispatch claim and the
    // subscriber snapshot are taken in ONE registry critical section while
    // holding the dispatch-order mutex, so an `Added` is never dispatched
    // twice (a withdraw between our insert and dispatch either waits on
    // DISPATCH_ORDER behind us, or has already taken the dispatch over and
    // flipped the flag — then we skip).
    {
        let mut registry = REGISTRY.lock();
        if let Some(existing) = registry.nodes.iter().find(|e| e.node.id == node.id) {
            let existing = existing.node.clone();
            return RegisterOutcome::DuplicateId(existing);
        }
        registry.nodes.push(RegistryEntry {
            node: node.clone(),
            added_dispatched: false,
        });
    }
    let _order = DISPATCH_ORDER.lock();
    let dispatch = {
        let mut registry = REGISTRY.lock();
        let Some(entry) = registry.nodes.iter_mut().find(|e| e.node.id == node.id) else {
            // A withdraw took the dispatch over AND removed the node while
            // we waited for the order mutex — nothing left to announce.
            return RegisterOutcome::Registered(node);
        };
        if entry.added_dispatched {
            // The id was withdrawn and re-published (index reuse): the new
            // publication's thread owns the dispatch.
            None
        } else {
            entry.added_dispatched = true;
            Some((
                registry.subscribers.clone(),
                registry.lifecycle_publisher.clone(),
            ))
        }
    };
    if let Some((subscribers, publisher)) = dispatch {
        dispatch_added(&node, subscribers, publisher);
    }
    RegisterOutcome::Registered(node)
}

/// Remove the publication for `id` (idempotent).
pub fn withdraw(id: u64) {
    let node = {
        let mut registry = REGISTRY.lock();
        let Some(entry) = registry.nodes.iter_mut().find(|e| e.node.id == id) else {
            return;
        };
        // Single teardown owner: the FIRST withdraw flips availability;
        // a second withdraw (concurrent or repeated) finds it already
        // false and returns — previously both dispatchers ran Removing/
        // Removed and both released the class index (a reused index could
        // be freed while its new owner was live).
        if !entry.node.available {
            return;
        }
        entry.node.available = false;
        entry.node.clone()
    };
    // All teardown dispatches run under DISPATCH_ORDER so an in-flight
    // `Added` cannot be overtaken (subscribers must observe Added before
    // Removing for this node). If the Added is still pending — the
    // registering thread has not dispatched it — this withdraw takes it
    // over and dispatches it here, in order, exactly once: the register
    // thread re-checks `added_dispatched` under this mutex and skips.
    let _order = DISPATCH_ORDER.lock();
    let (subscribers, publisher, dispatch_added_first) = {
        let mut registry = REGISTRY.lock();
        let take_over_added = registry
            .nodes
            .iter_mut()
            .find(|e| e.node.id == id)
            .is_some_and(|entry| {
                if entry.added_dispatched {
                    false
                } else {
                    entry.added_dispatched = true;
                    true
                }
            });
        (
            registry.subscribers.clone(),
            registry.lifecycle_publisher.clone(),
            take_over_added,
        )
    };
    if dispatch_added_first {
        dispatch_added(&node, subscribers.clone(), publisher.clone());
    }
    for subscriber in &subscribers {
        subscriber(DevNodeEvent::Removing(node.clone()));
    }
    if let Some(publisher) = publisher {
        publisher(DevNodeEvent::Removed(node.clone()));
    }
    REGISTRY.lock().nodes.retain(|e| e.node.id != id);
    for subscriber in subscribers {
        subscriber(DevNodeEvent::Removed(node.clone()));
    }
    // Release the index only after EVERY Removed dispatch (publisher and
    // subscribers): the devname/dev_t must stay exclusive for as long as
    // the node lives in devfs/sysfs, or a new device could cdev_add the
    // same dev_t while the old node's teardown is still in flight.
    if let Some(index) = node.allocated_index {
        INDEX_POOLS.lock().release(node.subsystem, index);
    }
}

/// Available-node snapshot.
pub fn nodes() -> Vec<DevNode> {
    REGISTRY
        .lock()
        .nodes
        .iter()
        .filter(|e| e.node.available)
        .map(|e| e.node.clone())
        .collect()
}

/// Subscribe to node events; returns the current snapshot.
pub fn subscribe(callback: DevNodeCallback) -> Vec<DevNode> {
    let mut registry = REGISTRY.lock();
    registry.subscribers.push(callback);
    registry
        .nodes
        .iter()
        .filter(|e| e.node.available)
        .map(|e| e.node.clone())
        .collect()
}

/// Install the single uevent lifecycle publisher. Must run before any
/// publication (boot-order discipline, same as !796).
/// Returns `false` if a publisher is already installed (boot-order
/// discipline, same as !796).
pub fn install_lifecycle_publisher(callback: DevNodeCallback) -> bool {
    let mut registry = REGISTRY.lock();
    if registry.lifecycle_publisher.is_some() {
        return false;
    }
    if !registry.nodes.is_empty() {
        return false;
    }
    registry.lifecycle_publisher = Some(callback);
    true
}

fn dispatch_added(
    node: &DevNode,
    subscribers: Vec<DevNodeCallback>,
    publisher: Option<DevNodeCallback>,
) {
    for subscriber in subscribers {
        subscriber(DevNodeEvent::Added(node.clone()));
    }
    if let Some(publisher) = publisher {
        publisher(DevNodeEvent::Added(node.clone()));
    }
}

/// Format the canonical physical path for a PCI device location (the
/// `devices/...` prefix source for DEVPATH and sysfs).
///
/// The single derivation: the block driver, the virtio drivers and the
/// class projections all call this — independent copies drifted before
/// (DEVPATH vs sysfs tree mismatch).
pub fn physical_path_pci(segment: u16, bus: u8, device: u8, function: u8) -> String {
    format!("pci{segment:04x}:{bus:02x}/{segment:04x}:{bus:02x}:{device:02x}.{function}")
}

#[unittest::mod_test]
mod dispatch_order_tests {
    //! The dispatch-order protocol: NO event dispatch runs under the
    //! registry spinlock (subscribers are projection layers that may
    //! block — devfs's task-context Mutex, sysfs rebuilds), and
    //! Added stays observable before Removing/Removed.
    //!
    //! Re-entry contract tested here: subscribers may READ the registry
    //! (`nodes()`) from inside a callback. Under the old protocol the
    //! Added dispatch held the SpinNoPreempt registry lock, so this
    //! re-entrant read self-deadlocked on the same CPU.

    use alloc::{
        string::{String, ToString},
        sync::Arc,
        vec,
        vec::Vec,
    };
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use ksync::Mutex;
    use unittest::{assert_eq, def_test};

    use super::{DevNodeEvent, DeviceNumber, publish, withdraw};

    #[def_test]
    fn added_dispatch_runs_outside_the_registry_lock() {
        // A subscriber reading the registry from INSIDE its Added callback
        // must not deadlock: the callback runs outside REGISTRY, under
        // DISPATCH_ORDER only (reads take REGISTRY briefly — no cycle).
        let reentered = Arc::new(AtomicBool::new(false));
        let order = Arc::new(AtomicUsize::new(0));
        let (re_flag, order_flag) = (reentered.clone(), order.clone());
        let _sub = crate::subscribe(Arc::new(move |event| {
            if let DevNodeEvent::Added(node) = event {
                // Re-entrant registry read — the assertion this test exists
                // for (deadlocks under the old dispatch-under-lock).
                re_flag.store(
                    crate::nodes().iter().any(|n| n.id == node.id),
                    Ordering::Release,
                );
                order_flag.store(1, Ordering::Release);
            }
        }));

        let node = publish(
            0x1d01,
            "dispatch_test",
            String::from("reentry0"),
            DeviceNumber(0x1d0),
            "platform/dispatch-test",
            None,
            Vec::new(),
            None,
        );
        assert!(reentered.load(Ordering::Acquire));
        assert_eq!(order.load(Ordering::Acquire), 1);
        withdraw(node.id);
    }

    #[def_test]
    fn added_is_observed_before_removing_and_removed() {
        // Event order per node: Added → Removing → Removed. The take-over
        // path (a withdraw racing a pending Added dispatch) preserves the
        // same order by construction — it serializes on DISPATCH_ORDER.
        // The registry is process-global and earlier tests may still hold
        // subscriptions: record only THIS node's events.
        let watched = 0x1d02u64;
        let seq = Arc::new(Mutex::new(Vec::new()));
        let seq2 = seq.clone();
        let _sub = crate::subscribe(Arc::new(move |event| {
            let (label, id) = match event {
                DevNodeEvent::Added(n) => ("added", n.id),
                DevNodeEvent::Removing(n) => ("removing", n.id),
                DevNodeEvent::Removed(n) => ("removed", n.id),
            };
            if id == watched {
                seq2.lock().push(label);
            }
        }));

        let node = publish(
            0x1d02,
            "dispatch_test",
            String::from("order0"),
            DeviceNumber(0x1d1),
            "platform/dispatch-test",
            None,
            Vec::new(),
            None,
        );
        withdraw(node.id);
        assert_eq!(
            *seq.lock(),
            vec![
                "added".to_string(),
                "removing".to_string(),
                "removed".to_string()
            ]
        );
    }
}
