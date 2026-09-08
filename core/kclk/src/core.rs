// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Clock tree node, parent links, rate caching, two-phase gating state, and
//! rate-change notifications.

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};

use kspin::SpinNoIrq;
use ksync::{Mutex, static_lock};

use crate::{
    ClkError, ClkEvent, ClkFlags, ClkHwOps, ClkNotifier, ClkNotifierId, ClkRateRequest, ClkResult,
    NotifierResult, Rate,
};

// Two global locks, mirroring Linux's prepare_lock + enable_lock split.
//
// CLK_LOCK (sleepable `ksync::Mutex`) serializes the slow mutations —
// `prepare`/`unprepare`/`set_rate`/`set_parent`/`round_rate` — so a rate
// change (which may recurse up the parent tree and reprogram a PLL) is atomic
// with respect to other changes. prepare/unprepare/set_rate may sleep;
// set_parent and reparent-time recalculation also run under CLK_ENABLE_LOCK and
// therefore must not sleep.
// Never taken from atomic/interrupt context; not re-entrant (re-entering the
// framework while holding it panics instead of deadlocking).
//
// CLK_ENABLE_LOCK (IRQ-saving spinlock) serializes `enable`/`disable`
// propagation, keeping the gate fast path usable from interrupt context
// (console/polling drivers call `clk_enable` from IRQ handlers on real
// hardware). Provider `enable`/`disable` callbacks run under it and must stay
// quick and non-sleeping — they only flip a gate bit.
//
// Lock order: CLK_LOCK → CLK_ENABLE_LOCK (the reparent reference-migration
// takes both, in that order). The reverse order never occurs, so no circular
// wait is possible. The theoretical interleaving window between a concurrent
// `enable` and `set_parent`'s migration (both serialize on CLK_ENABLE_LOCK for
// their count mutations, but the parent-index update sits between them) is the
// same one Linux's clk_core_set_parent has tolerated for a decade: counts
// balance because every mutation of them holds CLK_ENABLE_LOCK, and the
// migration brings the new parent up before tearing the old one down, then
// commits the parent index/back-edge before releasing CLK_ENABLE_LOCK, so an
// IRQ-side gate operation observes either the complete old topology or the
// complete new topology.
static_lock! {
    static CLK_LOCK: Mutex<()> = Mutex::new(());
}

/// Serializes `enable`/`disable` propagation across the whole tree (Linux's
/// `clk_enable_lock`). An IRQ-saving spinlock: usable from interrupt context,
/// and holding it makes the per-CPU IRQ-off guarantee deadlock-free against
/// same-CPU handlers. Never sleep while holding it.
static CLK_ENABLE_LOCK: SpinNoIrq<()> = SpinNoIrq::new(());

/// Global list of every registered clock, used by the unused-clock gate-down
/// pass and future PM walks. Strong references mirror Linux, which never frees
/// a `clk_core` during normal operation.
static CLOCKS: SpinNoIrq<Vec<Arc<ClkCore>>> = SpinNoIrq::new(Vec::new());

/// A clock slot waiting for a parent (by name) that has not been registered
/// yet. When a clock with `parent_name` is built, the framework adopts it.
#[derive(Clone)]
struct OrphanEntry {
    child: Arc<ClkCore>,
    parent_index: u8,
    parent_name: &'static str,
}

/// Clocks waiting for a not-yet-registered parent.
static ORPHANS: SpinNoIrq<Vec<OrphanEntry>> = SpinNoIrq::new(Vec::new());

/// Mutable per-node state, guarded by a spin lock.
struct ClkState {
    enable_count: i32,
    prepare_count: i32,
    parent_index: u8,
    rate: Rate,
    accuracy: u32,
    /// Times a GET_RATE_NOCACHE recalc failed and the cached rate was served
    /// instead. Surface through clk_summary: a nonzero count means the cached
    /// rate may be stale (hardware read failing).
    recalc_failures: u32,
    /// Per-consumer rate-range votes: `(vote_id, min, max)`. The effective
    /// range is the tightest intersection across all votes (Linux aggregates
    /// per-`struct clk`). Id `0` is the legacy slot used by direct
    /// `ClkCore::set_rate_range` callers (last-wins for that id).
    votes: Vec<(u64, Rate, Rate)>,
    next_vote_id: u64,
}

/// Registered rate-change subscribers, tagged with an id for unregistration.
struct NotifierState {
    next_id: ClkNotifierId,
    entries: Vec<(ClkNotifierId, ClkNotifier)>,
}

/// A node in the clock tree.
///
/// Provider drivers construct one with [`ClkBuilder`]; the framework caches the
/// rate and accuracy and serializes the two-phase gate (`prepare`/`enable`)
/// with reference counts that propagate up to the selected parent.
///
/// The node is cheap to share: all mutable state lives behind a
/// [`SpinNoIrq`], and the parent table holds strong references so a consumer
/// can never dereference a freed ancestor.
pub struct ClkCore {
    name: &'static str,
    flags: ClkFlags,
    ops: Arc<dyn ClkHwOps>,
    /// Parent table. Interior-mutable so an orphan slot (`None`) can be filled
    /// later when its named parent is registered.
    parents: SpinNoIrq<Vec<Option<Arc<ClkCore>>>>,
    /// Children whose *currently selected* parent is this node. Weak so a
    /// parent does not keep a dropped child alive; maintained by
    /// [`relink_selected`](Self::relink_selected).
    children: SpinNoIrq<Vec<Weak<ClkCore>>>,
    /// Weak back-reference to this node, used to push `self` into a parent's
    /// `children` list from a `&self` method. Set once in [`ClkBuilder::build`].
    self_ref: SpinNoIrq<Option<Weak<ClkCore>>>,
    state: SpinNoIrq<ClkState>,
    notifiers: SpinNoIrq<NotifierState>,
}

impl ClkCore {
    /// Human-readable name of this clock.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Policy flags configured at registration.
    pub fn flags(&self) -> ClkFlags {
        self.flags
    }

    /// Cached rate, in Hz.
    ///
    /// When [`ClkFlags::GET_RATE_NOCACHE`] is set, the hardware rate is
    /// re-read (and the cache refreshed) instead of trusting the cached value.
    pub fn get_rate(&self) -> Rate {
        if self.flags.contains(ClkFlags::GET_RATE_NOCACHE) {
            let parent_rate = self.parent_rate();
            match self.ops.recalc_rate(parent_rate) {
                Ok(rate) => {
                    self.state.lock().rate = rate;
                    return rate;
                }
                // A failed hardware read must not pass silently: count it so
                // clk_summary can flag a potentially stale cache, then serve
                // the last cached value (there is no valid "error rate").
                Err(_) => {
                    self.state.lock().recalc_failures += 1;
                }
            }
        }
        self.state.lock().rate
    }

