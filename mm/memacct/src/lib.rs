// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Hierarchical memory accounting for memory cgroups.
//!
//! [`Memcg`] owns the usage, protection and event counters of one cgroup.
//! Physical pages record the accounting group that first charged them, so
//! releasing a page does not depend on the releasing task still running in the
//! same cgroup. Controllers live in `kcgroup`; this crate only owns the
//! accounting tree and the page-ownership hand-off.
//!
//! Charging is lock-free with respect to ordinary scheduler and page-fault
//! state: counters are atomics and a short IRQ-safe lock serializes ancestor
//! commits so a limit failure never leaves a partially charged chain.
#![no_std]

extern crate alloc;

use alloc::{collections::BTreeMap, sync::Arc};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use kerrno::{KError, KResult};
use klazy::Once;
use kspin::{SpinNoIrq, static_lock};
use memaddr::PhysAddr;
use page_meta::{UsageKind, lookup};

const PAGE_SIZE: usize = 4096;
const UNLIMITED: u64 = u64::MAX;

/// Identity of one memory cgroup.
///
/// Zero means that no memory cgroup owns the page or task. Identities are
/// allocated monotonically and are never reused.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MemcgId(u32);

impl MemcgId {
    /// The absence of a memory cgroup.
    pub const NONE: Self = Self(0);

    /// Reconstructs an identity previously read from task or page metadata.
    ///
    /// The value must have come from a [`Memcg`] created by this crate.
    pub const fn from_raw(id: u32) -> Self {
        Self(id)
    }

    /// Returns whether no memory cgroup is associated with this value.
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// Returns the raw identity used by task and page metadata.
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

/// Page-attribution counters exposed through `memory.stat`.
///
/// Only classes with a real charge source are reported; a category that this
/// kernel cannot distinguish is omitted rather than derived from the total.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MemoryStats {
    /// Anonymous user memory charged through the virtual-memory backends.
    pub anon: u64,
    /// File-backed page-cache memory.
    pub file: u64,
}

/// Hierarchical memory counters exposed through `memory.events`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MemoryEvents {
    /// Times usage crossed the `memory.low` boundary upwards.
    pub low: u64,
    /// Times usage crossed `memory.high` upwards.
    pub high: u64,
    /// Times a charge was rejected by `memory.max`.
    pub max: u64,
    /// Times the out-of-memory handler was invoked.
    pub oom: u64,
    /// Tasks killed by the out-of-memory handler.
    pub oom_kill: u64,
}

/// One node of the memory accounting hierarchy.
///
/// A node is created once and never freed while the kernel runs, so a page or
/// task can hold a stable identity without keeping the cgroup object alive.
pub struct Memcg {
    id: MemcgId,
    parent: Option<Arc<Self>>,
    usage: AtomicU64,
    subtree_usage: AtomicU64,
    peak: AtomicU64,
    subtree_peak: AtomicU64,
    max: AtomicU64,
    high: AtomicU64,
    low: AtomicU64,
    swap_max: AtomicU64,
    events: Events,
    subtree_events: Events,
    stats: StatCounters,
    subtree_stats: StatCounters,
}

struct Events {
    low: AtomicU64,
    high: AtomicU64,
    max: AtomicU64,
    oom: AtomicU64,
    oom_kill: AtomicU64,
}

impl Events {
    const fn new() -> Self {
        Self {
            low: AtomicU64::new(0),
            high: AtomicU64::new(0),
            max: AtomicU64::new(0),
            oom: AtomicU64::new(0),
            oom_kill: AtomicU64::new(0),
        }
    }

    fn note(&self, kind: EventKind) {
        let counter = match kind {
            EventKind::Low => &self.low,
            EventKind::High => &self.high,
            EventKind::Max => &self.max,
            EventKind::Oom => &self.oom,
            EventKind::OomKill => &self.oom_kill,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy)]
enum ChargeClass {
    Anonymous,
    File,
}

impl ChargeClass {
    fn of(kind: UsageKind) -> Self {
        match kind {
            UsageKind::PageCache => Self::File,
            _ => Self::Anonymous,
        }
    }
}

struct StatCounters {
    anon: AtomicU64,
    file: AtomicU64,
}

impl StatCounters {
    const fn new() -> Self {
        Self {
            anon: AtomicU64::new(0),
            file: AtomicU64::new(0),
        }
    }

