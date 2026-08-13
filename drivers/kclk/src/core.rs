// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Clock tree node, parent links, rate caching, two-phase gating state, and
//! rate-change notifications.

use alloc::{sync::Arc, vec::Vec};

use kspin::SpinNoIrq;
use ksync::{Mutex, static_lock};

use crate::{
    ClkError, ClkEvent, ClkFlags, ClkHwOps, ClkNotifier, ClkNotifierId, ClkRateRequest, ClkResult,
    NotifierResult, Rate,
};

// Global serialization lock for sleepable clock mutations.
//
// All `prepare`/`unprepare`/`set_rate`/`set_parent`/`round_rate` operations
// take this lock, so a rate change (which may recurse up the parent tree and
// reprogram a PLL) is atomic with respect to other changes. The atomic gate
// (`enable`/`disable`) does *not* take this lock — it stays on the per-node
// spinlock so it remains usable from contexts that cannot block.
//
// This mirrors Linux's single `clk_prepare_lock`. Clock operations are not a
// hot path (boot/probe/DVFS), so one global lock is acceptable; the per-node
// spinlock still keeps `get_rate` reads lock-free w.r.t. this lock.
//
// The lock is a sleepable mutex (`ksync::Mutex`), matching Linux's
// `clk_prepare_lock`: providers may sleep in the `prepare`/`set_rate`/
// `set_parent`/`round_rate` callbacks that run under it. It must therefore
// never be taken from atomic or interrupt context, and it is not re-entrant —
// re-entering the framework while holding it panics instead of deadlocking.
static_lock! {
    static CLK_LOCK: Mutex<()> = Mutex::new(());
}

/// Global list of every registered clock, used by the unused-clock gate-down
/// pass and future PM walks. Strong references mirror Linux, which never frees
/// a `clk_core` during normal operation.
static CLOCKS: SpinNoIrq<Vec<Arc<ClkCore>>> = SpinNoIrq::new(Vec::new());