    /// Cached accuracy, in parts per billion.
    pub fn get_accuracy(&self) -> u32 {
        self.state.lock().accuracy
    }

    /// Clamp `rate` into this clock's effective `[min, max]` range — the
    /// tightest intersection of all consumer rate-range votes.
    fn clamp_rate(&self, rate: Rate) -> Rate {
        let (min, max) = self.effective_range();
        rate.clamp(min, max)
    }

    /// The tightest intersection of all consumer rate-range votes, or
    /// `(0, u64::MAX)` if none. Id `0` is the legacy slot used by direct
    /// `set_rate_range` callers.
    fn effective_range(&self) -> (Rate, Rate) {
        let st = self.state.lock();
        if st.votes.is_empty() {
            return (0, Rate::MAX);
        }
        let min = st.votes.iter().map(|(_, lo, _)| *lo).max().unwrap_or(0);
        let max = st
            .votes
            .iter()
            .map(|(_, _, hi)| *hi)
            .min()
            .unwrap_or(Rate::MAX);
        (min, max.max(min))
    }

    /// Insert or update the vote `id` to `(min, max)`. The caller validates
    /// `min <= max` and holds `CLK_LOCK`.
    ///
    /// Rejects a vote whose intersection with the other votes would be empty
    /// (e.g. [100,200] vs [300,400]) instead of clamping the effective range
    /// into a bogus single point — an unsatisfiable constraint must surface as
    /// an error, leaving the previous votes untouched.
    pub(crate) fn set_rate_vote(&self, id: u64, min: Rate, max: Rate) -> ClkResult {
        let mut st = self.state.lock();
        // Preview the effective range with this vote applied: fold over the
        // *other* votes (this id's old value, if any, is replaced) plus the
        // new one.
        let lo = st
            .votes
            .iter()
            .filter(|(vid, ..)| *vid != id)
            .map(|(_, vlo, _)| *vlo)
            .chain(core::iter::once(min))
            .max()
            .unwrap_or(0);
        let hi = st
            .votes
            .iter()
            .filter(|(vid, ..)| *vid != id)
            .map(|(_, _, vhi)| *vhi)
            .chain(core::iter::once(max))
            .min()
            .unwrap_or(Rate::MAX);
        if lo > hi {
            return Err(ClkError::InvalidRate);
        }
        if let Some(v) = st.votes.iter_mut().find(|(vid, ..)| *vid == id) {
            v.1 = min;
            v.2 = max;
        } else {
            st.votes.push((id, min, max));
        }
        Ok(())
    }

    /// Remove the vote `id` (a consumer going away).
    pub(crate) fn remove_rate_vote(&self, id: u64) {
        let _guard = CLK_LOCK.lock();
        self.state.lock().votes.retain(|(vid, ..)| *vid != id);
    }

    /// Allocate a fresh per-consumer vote id (>= 1).
    pub(crate) fn allocate_vote_id(&self) -> u64 {
        let mut st = self.state.lock();
        let id = st.next_vote_id.max(1);
        st.next_vote_id = id + 1;
        id
    }

    /// Constrain this clock's rate to `[min, max]` — internal/test entry on
    /// the reserved id-0 slot. Production consumers use
    /// [`ClkHandle::set_rate_range`](crate::ClkHandle::set_rate_range), whose
    /// per-consumer votes aggregate (intersect) instead of overwriting this
    /// single slot. Exposed `pub(crate)` only so tests can exercise clamping
    /// without a handle; there is deliberately no public core-level path.
    #[cfg(unittest)]
    pub(crate) fn set_rate_range(&self, min: Rate, max: Rate) -> ClkResult {
        if min > max {
            return Err(ClkError::InvalidRate);
        }
        let _guard = CLK_LOCK.lock();
        self.set_rate_vote(0, min, max)
    }

    /// The current effective `(min, max)` rate range (tightest intersection of
    /// all consumer votes + the legacy slot).
    pub fn get_rate_range(&self) -> (Rate, Rate) {
        self.effective_range()
    }

    /// The currently selected parent node, as a cloned [`Arc`].
    ///
    /// Cloning the [`Arc`] (cheap, refcount-only) lets callers recurse into the
    /// parent without holding this node's state lock, which would otherwise
    /// risk a lock-ordering deadlock during rate propagation.
    fn parent_node(&self) -> Option<Arc<ClkCore>> {
        let idx = self.state.lock().parent_index as usize;
        self.parents.lock().get(idx).and_then(Option::clone)
    }

    /// Rate of the selected parent, or `0` for a root.
    fn parent_rate(&self) -> Rate {
        self.parent_node().map(|p| p.get_rate()).unwrap_or(0)
    }

    /// Fill parent slot `index` without touching hardware.
    ///
    /// Used by the table-driven registrar to resolve a name-referenced parent
    /// after all nodes exist. If the slot is the currently selected parent,
    /// the parent→child back-edge is moved to the new parent. The caller holds
    /// `CLK_LOCK`.
    fn set_parent_slot(&self, index: u8, parent: Arc<ClkCore>) {
        let selected = self.state.lock().parent_index == index;
        let prev = {
            let mut table = self.parents.lock();
            let prev = if selected {
                table.get(index as usize).and_then(Option::clone)
            } else {
                None
            };
            if let Some(slot) = table.get_mut(index as usize) {
                *slot = Some(parent);
            }
            prev
        };
        if selected {
            self.relink_selected(prev);
        }
    }

    /// Resolve an empty parent slot and migrate this node's active references
    /// when that slot is currently selected.
    ///
    /// Table registration creates all nodes before connecting their parent
    /// slots. A node may nevertheless already be active when it was adopted
    /// as the parent of a pre-existing orphan. Merely filling its selected
    /// slot would then leave its prepare/enable references stranded at the
    /// node instead of propagating them into the newly attached ancestor.
    /// The caller holds [`CLK_LOCK`].
    pub(crate) fn resolve_parent_slot_locked(&self, index: u8, parent: Arc<ClkCore>) -> ClkResult {
        debug_assert_clk_lock_held();
        let is_selected = self.state.lock().parent_index == index;
        if !is_selected {
            self.set_parent_slot(index, parent);
            return Ok(());
        }

        let parent_rate = parent.get_rate();
        let parent_accuracy = parent.get_accuracy();
        let rate = self.ops.recalc_rate(parent_rate)?;
        let accuracy = self.ops.recalc_accuracy(parent_accuracy)?;
        let prepare_refs = self.state.lock().prepare_count;
        let mut prepared = 0;
        for _ in 0..prepare_refs {
            if let Err(error) = parent.prepare_locked() {
                for _ in 0..prepared {
                    let _ = parent.unprepare_locked();
                }
                return Err(error);
            }
            prepared += 1;
        }

        let enable_guard = CLK_ENABLE_LOCK.lock();
        let enable_refs = self.state.lock().enable_count;
        for enabled in 0..enable_refs {
            if let Err(error) = parent.enable_locked() {
                for _ in 0..enabled {
                    parent.disable_locked();
                }
                drop(enable_guard);
                for _ in 0..prepared {
                    let _ = parent.unprepare_locked();
                }
                return Err(error);
            }
        }

        self.set_parent_slot(index, parent);
        {
            let mut state = self.state.lock();
            state.rate = rate;
            state.accuracy = accuracy;
        }
        drop(enable_guard);
        Ok(())
    }