    fn counter(&self, class: ChargeClass) -> &AtomicU64 {
        match class {
            ChargeClass::Anonymous => &self.anon,
            ChargeClass::File => &self.file,
        }
    }

    fn add(&self, class: ChargeClass, bytes: u64) {
        self.counter(class).fetch_add(bytes, Ordering::AcqRel);
    }

    fn subtract(&self, class: ChargeClass, bytes: u64) {
        self.counter(class).fetch_sub(bytes, Ordering::AcqRel);
    }

    fn snapshot(&self) -> MemoryStats {
        MemoryStats {
            anon: self.anon.load(Ordering::Acquire),
            file: self.file.load(Ordering::Acquire),
        }
    }
}

#[derive(Clone, Copy)]
enum EventKind {
    Low,
    High,
    Max,
    Oom,
    OomKill,
}

/// Records one event on `node` and its ancestors while the charge lock is
/// held, so hierarchical counters stay consistent with the charge rollback.
///
/// The local counter belongs only to the node whose boundary was crossed;
/// ancestors observe the same event through their subtree counter.
fn note_event_locked(node: &Memcg, kind: EventKind) {
    node.events.note(kind);
    note_subtree_event_locked(node, kind);
}

fn note_subtree_event_locked(node: &Memcg, kind: EventKind) {
    node.subtree_events.note(kind);
    if let Some(parent) = &node.parent {
        note_subtree_event_locked(parent, kind);
    }
}

impl Memcg {
    fn new(id: MemcgId, parent: Option<Arc<Self>>) -> Self {
        Self {
            id,
            parent,
            usage: AtomicU64::new(0),
            subtree_usage: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            subtree_peak: AtomicU64::new(0),
            max: AtomicU64::new(UNLIMITED),
            high: AtomicU64::new(UNLIMITED),
            low: AtomicU64::new(0),
            swap_max: AtomicU64::new(UNLIMITED),
            events: Events::new(),
            subtree_events: Events::new(),
            stats: StatCounters::new(),
            subtree_stats: StatCounters::new(),
        }
    }

    /// Returns this node's stable identity.
    pub fn id(&self) -> MemcgId {
        self.id
    }

    /// Returns the parent node, or `None` for the root.
    pub fn parent(&self) -> Option<&Arc<Self>> {
        self.parent.as_ref()
    }

    /// Returns the charge currently attributed to this node alone.
    pub fn usage(&self) -> u64 {
        self.usage.load(Ordering::Acquire)
    }

    /// Returns the largest charge this node has held.
    pub fn peak(&self) -> u64 {
        self.peak.load(Ordering::Acquire)
    }

    /// Returns the charge of this node and every descendant.
    pub fn hierarchical_usage(&self) -> u64 {
        self.subtree_usage.load(Ordering::Acquire)
    }

    /// Returns the largest hierarchical charge observed.
    pub fn hierarchical_peak(&self) -> u64 {
        self.subtree_peak.load(Ordering::Acquire)
    }

    /// Returns the soft limit of this node, or `None` when unlimited.
    pub fn max(&self) -> Option<u64> {
        decode_limit(self.max.load(Ordering::Acquire))
    }

    /// Sets a hard limit. The new value may be below current usage; further
    /// charges then fail until usage drops.
    pub fn set_max(&self, limit: Option<u64>) {
        self.max.store(encode_limit(limit), Ordering::Release);
    }

    /// Returns the reclaim threshold, or `None` when unlimited.
    pub fn high(&self) -> Option<u64> {
        decode_limit(self.high.load(Ordering::Acquire))
    }

    /// Sets the reclaim threshold. Reaching it records an event but does not
    /// reject the charge.
    pub fn set_high(&self, limit: Option<u64>) {
        self.high.store(encode_limit(limit), Ordering::Release);
    }

