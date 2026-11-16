// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Consumer-side clock handle.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::{ClkCore, ClkError, ClkNotifier, ClkNotifierId, ClkResult, Rate};

/// A handle to a registered clock.
///
/// Obtained from the provider via [`crate::clk_get_by_name`] or
/// [`crate::clk_get_by_index`]. Mirrors the Linux `struct clk`: each
/// acquisition is a separate consumer carrying its own rate-range vote
/// (aggregated by [`ClkCore`] as the tightest intersection); cloning a handle
/// yields a fresh consumer with no inherited vote.
pub struct ClkHandle {
    core: Arc<ClkCore>,
    /// This consumer's rate-range vote id on the core, allocated on first
    /// `set_rate_range`. `0` until then. Removed on drop. Atomic only for
    /// `Sync` (set_rate_range runs under `CLK_LOCK`, so no concurrent access).
    vote_id: AtomicU64,
    /// This consumer's notifier registration id, if it registered one via
    /// [`register_notifier`](Self::register_notifier). `0` = none. Replaced by
    /// a new registration and removed on drop.
    notifier_id: AtomicU64,
}

impl Clone for ClkHandle {
    fn clone(&self) -> Self {
        // A clone is a separate consumer; it inherits neither the original's
        // rate-range vote nor its notifier registration (matching `clk_get`
        // returning a fresh `struct clk`).
        Self {
            core: self.core.clone(),
            vote_id: AtomicU64::new(0),
            notifier_id: AtomicU64::new(0),
        }
    }
}

impl Drop for ClkHandle {
    fn drop(&mut self) {
        let vote = self.vote_id.load(Ordering::Acquire);
        if vote != 0 {
            self.core.remove_rate_vote(vote);
        }
        let notifier = self.notifier_id.load(Ordering::Acquire);
        if notifier != 0 {
            self.core.remove_notifier(notifier);
        }
    }
}

impl core::fmt::Debug for ClkHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Print the clock name only — the underlying node carries locks and
        // register state that are neither Debug-safe nor useful in logs.
        f.debug_struct("ClkHandle")
            .field("name", &self.name())
            .finish()
    }
}

impl ClkHandle {
    /// Wrap an existing clock node. Provider lookups call this internally.
    pub fn new(core: Arc<ClkCore>) -> Self {
        Self {
            core,
            vote_id: AtomicU64::new(0),
            notifier_id: AtomicU64::new(0),
        }
    }

    /// Consume the handle, yielding the underlying node. Used by the
    /// table-driven registrar to adopt an external provider's clock as a
    /// parent slot (the handle drops its consumer state without releasing
    /// any vote or notifier — the node itself is not a consumer).
    pub(crate) fn into_core(self) -> Arc<ClkCore> {
        let me = core::mem::ManuallyDrop::new(self);
        // SAFETY: `me` is never used again after this read; the move out of
        // `core` is fine because `Drop` will not run on `me` (ManuallyDrop),
        // so the node reference simply transfers to the caller.
        unsafe { core::ptr::read(&me.core) }
    }