    /// Move this node's parent→child back-edge from `prev` (the previously
    /// selected parent, if any) to the currently selected parent.
    ///
    /// Called after the selected parent (`parents[parent_index]`) changes —
    /// from `build`, `set_parent_slot`, and `switch_parent_locked`. The caller
    /// holds `CLK_LOCK`.
    fn relink_selected(&self, prev: Option<Arc<ClkCore>>) {
        // Unlink from the previous selected parent.
        if let Some(prev) = prev {
            let self_ptr = self as *const ClkCore;
            prev.children.lock().retain(|w| w.as_ptr() != self_ptr);
        }
        // Link to the newly selected parent (if any / resolved).
        let cur = {
            let idx = self.state.lock().parent_index as usize;
            self.parents.lock().get(idx).and_then(Option::clone)
        };
        if let Some(cur) = cur
            && let Some(self_weak) = self.self_ref.lock().as_ref().cloned()
        {
            cur.children.lock().push(self_weak);
        }
    }

    /// Number of live children (for tests/diagnostics). Prunes dangling weaks.
    pub(crate) fn children_count(&self) -> usize {
        let mut list = self.children.lock();
        list.retain(|w| w.strong_count() > 0);
        list.len()
    }

    /// Recompute and refresh the cached rate from the currently selected parent.
    ///
    /// No hardware write — only `recalc_rate`, matching the non-sleeping
    /// contract. Used by the table-driven registrar for the top-down rate
    /// refresh once parents are resolved. The caller holds `CLK_LOCK`.
    pub(crate) fn refresh_rate(&self) {
        let parent_rate = self.parent_rate();
        if let Ok(rate) = self.ops.recalc_rate(parent_rate) {
            self.state.lock().rate = rate;
        }
    }

    /// Walk children top-down and refresh each cached rate from its (now
    /// current) parent rate. Called after this node's rate changed (via
    /// `set_rate` or `set_parent`), so descendants that derive from it pick up
    /// the new value instead of staying stale. The caller holds `CLK_LOCK`.
    fn refresh_descendants(&self) {
        // Snapshot live children under the lock, then recurse outside it so a
        // child's refresh (which takes its own locks) does not nest this node's
        // children lock.
        let children: Vec<Arc<ClkCore>> = self
            .children
            .lock()
            .iter()
            .filter_map(|w| w.upgrade())
            .collect();
        for child in children {
            child.refresh_rate();
            child.refresh_descendants();
        }
    }

    /// The currently selected parent node, as a cloned [`Arc`], or `None` for
    /// a root (or an orphan slot with no registered parent).
    pub fn get_parent(&self) -> Option<Arc<ClkCore>> {
        self.parent_node()
    }

    /// Register a rate-change subscriber on this clock.
    ///
    /// Returns an id that can be passed to [`remove_notifier`](Self::remove_notifier)
    /// to unregister the subscriber.
    pub fn add_notifier(&self, cb: ClkNotifier) -> ClkNotifierId {
        let mut st = self.notifiers.lock();
        let id = st.next_id;
        st.next_id += 1;
        st.entries.push((id, cb));
        id
    }

    /// Remove a previously registered subscriber by id.
    pub fn remove_notifier(&self, id: ClkNotifierId) {
        self.notifiers.lock().entries.retain(|(nid, _)| *nid != id);
    }

    /// Snapshot the notifier list so callbacks can be invoked without holding
    /// the notifier lock (a callback that, say, queries `get_rate` would
    /// otherwise self-deadlock).
    fn snapshot_notifiers(&self) -> Vec<ClkNotifier> {
        self.notifiers
            .lock()
            .entries
            .iter()
            .map(|(_, cb)| cb.clone())
            .collect()
    }

    /// Deliver `PreRateChange`; returns `false` if any subscriber vetoed.
    fn notify_pre(&self, old: Rate, new: Rate) -> bool {
        for cb in self.snapshot_notifiers() {
            if cb(&ClkEvent::PreRateChange { old, new }) == NotifierResult::Stop {
                return false;
            }
        }
        true
    }

    fn notify_post(&self, old: Rate, new: Rate) {
        for cb in self.snapshot_notifiers() {
            cb(&ClkEvent::PostRateChange { old, new });
        }
    }

    fn notify_aborted(&self, old: Rate, attempted: Rate) {
        for cb in self.snapshot_notifiers() {
            cb(&ClkEvent::RateChangeAborted { old, attempted });
        }
    }

    // ---- two-phase gating ----

    /// Prepare the slow path, propagating to the parent first.
    ///
    /// Serialized by the global [`CLK_LOCK`]; a hardware failure rolls the
    /// parent back to its previous count.
    pub fn prepare(&self) -> ClkResult {
        let _guard = CLK_LOCK.lock();
        self.prepare_locked()
    }

    pub(crate) fn prepare_locked(&self) -> ClkResult {
        debug_assert_clk_lock_held();
        let parent_done = match self.parent_node() {
            Some(parent) => {
                parent.prepare_locked()?;
                true
            }
            None => false,
        };

        // Read the count under the lock, then drop it: the `prepare` callback
        // is sleepable (it runs under `CLK_LOCK`), so it must not run with the
        // per-node `SpinNoIrq` held — that would block with IRQs disabled.
        // `CLK_LOCK` serializes this whole path, so no other task can touch
        // `prepare_count` between the read and the increment.
        let need_prepare = self.state.lock().prepare_count == 0;
        if need_prepare && let Err(err) = self.ops.prepare() {
            if parent_done && let Some(parent) = self.parent_node() {
                let _ = parent.unprepare_locked();
            }
            return Err(err);
        }
        self.state.lock().prepare_count += 1;
        Ok(())
    }

    /// Release one prepare reference, then propagate to the parent.
    ///
    /// Returns the hardware `unprepare` error, if any: the software count is
    /// already decremented (reflecting the consumer's release), so a caller
    /// receiving an error knows the hardware may still be prepared while the
    /// framework considers it unprepared — the mismatch is surfaced rather than
    /// silently dropped (Linux `clk_unprepare` reports it the same way).
    pub fn unprepare(&self) -> ClkResult {
        let _guard = CLK_LOCK.lock();
        self.unprepare_locked()
    }