/// A clock slot waiting for a parent (by name) that has not been registered
/// yet. When a clock with `parent_name` is built, the framework adopts it.
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
    min_rate: Rate,
    max_rate: Rate,
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
            if let Ok(rate) = self.ops.recalc_rate(parent_rate) {
                self.state.lock().rate = rate;
                return rate;
            }
        }
        self.state.lock().rate
    }

    /// Cached accuracy, in parts per billion.
    pub fn get_accuracy(&self) -> u32 {
        self.state.lock().accuracy
    }

    /// Clamp `rate` into this clock's configured `[min, max]` range.
    fn clamp_rate(&self, rate: Rate) -> Rate {
        let st = self.state.lock();
        rate.clamp(st.min_rate, st.max_rate.max(st.min_rate))
    }

    /// Constrain this clock's rate to `[min, max]`.
    ///
    /// Subsequent [`set_rate`](Self::set_rate) / [`round_rate`](Self::round_rate)
    /// requests are clamped into this range. The current rate is *not*
    /// retroactively changed. `min == 0` means no lower bound; `max ==
    /// u64::MAX` means no upper bound.
    pub fn set_rate_range(&self, min: Rate, max: Rate) -> ClkResult {
        if min > max {
            return Err(ClkError::InvalidRate);
        }
        let _guard = CLK_LOCK.lock();
        let mut st = self.state.lock();
        st.min_rate = min;
        st.max_rate = max;
        Ok(())
    }

    /// The current `(min, max)` rate range.
    pub fn get_rate_range(&self) -> (Rate, Rate) {
        let st = self.state.lock();
        (st.min_rate, st.max_rate)
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

    fn prepare_locked(&self) -> ClkResult {
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
                parent.unprepare_locked();
            }
            return Err(err);
        }
        self.state.lock().prepare_count += 1;
        Ok(())
    }

    /// Release one prepare reference, then propagate to the parent.
    pub fn unprepare(&self) {
        let _guard = CLK_LOCK.lock();
        self.unprepare_locked();
    }

    fn unprepare_locked(&self) {
        // Decrement under the lock, then drop it before the sleepable op —
        // same rationale as `prepare_locked`: `unprepare` may sleep and must
        // not run with `SpinNoIrq` held.
        let need_unprepare = {
            let mut st = self.state.lock();
            if st.prepare_count == 0 {
                return;
            }
            st.prepare_count -= 1;
            st.prepare_count == 0
        };
        if need_unprepare {
            let _ = self.ops.unprepare();
        }
        if let Some(parent) = self.parent_node() {
            parent.unprepare_locked();
        }
    }

    /// Enable the fast path, propagating to the parent first.
    ///
    /// Atomic; the caller must hold no spin locks that the `enable` op might
    /// also take. Not serialized by [`CLK_LOCK`] so it stays usable where
    /// sleeping is impossible.
    pub fn enable(&self) -> ClkResult {
        let parent_done = match self.parent_node() {
            Some(parent) => {
                parent.enable()?;
                true
            }
            None => false,
        };

        let mut st = self.state.lock();
        if st.enable_count == 0
            && let Err(err) = self.ops.enable()
        {
            drop(st);
            if parent_done && let Some(parent) = self.parent_node() {
                parent.disable();
            }
            return Err(err);
        }
        st.enable_count += 1;
        Ok(())
    }

    /// Release one enable reference, then propagate to the parent.
    pub fn disable(&self) {
        let mut st = self.state.lock();
        if st.enable_count == 0 {
            return;
        }
        st.enable_count -= 1;
        if st.enable_count == 0 {
            self.ops.disable();
        }
        drop(st);
        if let Some(parent) = self.parent_node() {
            parent.disable();
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
            (st.min_rate, st.max_rate.max(st.min_rate), st.parent_index)
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
        let req = self.determine_rate_locked(rate)?;

        // Glitch avoidance (SET_RATE_GATE): gate across the reprogram and
        // restore afterwards, matching Linux. The re-enable is *not* silent: if
        // it fails the clock is left gated while its rate changed, an
        // inconsistent state we surface as BadState rather than reporting
        // success.
        let gate_around = self.flags.contains(ClkFlags::SET_RATE_GATE);
        let was_enabled = if gate_around {
            self.is_enabled().unwrap_or(false)
        } else {
            false
        };
        if gate_around && was_enabled {
            self.ops.disable();
        }

        let result = self.program_rate_locked(&req);

        if gate_around && was_enabled && self.ops.enable().is_err() {
            // Rate/parent may have changed but the clock could not be
            // re-enabled — surface the inconsistency instead of masking it.
            return Err(ClkError::BadState);
        }

        let actual = result?;
        self.state.lock().rate = actual;
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
        // Lateral: switch to the parent `determine_rate` selected, if different.
        let current = self.state.lock().parent_index;
        if let Some(idx) = req.best_parent_index.filter(|&i| i != current) {
            self.switch_parent_locked(idx)?;
        }
        let parent_rate = self.parent_rate();

        // Upward: the node asks its selected parent to change rate.
        if let Some(target) = req
            .parent_target
            .filter(|_| self.flags.contains(ClkFlags::SET_RATE_PARENT))
        {
            let parent = self.parent_node().ok_or(ClkError::NotSupported)?;
            let actual_parent = parent.set_rate_locked(target)?;
            return self.ops.recalc_rate(actual_parent);
        }

        // Own rate field; a transparent node (no `set_rate`) just reports its
        // (possibly just-switched) parent rate.
        match self.ops.set_rate(req.rate, parent_rate) {
            Ok(actual) => Ok(actual),
            Err(ClkError::NotSupported) => self.ops.recalc_rate(parent_rate),
            Err(err) => Err(err),
        }
    }

    // ---- parent selection ----

    /// Select parent `index` and refresh the cached rate and accuracy.
    ///
    /// Serialized by [`CLK_LOCK`]. The new parent must already be registered at
    /// `index`. When [`ClkFlags::SET_RATE_GATE`] is set, the clock is gated
    /// across the mux switch and restored afterwards, to avoid glitches on
    /// running hardware.
    pub fn set_parent(&self, index: u8) -> ClkResult {
        let _guard = CLK_LOCK.lock();
        // Glitch avoidance (SET_RATE_GATE) around the mux switch: same
        // re-enable contract as set_rate — a failed re-enable is surfaced, not
        // silently masked.
        let gate_around = self.flags.contains(ClkFlags::SET_RATE_GATE);
        let was_enabled = if gate_around {
            self.is_enabled().unwrap_or(false)
        } else {
            false
        };
        if gate_around && was_enabled {
            self.ops.disable();
        }
        let result = self.switch_parent_locked(index);
        if gate_around && was_enabled && self.ops.enable().is_err() {
            return Err(ClkError::BadState);
        }
        result
    }

    /// Raw parent switch plus cache refresh.
    ///
    /// The caller must hold [`CLK_LOCK`] and is responsible for any
    /// `SET_RATE_GATE` glitch avoidance around the switch.
    fn switch_parent_locked(&self, index: u8) -> ClkResult {
        let parent = {
            let table = self.parents.lock();
            if index as usize >= table.len() {
                return Err(ClkError::InvalidParent);
            }
            // Reject switching to an orphan slot: recalculating against a
            // missing parent would silently produce a zero rate.
            match table[index as usize].clone() {
                Some(parent) => parent,
                None => return Err(ClkError::InvalidParent),
            }
        };
        self.ops.set_parent(index)?;

        let parent_rate = parent.get_rate();
        let parent_accuracy = parent.get_accuracy();
        let mut st = self.state.lock();
        st.parent_index = index;
        st.rate = self.ops.recalc_rate(parent_rate)?;
        st.accuracy = self.ops.recalc_accuracy(parent_accuracy)?;
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
            state: SpinNoIrq::new(ClkState {
                enable_count: 0,
                prepare_count: 0,
                parent_index,
                rate,
                accuracy,
                min_rate: 0,
                max_rate: Rate::MAX,
            }),
            notifiers: SpinNoIrq::new(NotifierState {
                next_id: 0,
                entries: Vec::new(),
            }),
        });
        // Post-registration hook (Linux `clk_ops.init`). Runs before the clock
        // is published so a failing init aborts registration.
        core.ops.init()?;
        CLOCKS.lock().push(core.clone());
        // Register this node's own deferred-parent slots, then adopt any orphan
        // that was waiting for this node.
        for (index, pname) in pending_parents {
            ORPHANS.lock().push(OrphanEntry {
                child: core.clone(),
                parent_index: index,
                parent_name: pname,
            });
        }
        adopt_orphans(core.name(), core.clone());
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
fn adopt_orphans(name: &'static str, parent: Arc<ClkCore>) {
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

    for entry in matched {
        {
            let mut table = entry.child.parents.lock();
            if let Some(slot) = table.get_mut(entry.parent_index as usize) {
                *slot = Some(parent.clone());
            }
        }
        if entry.child.state.lock().parent_index == entry.parent_index {
            let parent_rate = parent.get_rate();
            let parent_accuracy = parent.get_accuracy();
            let mut st = entry.child.state.lock();
            st.rate = entry.child.ops.recalc_rate(parent_rate).unwrap_or(st.rate);
            st.accuracy = entry
                .child
                .ops
                .recalc_accuracy(parent_accuracy)
                .unwrap_or(st.accuracy);
        }
    }
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
        let unused = {
            let st = clk.state.lock();
            st.enable_count == 0
        };
        if unused && clk.is_enabled().unwrap_or(false) {
            clk.ops.disable();
        }
    }
}