    /// Human-readable name of the underlying clock.
    pub fn name(&self) -> &'static str {
        self.core.name()
    }

    /// Cached rate, in Hz.
    pub fn get_rate(&self) -> Rate {
        self.core.get_rate()
    }

    /// Cached accuracy, in parts per billion.
    pub fn get_accuracy(&self) -> u32 {
        self.core.get_accuracy()
    }

    /// Prepare the slow path, propagating to the parent first.
    pub fn prepare(&self) -> ClkResult {
        self.core.prepare()
    }

    /// Release one prepare reference, then propagate to the parent.
    ///
    /// Returns the hardware `unprepare` error, if any — the software count is
    /// already decremented, so an error means the hardware may still be
    /// prepared while the framework considers it unprepared.
    pub fn unprepare(&self) -> ClkResult {
        self.core.unprepare()
    }

    /// Enable the atomic fast path, propagating to the parent first.
    pub fn enable(&self) -> ClkResult {
        self.core.enable()
    }

    /// Release one enable reference, then propagate to the parent.
    pub fn disable(&self) {
        self.core.disable()
    }

    /// Prepare then enable, in that order.
    ///
    /// Atomic w.r.t. `set_parent`: the whole prepare+enable pair runs under one
    /// `CLK_LOCK` acquisition, so a concurrent reparent cannot observe the
    /// prepare half propagating to one parent and the enable half to another
    /// (the halves independently re-resolve "current parent" otherwise,
    /// leaking a reference on the pre-switch parent — a race the concurrency
    /// stress test caught live). On enable failure the prepare reference is
    /// rolled back, matching Linux `clk_prepare_enable`.
    pub fn prepare_enable(&self) -> ClkResult {
        crate::core::with_clk_lock(|| -> ClkResult<()> {
            self.core.prepare_locked()?;
            // The enable half must also hold CLK_ENABLE_LOCK (CLK_LOCK →
            // CLK_ENABLE_LOCK order, same as the set_parent migration): the
            // 0→1 transition check inside enable_locked is check-then-act on
            // the per-node count, and two unsynchronized callers can both see
            // 0, both fire ops.enable, and double-increment — the hardware-op
            // counters then unbalance (caught by the concurrency stress test).
            let el = crate::core::enable_lock();
            if let Err(err) = self.core.enable_locked() {
                drop(el);
                let _ = self.core.unprepare_locked();
                return Err(err);
            }
            Ok(())
        })
    }

    /// Disable then unprepare, in that order.
    ///
    /// Same atomicity rationale as [`prepare_enable`](Self::prepare_enable):
    /// one `CLK_LOCK` acquisition across both halves so the disable half and
    /// the unprepare half resolve the same parent. Discards the hardware
    /// `unprepare` error (the release is already committed); use
    /// [`unprepare`](Self::unprepare) directly to observe it.
    pub fn disable_unprepare(&self) {
        crate::core::with_clk_lock(|| {
            let el = crate::core::enable_lock();
            self.core.disable_locked();
            drop(el);
            let _ = self.core.unprepare_locked();
        });
    }

    /// Whether the hardware reports the clock as enabled.
    pub fn is_enabled(&self) -> ClkResult<bool> {
        self.core.is_enabled()
    }

    /// Whether the hardware reports the clock as enabled.
    ///
    /// Register-backed view for diagnostics; a gate-less provider reports
    /// `false` even while its enable count is positive. Consumers that want
    /// the prepare/enable contract's answer use [`Self::is_enabled`].
    pub fn is_enabled_in_hw(&self) -> ClkResult<bool> {
        self.core.is_enabled_in_hw()
    }

    /// Whether the hardware reports the clock as prepared.
    pub fn is_prepared(&self) -> ClkResult<bool> {
        self.core.is_prepared()
    }

    /// The currently selected parent clock, if any.
    pub fn get_parent(&self) -> Option<ClkHandle> {
        self.core.get_parent().map(ClkHandle::new)
    }

    /// Compute the closest achievable rate for `rate` without changing state.
    ///
    /// Delegates to [`ClkCore::round_rate`], which may propagate to the parent.
    pub fn round_rate(&self, rate: Rate) -> ClkResult<Rate> {
        self.core.round_rate(rate)
    }

    /// Program a new rate and return the rate now in effect.
    pub fn set_rate(&self, rate: Rate) -> ClkResult<Rate> {
        self.core.set_rate(rate)
    }

    /// Constrain this clock's rate to `[min, max]` for later `set_rate`/
    /// `round_rate` calls.
    /// Constrain this consumer's view of the clock's rate to `[min, max]`.
    /// The effective range is the tightest intersection across all consumers'
    /// votes (and the legacy core-level slot), so two consumers cannot clobber
    /// each other. The vote is removed when this handle is dropped.
    pub fn set_rate_range(&self, min: Rate, max: Rate) -> ClkResult {
        if min > max {
            return Err(ClkError::InvalidRate);
        }
        crate::core::with_clk_lock(|| -> ClkResult<()> {
            let id = self.vote_id.load(Ordering::Acquire);
            let id = if id != 0 {
                id
            } else {
                let new = self.core.allocate_vote_id();
                self.vote_id.store(new, Ordering::Release);
                new
            };
            self.core.set_rate_vote(id, min, max)
        })
    }

    /// The current `(min, max)` rate range.
    pub fn get_rate_range(&self) -> (Rate, Rate) {
        self.core.get_rate_range()
    }

    /// Select parent `index` and refresh the cached rate.
    pub fn set_parent(&self, index: u8) -> ClkResult {
        self.core.set_parent(index)
    }

    /// Register a rate-change subscriber on this clock.
    ///
    /// The subscriber is invoked around rate changes; a `PreRateChange` veto
    /// aborts the change. Returns an id for [`unregister_notifier`](Self::unregister_notifier).
    /// See [`crate::ClkEvent`].
    ///
    /// The registration is owned by this handle: a new registration replaces
    /// the previous one, and it is unregistered automatically when the handle
    /// is dropped (one live notifier per handle; to register several, clone the
    /// handle — each clone owns its own registration).
    pub fn register_notifier(&self, cb: ClkNotifier) -> ClkNotifierId {
        crate::core::with_clk_lock(|| {
            let prev = self.notifier_id.swap(0, Ordering::AcqRel);
            if prev != 0 {
                self.core.remove_notifier(prev);
            }
            let id = self.core.add_notifier(cb);
            self.notifier_id.store(id, Ordering::Release);
            id
        })
    }

    /// Remove a subscriber previously registered via [`register_notifier`](Self::register_notifier).
    /// If it is the handle's tracked registration, the tracking is cleared so
    /// drop will not unregister it twice.
    pub fn unregister_notifier(&self, id: ClkNotifierId) {
        self.core.remove_notifier(id);
        // CAS result is intentionally discarded: on success drop will not
        // unregister twice; on failure this was not the tracked id.
        let _ = self
            .notifier_id
            .compare_exchange(id, 0, Ordering::AcqRel, Ordering::Acquire);
    }
}