    /// Returns the last hardware `unprepare` error encountered while unwinding
    /// this node's chain (parent errors are included), or `Ok(())`.
    pub(crate) fn unprepare_locked(&self) -> ClkResult {
        debug_assert_clk_lock_held();
        // Decrement under the lock, then drop it before the sleepable op —
        // same rationale as `prepare_locked`: `unprepare` may sleep and must
        // not run with `SpinNoIrq` held.
        let need_unprepare = {
            let mut st = self.state.lock();
            if st.prepare_count == 0 {
                return Ok(());
            }
            st.prepare_count -= 1;
            st.prepare_count == 0
        };
        let mut own_err = Ok(());
        if need_unprepare {
            own_err = self.ops.unprepare();
        }
        let parent_err = if let Some(parent) = self.parent_node() {
            parent.unprepare_locked()
        } else {
            Ok(())
        };
        own_err.and(parent_err)
    }

    /// Enable the fast path, propagating to the parent first.
    ///
    /// Serialized by [`CLK_ENABLE_LOCK`] — an IRQ-saving spinlock, mirroring
    /// Linux's `clk_enable_lock` — so it stays usable from interrupt context
    /// (console/polling drivers) while still being mutually exclusive with
    /// `set_parent`'s count migration. The provider `enable` callback runs
    /// under it and must be quick and non-sleeping (it only flips a gate bit).
    pub fn enable(&self) -> ClkResult {
        let _guard = CLK_ENABLE_LOCK.lock();
        self.enable_locked()
    }

    /// Internal enable; the caller holds [`CLK_ENABLE_LOCK`]. Propagates to
    /// the parent first and rolls it back on a hardware failure.
    pub(crate) fn enable_locked(&self) -> ClkResult {
        // Linux __clk_enable WARN semantics: enabling an unprepared clock is
        // consumer misuse — hardware needs prepare (PLL settle, async
        // unlocks) before it can be safely ungated. kclk (like the other
        // core-service crates) has no logging dependency, so this surfaces
        // as a debug-build assertion rather than a runtime warning.
        debug_assert!(
            self.state.lock().prepare_count > 0,
            "kclk: enabling unprepared clock \"{}\" — call prepare() first",
            self.name
        );

        let parent_done = match self.parent_node() {
            Some(parent) => {
                parent.enable_locked()?;
                true
            }
            None => false,
        };

        let need_enable = self.state.lock().enable_count == 0;
        if need_enable && let Err(err) = self.ops.enable() {
            if parent_done && let Some(parent) = self.parent_node() {
                parent.disable_locked();
            }
            return Err(err);
        }
        self.state.lock().enable_count += 1;
        Ok(())
    }

    /// Release one enable reference, then propagate to the parent.
    ///
    /// Serialized by [`CLK_ENABLE_LOCK`]; see [`enable`](Self::enable).
    pub fn disable(&self) {
        let _guard = CLK_ENABLE_LOCK.lock();
        self.disable_locked();
    }

    /// Internal disable; the caller holds [`CLK_ENABLE_LOCK`].
    pub(crate) fn disable_locked(&self) {
        let need_disable = {
            let mut st = self.state.lock();
            if st.enable_count == 0 {
                return;
            }
            st.enable_count -= 1;
            st.enable_count == 0
        };
        if need_disable {
            self.ops.disable();
        }
        if let Some(parent) = self.parent_node() {
            parent.disable_locked();
        }
    }

    /// Whether the hardware reports the clock as enabled.
    pub fn is_enabled(&self) -> ClkResult<bool> {
        self.ops.is_enabled()
    }

    /// Whether the hardware reports the clock as prepared.
    pub fn is_prepared(&self) -> ClkResult<bool> {
        self.ops.is_prepared()
    }

    /// `(prepare_count, enable_count)` — for tests and diagnostics.
    pub(crate) fn counts(&self) -> (i32, i32) {
        let st = self.state.lock();
        (st.prepare_count, st.enable_count)
    }

    /// Invariant check: does the cached rate still equal what `recalc_rate`
    /// would derive from the (current) parent rate? Roots are trivially
    /// coherent. For tests/diagnostics.
    pub(crate) fn cache_coherent(&self) -> bool {
        let Some(p) = self.parent_node() else {
            return true;
        };
        match self.ops.recalc_rate(p.get_rate()) {
            Ok(expected) => expected == self.state.lock().rate,
            Err(_) => true,
        }
    }

    // ---- rate ----

    /// Compute the closest achievable rate for `rate` without changing state.
    ///
    /// Serialized by [`CLK_LOCK`]. If this node asks for a parent change
    /// (`parent_target`) and carries [`ClkFlags::SET_RATE_PARENT`], the request
    /// is forwarded to the parent and this node is re-rounded against the new
    /// parent rate — so a divider behind a mux can still be satisfied by
    /// changing the mux's selected source.
    pub fn round_rate(&self, rate: Rate) -> ClkResult<Rate> {
        let _guard = CLK_LOCK.lock();
        self.round_rate_locked(rate)
    }

    /// Run the provider's `determine_rate` to produce a rate decision for `rate`.
    ///
    /// Seeds the request with the clamped request rate and the currently
    /// selected parent, then hands it (with the candidate parent list) to the
    /// provider. Serialized by [`CLK_LOCK`] like every sleepable mutation.
    fn determine_rate_locked(&self, rate: Rate) -> ClkResult<ClkRateRequest> {
        let rate = self.clamp_rate(rate);
        let (min, max, current) = {
            let st = self.state.lock();
            // Effective range = tightest intersection of all votes (inline to
            // avoid re-entering the state lock via effective_range()).
            let (min, max) = if st.votes.is_empty() {
                (0, Rate::MAX)
            } else {
                let lo = st.votes.iter().map(|(_, lo, _)| *lo).max().unwrap_or(0);
                let hi = st
                    .votes
                    .iter()
                    .map(|(_, _, hi)| *hi)
                    .min()
                    .unwrap_or(Rate::MAX);
                (lo, hi.max(lo))
            };
            (min, max, st.parent_index)
        };
        let current_rate = self.parent_rate();

        let parents: Vec<(u8, Rate)> = {
            let table = self.parents.lock();
            let entries: Vec<(u8, Arc<ClkCore>)> = table
                .iter()
                .enumerate()
                .filter_map(|(i, p)| p.as_ref().map(|pc| (i as u8, pc.clone())))
                .collect();
            entries
                .into_iter()
                .map(|(i, pc)| (i, pc.get_rate()))
                .collect()
        };

        let mut req = ClkRateRequest {
            rate,
            min_rate: min,
            max_rate: max,
            best_parent_index: Some(current),
            best_parent_rate: current_rate,
            parent_target: None,
        };
        self.ops.determine_rate(&mut req, &parents)?;
        req.rate = req.rate.clamp(min, max);
        Ok(req)
    }