    /// Returns the reclaim-protection floor.
    pub fn low(&self) -> u64 {
        self.low.load(Ordering::Acquire)
    }

    /// Sets the reclaim-protection floor.
    pub fn set_low(&self, limit: u64) {
        self.low.store(limit, Ordering::Release);
    }

    /// Returns the swap limit, or `None` when unlimited.
    pub fn swap_max(&self) -> Option<u64> {
        decode_limit(self.swap_max.load(Ordering::Acquire))
    }

    /// Sets the swap limit. This kernel has no swap backend, so only
    /// unlimited and zero are meaningful.
    pub fn set_swap_max(&self, limit: Option<u64>) {
        self.swap_max.store(encode_limit(limit), Ordering::Release);
    }

    /// Returns local event counters.
    pub fn events(&self) -> MemoryEvents {
        MemoryEvents {
            low: self.events.low.load(Ordering::Relaxed),
            high: self.events.high.load(Ordering::Relaxed),
            max: self.events.max.load(Ordering::Relaxed),
            oom: self.events.oom.load(Ordering::Relaxed),
            oom_kill: self.events.oom_kill.load(Ordering::Relaxed),
        }
    }

    /// Returns page-attribution counters for this node alone.
    pub fn stats(&self) -> MemoryStats {
        self.stats.snapshot()
    }

    /// Returns page-attribution counters for this node and all descendants.
    pub fn hierarchical_stats(&self) -> MemoryStats {
        self.subtree_stats.snapshot()
    }

    /// Resets the visible peak to the current usage.
    ///
    /// The charge lock excludes concurrent commits, so the new peak is a
    /// coherent snapshot rather than a value already below live usage.
    pub fn reset_peak(&self) {
        let _guard = CHARGE_LOCK.lock();
        self.peak.store(self.usage(), Ordering::Release);
        self.subtree_peak.store(
            self.subtree_usage.load(Ordering::Acquire),
            Ordering::Release,
        );
    }

    /// Returns event counters for this node and all descendants.
    pub fn hierarchical_events(&self) -> MemoryEvents {
        MemoryEvents {
            low: self.subtree_events.low.load(Ordering::Relaxed),
            high: self.subtree_events.high.load(Ordering::Relaxed),
            max: self.subtree_events.max.load(Ordering::Relaxed),
            oom: self.subtree_events.oom.load(Ordering::Relaxed),
            oom_kill: self.subtree_events.oom_kill.load(Ordering::Relaxed),
        }
    }

