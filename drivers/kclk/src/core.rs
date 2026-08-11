// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Clock tree node, parent links, rate caching, two-phase gating state, and
//! rate-change notifications.

use alloc::{sync::Arc, vec::Vec};

use kspin::{SpinNoIrq, SpinRaw};

use crate::{ClkError, ClkEvent, ClkFlags, ClkHwOps, ClkNotifier, ClkResult, NotifierResult, Rate};

/// Global serialization lock for sleepable clock mutations.
///
/// All `prepare`/`unprepare`/`set_rate`/`set_parent`/`round_rate` operations
/// take this lock, so a rate change (which may recurse up the parent tree and
/// reprogram a PLL) is atomic with respect to other changes. The atomic gate
/// (`enable`/`disable`) does *not* take this lock — it stays on the per-node
/// spinlock so it remains usable from contexts that cannot block.
///
/// This mirrors Linux's single `clk_prepare_lock`. Clock operations are not a
/// hot path (boot/probe/DVFS), so a single global lock is acceptable; the
/// per-node spinlock still keeps `get_rate` reads lock-free w.r.t. this lock.
///
/// The lock is a plain spinlock, not a sleepable mutex: callbacks must not
/// sleep while held. Current register-backed ops are non-sleeping; a future
/// PLL whose `prepare` truly needs to sleep will require a sleepable mutex.
static CLK_LOCK: SpinRaw<()> = SpinRaw::new(());

/// Mutable per-node state, guarded by a spin lock.
struct ClkState {
    enable_count: i32,
    prepare_count: i32,
    parent_index: u8,
    rate: Rate,
    accuracy: u32,
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
    parents: Vec<Option<Arc<ClkCore>>>,
    state: SpinNoIrq<ClkState>,
    notifiers: SpinNoIrq<Vec<ClkNotifier>>,
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
    pub fn get_rate(&self) -> Rate {
        self.state.lock().rate
    }

    /// Cached accuracy, in parts per billion.
    pub fn get_accuracy(&self) -> u32 {
        self.state.lock().accuracy
    }

    /// The currently selected parent node, as a cloned [`Arc`].
    ///
    /// Cloning the [`Arc`] (cheap, refcount-only) lets callers recurse into the
    /// parent without holding this node's state lock, which would otherwise
    /// risk a lock-ordering deadlock during rate propagation.
    fn parent_node(&self) -> Option<Arc<ClkCore>> {
        let idx = self.state.lock().parent_index as usize;
        self.parents.get(idx).and_then(Option::clone)
    }

    /// Rate of the selected parent, or `0` for a root.
    fn parent_rate(&self) -> Rate {
        self.parent_node().map(|p| p.get_rate()).unwrap_or(0)
    }

    /// Register a rate-change subscriber on this clock.
    ///
    /// Subscribers live for the lifetime of the node; there is no explicit
    /// unregister. This matches the boot/probe-time configuration use case.
    pub fn add_notifier(&self, cb: ClkNotifier) {
        self.notifiers.lock().push(cb);
    }

    /// Snapshot the notifier list so callbacks can be invoked without holding
    /// the notifier lock (a callback that, say, queries `get_rate` would
    /// otherwise self-deadlock).
    fn snapshot_notifiers(&self) -> Vec<ClkNotifier> {
        self.notifiers.lock().clone()
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

        let mut st = self.state.lock();
        if st.prepare_count == 0
            && let Err(err) = self.ops.prepare()
        {
            drop(st);
            if parent_done && let Some(parent) = self.parent_node() {
                parent.unprepare_locked();
            }
            return Err(err);
        }
        st.prepare_count += 1;
        Ok(())
    }

    /// Release one prepare reference, then propagate to the parent.
    pub fn unprepare(&self) {
        let _guard = CLK_LOCK.lock();
        self.unprepare_locked();
    }

    fn unprepare_locked(&self) {
        let mut st = self.state.lock();
        if st.prepare_count == 0 {
            return;
        }
        st.prepare_count -= 1;
        if st.prepare_count == 0 {
            // The hardware op runs with the lock held, matching Linux: providers
            // are expected to touch only their own registers and not re-enter
            // the framework while unpreparing.
            let _ = self.ops.unprepare();
        }
        drop(st);
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

    fn round_rate_locked(&self, rate: Rate) -> ClkResult<Rate> {
        let parent_rate = self.parent_rate();
        let rounded = self.ops.round_rate(rate, parent_rate)?;
        if let Some(target) = rounded
            .parent_target
            .filter(|_| self.flags.contains(ClkFlags::SET_RATE_PARENT))
            && let Some(parent) = self.parent_node()
        {
            let new_parent = parent.round_rate_locked(target)?;
            return self.ops.round_rate(rate, new_parent).map(|r| r.rate);
        }
        Ok(rounded.rate)
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
        let parent_rate = self.parent_rate();
        let rounded = self.ops.round_rate(rate, parent_rate)?;

        let actual = if let Some(target) = rounded
            .parent_target
            .filter(|_| self.flags.contains(ClkFlags::SET_RATE_PARENT))
        {
            // Forward to the parent: reprogram it, then recalc ourselves.
            let parent = self.parent_node().ok_or(ClkError::NotSupported)?;
            let actual_parent = parent.set_rate_locked(target)?;
            self.ops.recalc_rate(actual_parent)?
        } else {
            self.ops.set_rate(rounded.rate, parent_rate)?
        };

        self.state.lock().rate = actual;
        Ok(actual)
    }

    // ---- parent selection ----

    /// Select parent `index` and refresh the cached rate and accuracy.
    ///
    /// Serialized by [`CLK_LOCK`]. The new parent must already be registered at
    /// `index`; selecting an unmapped (orphan) slot is rejected.
    pub fn set_parent(&self, index: u8) -> ClkResult {
        let _guard = CLK_LOCK.lock();
        if index as usize >= self.parents.len() {
            return Err(ClkError::InvalidParent);
        }
        // Reject switching to an orphan slot: recalculating against a missing
        // parent would silently produce a zero rate.
        let Some(parent) = self.parents[index as usize].clone() else {
            return Err(ClkError::InvalidParent);
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
}

impl ClkBuilder {
    /// Start a node with the given name and hardware operations.
    pub fn new(name: &'static str, ops: Arc<dyn ClkHwOps>) -> Self {
        Self {
            name,
            flags: ClkFlags::empty(),
            ops,
            parents: Vec::new(),
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
        let parent_index = self.ops.get_parent().unwrap_or(0);
        let (parent_rate, parent_accuracy) = self
            .parents
            .get(parent_index as usize)
            .and_then(Option::clone)
            .map(|p| (p.get_rate(), p.get_accuracy()))
            .unwrap_or((0, 0));
        let rate = self.ops.recalc_rate(parent_rate)?;
        let accuracy = self.ops.recalc_accuracy(parent_accuracy)?;
        Ok(Arc::new(ClkCore {
            name: self.name,
            flags: self.flags,
            ops: self.ops,
            parents: self.parents,
            state: SpinNoIrq::new(ClkState {
                enable_count: 0,
                prepare_count: 0,
                parent_index,
                rate,
                accuracy,
            }),
            notifiers: SpinNoIrq::new(Vec::new()),
        }))
    }
}