    fn round_rate_locked(&self, rate: Rate) -> ClkResult<Rate> {
        debug_assert_clk_lock_held();
        let req = self.determine_rate_locked(rate)?;
        if let Some(target) = req
            .parent_target
            .filter(|_| self.flags.contains(ClkFlags::SET_RATE_PARENT))
            && let Some(parent) = self.parent_node()
        {
            let new_parent = parent.round_rate_locked(target)?;
            return self.ops.round_rate(req.rate, new_parent).map(|r| r.rate);
        }
        Ok(req.rate)
    }

    /// Program a new rate and return the rate now in effect.
    ///
    /// Serialized by [`CLK_LOCK`]; rate-change [`ClkEvent`]s are delivered to
    /// this node's subscribers around the change (a `Pre` veto aborts with
    /// [`ClkError::Aborted`]).
    ///
    /// When this node asks for a parent change and carries
    /// [`ClkFlags::SET_RATE_PARENT`], the request is forwarded to the parent
    /// (reprogramming it) and this node is recalculated. Only this node's
    /// notifiers fire; intermediate parents changed during propagation do not
    /// receive separate notifications.
    pub fn set_rate(&self, rate: Rate) -> ClkResult<Rate> {
        let _guard = CLK_LOCK.lock();
        let rate = self.clamp_rate(rate);
        let old = self.get_rate();

        if !self.notify_pre(old, rate) {
            self.notify_aborted(old, rate);
            return Err(ClkError::Aborted);
        }

        match self.set_rate_locked(rate) {
            Ok(actual) => {
                self.notify_post(old, actual);
                Ok(actual)
            }
            Err(err) => {
                self.notify_aborted(old, rate);
                Err(err)
            }
        }
    }

    fn set_rate_locked(&self, rate: Rate) -> ClkResult<Rate> {
        debug_assert_clk_lock_held();
        // SET_RATE_GATE (Linux CLK_SET_RATE_GATE): the clock must not be
        // reprogrammed while enabled — reject with Busy instead of the
        // glitch-prone disable/change/re-enable dance, whose re-enable failure
        // would leave enable_count > 0 on gated hardware (permanently off).
        if self.flags.contains(ClkFlags::SET_RATE_GATE) && self.state.lock().enable_count > 0 {
            return Err(ClkError::Busy);
        }

        let req = self.determine_rate_locked(rate)?;
        let actual = self.program_rate_locked(&req)?;
        self.state.lock().rate = actual;
        // This node's rate changed — refresh descendants that derive from it.
        self.refresh_descendants();
        Ok(actual)
    }

    /// Apply the rate decision in `rounded`. Honors `SET_RATE_PARENT`: when the
    /// node asked for a parent change, the parent is reprogrammed first and
    /// this node's new rate is reported via [`ClkHwOps::recalc_rate`].
    ///
    /// Takes the already-computed [`RoundResult`] so the op is rounded exactly
    /// once (never re-queried), and so the parent_target decided upstream is the
    /// one honored here.
    ///
    /// Per the `round_rate` contract, a node that returns `parent_target`
    /// declares its rate is determined by the parent; the framework therefore
    /// reports it with `recalc_rate`, never `set_rate`, on this branch.
    fn program_rate_locked(&self, req: &ClkRateRequest) -> ClkResult<Rate> {
        debug_assert_clk_lock_held();
        // Lateral: switch to the parent `determine_rate` selected, if different.
        let current = self.state.lock().parent_index;
        let mut reparented = false;
        if let Some(idx) = req.best_parent_index.filter(|&i| i != current) {
            self.switch_parent_locked(idx)?;
            reparented = true;
        }
        let parent_rate = self.parent_rate();

        let result = if let Some(target) = req
            .parent_target
            .filter(|_| self.flags.contains(ClkFlags::SET_RATE_PARENT))
        {
            // Upward: the node asks its selected parent to change rate.
            let parent = self.parent_node().ok_or(ClkError::NotSupported)?;
            let old_parent_rate = parent.get_rate();
            match parent.set_rate_locked(target) {
                Ok(actual_parent) => match self.ops.recalc_rate(actual_parent) {
                    Ok(rate) => Ok(rate),
                    Err(error) => {
                        // The child could not accept the newly programmed
                        // parent rate. Restore the upstream transaction before
                        // reporting an aborted change to this node's consumer.
                        match parent.set_rate_locked(old_parent_rate) {
                            Ok(restored) if restored == old_parent_rate => Err(error),
                            Ok(_) | Err(_) => Err(ClkError::BadState),
                        }
                    }
                },
                Err(error) => Err(error),
            }
        } else {
            // Own rate field. A transparent node (no `set_rate`) only
            // succeeds by reporting its parent's rate after a source switch.
            match self.ops.set_rate(req.rate, parent_rate) {
                Ok(actual) => Ok(actual),
                Err(ClkError::NotSupported) if reparented => self.ops.recalc_rate(parent_rate),
                Err(error) => Err(error),
            }
        };

        if result.is_err() && reparented && self.switch_parent_locked(current).is_err() {
            // The primary operation and its topology rollback both failed;
            // report the stronger invariant failure rather than pretending
            // the requested rate was the only failed effect.
            return Err(ClkError::BadState);
        }
        result
    }

    // ---- parent selection ----

    /// Select parent `index` and refresh the cached rate and accuracy.
    ///
    /// Serialized by [`CLK_LOCK`]. The new parent must already be registered at
    /// `index`. When [`ClkFlags::SET_PARENT_GATE`] is set and the clock is
    /// prepared, the switch is rejected with [`Busy`](crate::ClkError::Busy)
    /// (Linux `CLK_SET_PARENT_GATE` semantics).
    pub fn set_parent(&self, index: u8) -> ClkResult {
        let _guard = CLK_LOCK.lock();
        // SET_PARENT_GATE (Linux CLK_SET_PARENT_GATE): reject the mux switch
        // while the clock is prepared, instead of a disable/switch/re-enable
        // dance (whose re-enable failure would strand enabled-count on gated
        // HW).
        if self.flags.contains(ClkFlags::SET_PARENT_GATE) && self.state.lock().prepare_count > 0 {
            return Err(ClkError::Busy);
        }
        self.switch_parent_locked(index)
    }