    /// Records a completed OOM kill against this node.
    pub fn note_oom_kill(&self) {
        let _guard = CHARGE_LOCK.lock();
        note_event_locked(self, EventKind::OomKill);
    }
}

const fn encode_limit(limit: Option<u64>) -> u64 {
    match limit {
        Some(value) => value,
        None => UNLIMITED,
    }
}

const fn decode_limit(raw: u64) -> Option<u64> {
    if raw == UNLIMITED { None } else { Some(raw) }
}

static ROOT: Once<Arc<Memcg>> = Once::new();
static NEXT_ID: AtomicU32 = AtomicU32::new(1);
static_lock! {
    static TREE: SpinNoIrq<BTreeMap<u32, Arc<Memcg>>> = SpinNoIrq::new(BTreeMap::new());
}
static_lock! {
    static CHARGE_LOCK: SpinNoIrq<()> = SpinNoIrq::new(());
}
static_lock! {
    static OOM_HANDLER: SpinNoIrq<Option<fn(MemcgId)>> = SpinNoIrq::new(None);
}
static CURRENT: [AtomicU32; kbuild_config::NR_CPUS] =
    [const { AtomicU32::new(0) }; kbuild_config::NR_CPUS];

fn new_node(parent: Option<Arc<Memcg>>) -> Arc<Memcg> {
    let raw = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    assert!(raw != 0, "memory cgroup identity space exhausted");
    let node = Arc::new(Memcg::new(MemcgId::from_raw(raw), parent));
    TREE.lock().insert(raw, Arc::clone(&node));
    node
}

/// Returns the root accounting node, creating it on first use.
pub fn root() -> Arc<Memcg> {
    if let Some(root) = ROOT.get() {
        return Arc::clone(root);
    }
    Arc::clone(ROOT.call_once(|| new_node(None)))
}

/// Creates a child of `parent`.
pub fn create_child(parent: &Arc<Memcg>) -> Arc<Memcg> {
    new_node(Some(Arc::clone(parent)))
}

/// Looks up a node by stable identity.
pub fn get(id: MemcgId) -> Option<Arc<Memcg>> {
    if id.is_none() {
        return None;
    }
    TREE.lock().get(&id.0).cloned()
}

/// Installs the single out-of-memory notifier.
///
/// Returns false when a notifier is already installed. The callback runs after
/// the accounting lock is released and must not sleep.
pub fn install_oom_handler(handler: fn(MemcgId)) -> bool {
    let mut slot = OOM_HANDLER.lock();
    if slot.is_some() {
        return false;
    }
    *slot = Some(handler);
    true
}

/// Returns the memory cgroup charged on the current CPU.
pub fn current_memcg() -> MemcgId {
    let cpu = khal::percpu::this_cpu_id().as_usize();
    CURRENT.get(cpu).map_or(MemcgId::NONE, |id| {
        MemcgId::from_raw(id.load(Ordering::Relaxed))
    })
}

/// Sets the memory cgroup charged by subsequent page allocations on this CPU.
pub fn set_current_memcg(id: MemcgId) {
    let cpu = khal::percpu::this_cpu_id().as_usize();
    if let Some(slot) = CURRENT.get(cpu) {
        slot.store(id.0, Ordering::Relaxed);
    }
}

fn record_stats_locked(node: &Memcg, target: &Memcg, bytes: u64, class: ChargeClass) {
    if core::ptr::eq(node, target) {
        node.stats.add(class, bytes);
    }
    node.subtree_stats.add(class, bytes);
}

fn release_subtree_stats_locked(node: &Memcg, bytes: u64, class: ChargeClass) {
    node.subtree_stats.subtract(class, bytes);
    if let Some(parent) = &node.parent {
        release_subtree_stats_locked(parent, bytes, class);
    }
}

fn release_stats_locked(node: &Memcg, target: &Memcg, bytes: u64, class: ChargeClass) {
    if core::ptr::eq(node, target) {
        node.stats.subtract(class, bytes);
    }
    release_subtree_stats_locked(node, bytes, class);
}

fn uncharge_locked(node: &Memcg, target: &Memcg, bytes: u64, class: ChargeClass) {
    if core::ptr::eq(node, target) {
        node.usage.fetch_sub(bytes, Ordering::AcqRel);
        node.stats.subtract(class, bytes);
    }
    node.subtree_usage.fetch_sub(bytes, Ordering::AcqRel);
    node.subtree_stats.subtract(class, bytes);
    if let Some(parent) = &node.parent {
        uncharge_locked(parent, target, bytes, class);
    }
}

fn release_subtree_locked(node: &Memcg, bytes: u64) {
    node.subtree_usage.fetch_sub(bytes, Ordering::AcqRel);
    if let Some(parent) = &node.parent {
        release_subtree_locked(parent, bytes);
    }
}

fn charge_locked(
    node: &Arc<Memcg>,
    target: &Arc<Memcg>,
    bytes: u64,
    class: ChargeClass,
) -> Result<(), MemcgId> {
    if let Some(parent) = &node.parent {
        charge_locked(parent, target, bytes, class)?;
    }

    let is_target = Arc::ptr_eq(node, target);
    let previous = if is_target {
        Some(node.usage.fetch_add(bytes, Ordering::AcqRel))
    } else {
        None
    };
    let previous_subtree = node.subtree_usage.fetch_add(bytes, Ordering::AcqRel);
    record_stats_locked(node, target, bytes, class);
    let charged_subtree = previous_subtree.saturating_add(bytes);
    let max = node.max.load(Ordering::Acquire);
    if max != UNLIMITED && charged_subtree > max {
        if is_target {
            node.usage.fetch_sub(bytes, Ordering::AcqRel);
        }
        release_subtree_locked(node, bytes);
        release_stats_locked(node, target, bytes, class);
        note_event_locked(node, EventKind::Max);
        note_event_locked(node, EventKind::Oom);
        return Err(node.id);
    }

    if let Some(previous) = previous {
        let charged = previous.saturating_add(bytes);
        let _ = node
            .peak
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |peak| {
                (charged > peak).then_some(charged)
            });
    }
    let _ = node
        .subtree_peak
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |peak| {
            (charged_subtree > peak).then_some(charged_subtree)
        });
    let high = node.high.load(Ordering::Acquire);
    if high != UNLIMITED && previous_subtree < high && charged_subtree >= high {
        note_event_locked(node, EventKind::High);
    }
    let low = node.low.load(Ordering::Acquire);
    if low != 0 && previous_subtree < low && charged_subtree >= low {
        note_event_locked(node, EventKind::Low);
    }
    Ok(())
}

