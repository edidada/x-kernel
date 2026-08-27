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
    pub fn unprepare(&self) {
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
    /// On enable failure the prepare reference is rolled back, matching Linux
    /// `clk_prepare_enable`.
    pub fn prepare_enable(&self) -> ClkResult {
        self.core.prepare()?;
        if let Err(err) = self.core.enable() {
            self.core.unprepare();
            return Err(err);
        }
        Ok(())
    }

    /// Disable then unprepare, in that order.
    pub fn disable_unprepare(&self) {
        self.core.disable();
        self.core.unprepare();
    }

    /// Whether the hardware reports the clock as enabled.
    pub fn is_enabled(&self) -> ClkResult<bool> {
        self.core.is_enabled()
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
        crate::core::with_clk_lock(|| {
            let id = self.vote_id.load(Ordering::Acquire);
            let id = if id != 0 {
                id
            } else {
                let new = self.core.allocate_vote_id();
                self.vote_id.store(new, Ordering::Release);
                new
            };
            self.core.set_rate_vote(id, min, max);
        });
        Ok(())
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
        self.notifier_id
            .compare_exchange(id, 0, Ordering::AcqRel, Ordering::Acquire)
            .ok();
    }
}