    /// Raw parent switch plus cache refresh.
    ///
    /// The caller must hold [`CLK_LOCK`] and has already enforced any relevant
    /// rate/parent gating policy.
    fn switch_parent_locked(&self, index: u8) -> ClkResult {
        debug_assert_clk_lock_held();
        // Resolve the new parent and capture the previously selected parent
        // (to move the parent→child back-edge + prepare/enable refs after the
        // switch).
        let (new_parent, prev) = {
            let table = self.parents.lock();
            if index as usize >= table.len() {
                return Err(ClkError::InvalidParent);
            }
            // Reject switching to an orphan slot: recalculating against a
            // missing parent would silently produce a zero rate.
            let Some(parent) = table[index as usize].clone() else {
                return Err(ClkError::InvalidParent);
            };
            let old_idx = self.state.lock().parent_index as usize;
            let prev = table.get(old_idx).and_then(Option::clone);
            (parent, prev)
        };
        // Prepare references are serialized by CLK_LOCK and can be migrated
        // before entering the atomic part of the transition. Enable references
        // are sampled only after CLK_ENABLE_LOCK is held; otherwise an IRQ-side
        // enable could race the sample and remain attached to the old parent.
        let prep = self.state.lock().prepare_count;
        // 1. Bring the new parent up BEFORE the switch (new first, then old
        //    down) so the clock does not go dark. Each step rolls back the
        //    earlier ones on failure, leaving state unchanged.
        //
        // The migration moves ALL `prep`/`en` references this node holds — a
        // node with two prepared consumers propagates two references to its
        // parent; moving only one leaves a leaked reference on the old parent
        // and an under-counted new one (caught by the concurrency stress
        // test: two gate workers + one switcher unbalanced the counters).
        let mut brought_up_prep = 0;
        for _ in 0..prep {
            if let Err(error) = new_parent.prepare_locked() {
                for _ in 0..brought_up_prep {
                    let _ = new_parent.unprepare_locked();
                }
                return Err(error);
            }
            brought_up_prep += 1;
        }
        // From this point through the hardware mux write and parent-index
        // commit, enable/disable cannot observe a half-transitioned topology.
        // `ClkHwOps::set_parent`, recalc callbacks, and enable callbacks are
        // atomic-context operations and must not sleep.
        let enable_guard = CLK_ENABLE_LOCK.lock();
        let en = self.state.lock().enable_count;
        let mut brought_up_en = 0;
        if en > 0 {
            for _ in 0..en {
                if let Err(e) = new_parent.enable_locked() {
                    for _ in 0..brought_up_en {
                        new_parent.disable_locked();
                    }
                    drop(enable_guard);
                    for _ in 0..brought_up_prep {
                        let _ = new_parent.unprepare_locked();
                    }
                    return Err(e);
                }
                brought_up_en += 1;
            }
        }
        // 2. Compute the new rate/accuracy BEFORE writing the hardware mux, so a
        //    recalc failure does not leave the hardware switched while the
        //    cached parent_index lags (P0-3d). Roll back the new-parent
        //    migration on failure.
        let new_rate = self.ops.recalc_rate(new_parent.get_rate());
        let new_accuracy = self.ops.recalc_accuracy(new_parent.get_accuracy());
        let (new_rate, new_accuracy) = match (new_rate, new_accuracy) {
            (Ok(r), Ok(a)) => (r, a),
            (Err(e), _) | (_, Err(e)) => {
                for _ in 0..brought_up_en {
                    new_parent.disable_locked();
                }
                drop(enable_guard);
                for _ in 0..brought_up_prep {
                    let _ = new_parent.unprepare_locked();
                }
                return Err(e);
            }
        };
        // 3. Switch the hardware mux; roll back on failure.
        if let Err(e) = self.ops.set_parent(index) {
            for _ in 0..brought_up_en {
                new_parent.disable_locked();
            }
            drop(enable_guard);
            for _ in 0..brought_up_prep {
                let _ = new_parent.unprepare_locked();
            }
            return Err(e);
        }
        // 4. Tear the old parent down AFTER the successful switch — all
        //    `en`/`prep` references that were migrated up in step 1.
        if let Some(old) = prev.clone() {
            for _ in 0..en {
                old.disable_locked();
            }
        }
        // 5. Commit the topology while enable/disable is still excluded.
        {
            let mut st = self.state.lock();
            st.parent_index = index;
            st.rate = new_rate;
            st.accuracy = new_accuracy;
        }
        self.relink_selected(prev.clone());
        drop(enable_guard);
        // Slow unprepare callbacks run outside the spinlock. CLK_LOCK still
        // prevents another topology or prepare transition from interleaving.
        if let Some(old) = prev {
            for _ in 0..prep {
                let _ = old.unprepare_locked();
            }
        }
        self.refresh_descendants();
        Ok(())
    }
}

/// Builder for registering a [`ClkCore`].
///
/// Constructed with [`ClkBuilder::new`], configured with the chainable
/// setters, and finalized with [`ClkBuilder::build`], which queries the
/// hardware for the initial rate and accuracy.
pub struct ClkBuilder {
    name: &'static str,
    flags: ClkFlags,
    ops: Arc<dyn ClkHwOps>,
    parents: Vec<Option<Arc<ClkCore>>>,
    /// `(parent-slot-index, parent-name)` pairs awaiting a deferred parent.
    pending_parents: Vec<(u8, &'static str)>,
}

impl ClkBuilder {
    /// Start a node with the given name and hardware operations.
    pub fn new(name: &'static str, ops: Arc<dyn ClkHwOps>) -> Self {
        Self {
            name,
            flags: ClkFlags::empty(),
            ops,
            parents: Vec::new(),
            pending_parents: Vec::new(),
        }
    }

    /// Set policy flags.
    pub fn flags(mut self, flags: ClkFlags) -> Self {
        self.flags = flags;
        self
    }

    /// Append a single selectable parent.
    pub fn parent(mut self, parent: Arc<ClkCore>) -> Self {
        self.parents.push(Some(parent));
        self
    }

    /// Declare a selectable parent by name.
    ///
    /// The slot is left as an orphan (`None`); when a clock named `name` is
    /// later registered, the framework fills the slot and refreshes this node's
    /// cached rate. This supports probe-order-independent registration.
    pub fn parent_name(mut self, name: &'static str) -> Self {
        let index = self.parents.len() as u8;
        self.parents.push(None);
        self.pending_parents.push((index, name));
        self
    }

    /// Replace the whole parent table.
    ///
    /// Entries may be [`None`] to model a parent that has not been registered
    /// yet (an orphan), matching Linux's `clk_register` semantics.
    pub fn parents(mut self, parents: Vec<Option<Arc<ClkCore>>>) -> Self {
        self.parents = parents;
        self
    }