fn charge_bytes_class(node: &Arc<Memcg>, bytes: u64, class: ChargeClass) -> KResult<()> {
    if bytes == 0 {
        return Ok(());
    }
    let failed = {
        let _guard = CHARGE_LOCK.lock();
        charge_locked(node, node, bytes, class).err()
    };
    if let Some(id) = failed {
        if let Some(handler) = *OOM_HANDLER.lock() {
            handler(id);
        }
        return Err(KError::NoMemory);
    }
    Ok(())
}

#[cfg(unittest)]
fn uncharge_bytes(node: &Arc<Memcg>, bytes: u64) {
    uncharge_bytes_class(node, bytes, ChargeClass::Anonymous);
}

fn charge_bytes(node: &Arc<Memcg>, bytes: u64) -> KResult<()> {
    charge_bytes_class(node, bytes, ChargeClass::Anonymous)
}

fn uncharge_bytes_class(node: &Arc<Memcg>, bytes: u64, class: ChargeClass) {
    if bytes == 0 {
        return;
    }
    let _guard = CHARGE_LOCK.lock();
    uncharge_locked(node, node, bytes, class);
}

/// Charges `bytes` held by `paddr` to the current memory cgroup.
///
/// The page's metadata records the owner so [`uncharge_frame`] can release the
/// charge from an arbitrary later context.
pub fn charge_frame(paddr: PhysAddr, bytes: usize) -> KResult<()> {
    let id = current_memcg();
    if id.is_none() {
        return Ok(());
    }
    let Some(node) = get(id) else {
        return Err(KError::InvalidInput);
    };
    let first_page = paddr.as_usize() / PAGE_SIZE;
    let class = lookup(first_page)
        .map(|meta| ChargeClass::of(meta.usage_kind()))
        .unwrap_or(ChargeClass::Anonymous);
    let pages = bytes.div_ceil(PAGE_SIZE);
    charge_bytes_class(&node, bytes as u64, class)?;
    for page in 0..pages {
        if let Some(meta) = lookup(paddr.as_usize() / PAGE_SIZE + page) {
            meta.set_memcg(id.as_u32());
        }
    }
    Ok(())
}

/// Releases the charge recorded for `paddr`.
pub fn uncharge_frame(paddr: PhysAddr, bytes: usize) {
    let first_page = paddr.as_usize() / PAGE_SIZE;
    let class = lookup(first_page)
        .map(|meta| ChargeClass::of(meta.usage_kind()))
        .unwrap_or(ChargeClass::Anonymous);
    let pages = bytes.div_ceil(PAGE_SIZE);
    let mut owner = MemcgId::NONE;
    for page in 0..pages {
        if let Some(meta) = lookup(first_page + page) {
            let id = MemcgId::from_raw(meta.take_memcg());
            if !id.is_none() {
                owner = id;
            }
        }
    }
    if let Some(node) = get(owner) {
        uncharge_bytes_class(&node, bytes as u64, class);
    }
}

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/lib.rs");