    /// Finalize and register the node.
    ///
    /// The initial rate and accuracy are computed by querying the hardware
    /// (`recalc_rate` / `recalc_accuracy`) against the selected parent.
    pub fn build(self) -> ClkResult<Arc<ClkCore>> {
        let Self {
            name,
            flags,
            ops,
            parents,
            pending_parents,
        } = self;
        let parent_index = ops.get_parent().unwrap_or(0);
        // Reject a parent index outside the table (except a root with no
        // parents); an out-of-range index would otherwise be silently treated
        // as a root and produce a zero rate.
        if !parents.is_empty() && parent_index as usize >= parents.len() {
            return Err(ClkError::InvalidParent);
        }
        let (parent_rate, parent_accuracy) = parents
            .get(parent_index as usize)
            .and_then(Option::clone)
            .map(|p| (p.get_rate(), p.get_accuracy()))
            .unwrap_or((0, 0));
        let rate = ops.recalc_rate(parent_rate)?;
        let accuracy = ops.recalc_accuracy(parent_accuracy)?;
        let core = Arc::new(ClkCore {
            name,
            flags,
            ops,
            parents: SpinNoIrq::new(parents),
            children: SpinNoIrq::new(Vec::new()),
            self_ref: SpinNoIrq::new(None),
            state: SpinNoIrq::new(ClkState {
                enable_count: 0,
                prepare_count: 0,
                parent_index,
                rate,
                accuracy,
                recalc_failures: 0,
                votes: Vec::new(),
                next_vote_id: 1,
            }),
            notifiers: SpinNoIrq::new(NotifierState {
                // Ids start at 1; 0 is the reserved "no notifier" sentinel used
                // by ClkHandle's automatic unregister tracking.
                next_id: 1,
                entries: Vec::new(),
            }),
        });
        // Self back-reference, then link to the initial selected parent.
        *core.self_ref.lock() = Some(Arc::downgrade(&core));
        core.relink_selected(None);
        // Post-registration hook (Linux `clk_ops.init`). Runs before the clock
        // is published so a failing init aborts registration.
        core.ops.init()?;
        // Adopt existing children before publishing this node. Adoption is a
        // transaction: if their active references cannot be migrated, build
        // fails and the children remain retryable orphans.
        adopt_orphans(core.name(), core.clone())?;
        // Register this node's own deferred-parent slots only after adoption
        // succeeds, so a failed build cannot strand entries referencing an
        // unpublished node. The push runs under CLK_LOCK, the same lock
        // `adopt_orphans` holds while draining ORPHANS: an unlocked push
        // racing a concurrent adoption could slot in just after the scan and
        // never be adopted (a permanently orphaned child whose parent
        // registered all along). The entry is deliberately not resolved
        // against CLOCKS here — adoption is triggered by *registration* (the
        // publish path below), and a same-named node may already exist from
        // a prior registration without being this child's intended parent.
        with_clk_lock(|| {
            for (index, pname) in pending_parents {
                ORPHANS.lock().push(OrphanEntry {
                    child: core.clone(),
                    parent_index: index,
                    parent_name: pname,
                });
            }
        });
        CLOCKS.lock().push(core.clone());
        Ok(core)
    }
}

/// Fill every orphan slot that names `name` with `parent`, refreshing the
/// orphan's cached rate if it currently selects that slot.
///
/// Called from [`ClkBuilder::build`] whenever a clock is registered. Descendant
/// clocks built against a stale (zero) orphan rate are *not* recursively
/// refreshed; register children after their parents, or re-run `recalc` on the
/// affected subtree, matching Linux's per-node orphan adoption.
///
/// Holds `CLK_LOCK` so the orphan's reparenting and cached-rate refresh are
/// atomic with respect to concurrent `set_rate`/`set_parent`/`prepare`, which
/// also mutate rate state under `CLK_LOCK`.
fn adopt_orphans(name: &'static str, parent: Arc<ClkCore>) -> ClkResult {
    let _guard = CLK_LOCK.lock();
    let matched: Vec<OrphanEntry> = {
        let mut orphans = ORPHANS.lock();
        let mut matched = Vec::new();
        let mut i = 0;
        while i < orphans.len() {
            if orphans[i].parent_name == name {
                matched.push(orphans.remove(i));
            } else {
                i += 1;
            }
        }
        matched
    };

    struct AdoptionState {
        entry_index: usize,
        enable_refs: i32,
        rate: Rate,
        accuracy: u32,
    }

    let mut states = Vec::new();
    let mut prepared_refs = 0;
    for (entry_index, entry) in matched.iter().enumerate() {
        if entry.child.state.lock().parent_index != entry.parent_index {
            continue;
        }
        let prepare_refs = entry.child.state.lock().prepare_count;
        for _ in 0..prepare_refs {
            if let Err(error) = parent.prepare_locked() {
                for _ in 0..prepared_refs {
                    let _ = parent.unprepare_locked();
                }
                ORPHANS.lock().extend(matched.iter().cloned());
                return Err(error);
            }
            prepared_refs += 1;
        }
        let rate = match entry.child.ops.recalc_rate(parent.get_rate()) {
            Ok(rate) => rate,
            Err(error) => {
                for _ in 0..prepared_refs {
                    let _ = parent.unprepare_locked();
                }
                ORPHANS.lock().extend(matched.iter().cloned());
                return Err(error);
            }
        };
        let accuracy = match entry.child.ops.recalc_accuracy(parent.get_accuracy()) {
            Ok(accuracy) => accuracy,
            Err(error) => {
                for _ in 0..prepared_refs {
                    let _ = parent.unprepare_locked();
                }
                ORPHANS.lock().extend(matched.iter().cloned());
                return Err(error);
            }
        };
        states.push(AdoptionState {
            entry_index,
            enable_refs: 0,
            rate,
            accuracy,
        });
    }

    // Exclude IRQ-side enable/disable from the reference snapshot through the
    // slot/cache commit, exactly like an ordinary parent transition.
    let enable_guard = CLK_ENABLE_LOCK.lock();
    let mut enabled_refs = 0;
    for state in &mut states {
        state.enable_refs = matched[state.entry_index].child.state.lock().enable_count;
        for _ in 0..state.enable_refs {
            if let Err(error) = parent.enable_locked() {
                for _ in 0..enabled_refs {
                    parent.disable_locked();
                }
                drop(enable_guard);
                for _ in 0..prepared_refs {
                    let _ = parent.unprepare_locked();
                }
                ORPHANS.lock().extend(matched.iter().cloned());
                return Err(error);
            }
            enabled_refs += 1;
        }
    }

    for (entry_index, entry) in matched.iter().enumerate() {
        entry
            .child
            .set_parent_slot(entry.parent_index, parent.clone());
        if let Some(state) = states.iter().find(|state| state.entry_index == entry_index) {
            let mut child_state = entry.child.state.lock();
            child_state.rate = state.rate;
            child_state.accuracy = state.accuracy;
        }
    }
    drop(enable_guard);
    for state in &states {
        matched[state.entry_index].child.refresh_descendants();
    }
    Ok(())
}

/// Run `f` under the global clock lock, exposing it to sibling helpers like
/// the table-driven registrar. Public only within the crate.
pub(crate) fn with_clk_lock<T>(f: impl FnOnce() -> T) -> T {
    let _g = CLK_LOCK.lock();
    f()
}

/// Acquire the enable-path lock (for callers already holding `CLK_LOCK`; the
/// CLK_LOCK → CLK_ENABLE_LOCK order is the only legal nesting).
pub(crate) fn enable_lock() -> kspin::SpinNoIrqGuard<'static, ()> {
    CLK_ENABLE_LOCK.lock()
}

/// Debug-build assertion that the current task holds [`CLK_LOCK`].
///
/// The `*_locked` methods document "caller holds CLK_LOCK" as a comment-only
/// contract; a mis-ordered call would silently race instead of failing loudly.
/// This turns that contract into a runtime check in debug builds (release
/// builds compile it away). Call it at the top of every `*_locked` entry that
/// assumes the lock — the cost is one atomic load of the mutex owner id.
#[cfg(debug_assertions)]
pub(crate) fn debug_assert_clk_lock_held() {
    debug_assert!(
        CLK_LOCK.is_owned_by_current(),
        "CLK_LOCK not held: a *_locked method was called without it (lock-order bug)"
    );
}

#[cfg(not(debug_assertions))]
pub(crate) fn debug_assert_clk_lock_held() {}

/// Current length of the global clock list. Useful for diagnostics and tests.
pub fn clocks_len() -> usize {
    CLOCKS.lock().len()
}

/// Remove an unpublished registration batch and restore pre-existing orphans.
pub(crate) fn rollback_nodes(nodes: &[Arc<ClkCore>]) {
    let _guard = CLK_LOCK.lock();
    let is_batch_node =
        |candidate: &Arc<ClkCore>| nodes.iter().any(|node| Arc::ptr_eq(node, candidate));

    // Scan every pre-existing clock's complete parent table. A non-selected
    // orphan slot has no parent->child back-edge, so walking only the batch
    // nodes' `children` lists would fail to restore it.
    let clocks: Vec<Arc<ClkCore>> = CLOCKS.lock().iter().cloned().collect();
    let mut restored = Vec::new();
    for child in clocks.iter().filter(|child| !is_batch_node(child)) {
        let selected_index = child.state.lock().parent_index;
        for (index, slot) in child.parents.lock().iter().enumerate() {
            let Some(parent) = slot else { continue };
            if !is_batch_node(parent) {
                continue;
            }
            let parent_index = index as u8;
            let prepare_refs = if selected_index == parent_index {
                child.state.lock().prepare_count
            } else {
                0
            };
            restored.push((child.clone(), parent.clone(), parent_index, prepare_refs));
        }
    }

    // Sample enable references, release them, and detach the slots in one
    // atomic topology transaction. Otherwise an IRQ-side enable could land on
    // the failed parent after the sample but immediately before slot removal.
    let enable_guard = CLK_ENABLE_LOCK.lock();
    for (child, parent, parent_index, _) in &restored {
        if child.state.lock().parent_index == *parent_index {
            let enable_refs = child.state.lock().enable_count;
            for _ in 0..enable_refs {
                parent.disable_locked();
            }
        }
    }
    let mut selected_children = Vec::new();
    for (child, parent, parent_index, _) in &restored {
        if let Some(slot) = child.parents.lock().get_mut(*parent_index as usize) {
            *slot = None;
        }
        parent
            .children
            .lock()
            .retain(|entry| entry.as_ptr() != Arc::as_ptr(child));
        if child.state.lock().parent_index == *parent_index {
            let rate = child.ops.recalc_rate(0);
            let accuracy = child.ops.recalc_accuracy(0);
            let mut state = child.state.lock();
            if let Ok(rate) = rate {
                state.rate = rate;
            }
            if let Ok(accuracy) = accuracy {
                state.accuracy = accuracy;
            }
            selected_children.push(child.clone());
        }
    }
    drop(enable_guard);

    // Prepare counts cannot change while CLK_LOCK is held. The slow callbacks
    // deliberately run after IRQs are restored and after the topology commit.
    for (_, parent, _, prepare_refs) in &restored {
        for _ in 0..*prepare_refs {
            let _ = parent.unprepare_locked();
        }
    }
    for (child, parent, parent_index, _) in restored {
        ORPHANS.lock().push(OrphanEntry {
            child,
            parent_index,
            parent_name: parent.name,
        });
    }
    for child in selected_children {
        child.refresh_descendants();
    }

    ORPHANS
        .lock()
        .retain(|entry| !nodes.iter().any(|node| Arc::ptr_eq(node, &entry.child)));
    for node in nodes {
        if let Some(parent) = node.parent_node() {
            parent
                .children
                .lock()
                .retain(|child| child.as_ptr() != Arc::as_ptr(node));
        }
        // Parent slots are strong Arcs. Clear the entire failed graph so a
        // cycle rejected during table validation does not keep every batch
        // node alive after it is removed from CLOCKS.
        node.parents.lock().iter_mut().for_each(|slot| *slot = None);
    }
    CLOCKS
        .lock()
        .retain(|clock| !nodes.iter().any(|node| Arc::ptr_eq(node, clock)));
}

/// Gate off any clock that is enabled in hardware but holds no consumer
/// references, skipping clocks marked [`ClkFlags::IS_CRITICAL`] or
/// [`ClkFlags::IGNORE_UNUSED`].
///
/// Intended for a late-init pass, mirroring Linux `clk_disable_unused`. Only
/// the gate is turned off; `prepare` state is left alone, matching Linux.
pub fn clk_disable_unused() {
    let _guard = CLK_LOCK.lock();
    let clocks: Vec<Arc<ClkCore>> = CLOCKS.lock().iter().cloned().collect();
    for clk in &clocks {
        // `intersects`, not `contains`: skip if *either* flag is set.
        if clk
            .flags
            .intersects(ClkFlags::IS_CRITICAL | ClkFlags::IGNORE_UNUSED)
        {
            continue;
        }
        // Re-check and gate under the enable lock: a concurrent
        // `clk_enable` from interrupt context could otherwise raise the
        // count and set the gate bit between the count read here and the
        // `disable` below, silently turning a live clock off.
        let _el = CLK_ENABLE_LOCK.lock();
        let unused = {
            let st = clk.state.lock();
            st.enable_count == 0
        };
        if unused && clk.is_enabled().unwrap_or(false) {
            clk.ops.disable();
        }
    }
}

/// A one-line summary of one clock (see [`clk_summary`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClkSummaryEntry {
    /// Clock name.
    pub name: &'static str,
    /// Cached rate, Hz.
    pub rate: Rate,
    /// `(prepare_count, enable_count)`.
    pub counts: (i32, i32),
}

/// Snapshot every registered clock as `(name, rate, counts)` — the kernel
/// analogue of Linux debugfs `clk_summary`, for bring-up debugging and
/// diagnostics. Order follows registration.
pub fn clk_summary() -> Vec<ClkSummaryEntry> {
    let clocks: Vec<Arc<ClkCore>> = CLOCKS.lock().iter().cloned().collect();
    clocks
        .iter()
        .map(|c| ClkSummaryEntry {
            name: c.name(),
            rate: c.get_rate(),
            counts: c.counts(),
        })
        .collect()
}
