// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Provider-side hardware operation callbacks.
//!
//! [`ClkHwOps`] is the provider-side contract, mirroring the Linux
//! `struct clk_ops`. A clock-controller driver implements the subset of methods
//! relevant to its clock type; the default implementations model a node that
//! neither gates nor changes rate.
//!
//! # The two-phase gate
//!
//! Clock enabling is split into two phases, mirroring Linux's
//! `prepare_lock` + `enable_lock` two-lock model:
//!
//! - [`prepare`](ClkHwOps::prepare) / [`unprepare`](ClkHwOps::unprepare) carry
//!   the slow, controller-global setup; they run under the sleepable
//!   `CLK_LOCK` mutex and **may sleep** (a PLL lock wait can block).
//! - [`enable`](ClkHwOps::enable) / [`disable`](ClkHwOps::disable) toggle only
//!   a gate bit. They run under a separate IRQ-saving **spinlock**
//!   (`CLK_ENABLE_LOCK`), so `enable` stays usable from interrupt context
//!   (console/polling drivers) while remaining mutually exclusive with
//!   `set_parent`'s reference migration. The provider `enable`/`disable`
//!   callback runs with IRQs disabled and must be quick and non-sleeping —
//!   it only flips a gate bit (use `HIWORD_MASK` on a shared register word).
//!
//! The remaining callbacks (`recalc_rate`, `recalc_accuracy`, `is_enabled`,
//! `is_prepared`, `get_parent`, `set_parent`, `init`, `determine_rate`) may be
//! invoked without the sleepable lock or, for `set_parent` and reparent-time
//! recalculation, while `CLK_ENABLE_LOCK` has disabled IRQs. They **must not
//! sleep**. `set_rate` runs only under `CLK_LOCK` and may sleep, but returning
//! an error must leave its hardware state unchanged.
//!
//! # Re-entrancy
//!
//! A callback must not:
//! - re-enter the clock framework (`clk_get_*`, `set_rate`, `set_parent`,
//!   `prepare`, `enable`) — the global lock is **not re-entrant** and panics on
//!   self-acquisition instead of deadlocking;
//! - acquire any lock that could be held in reverse order during tree
//!   propagation.
//!
//! Touching the node's own registers is the expected work.

use crate::{ClkError, ClkRateRequest, ClkResult, Rate, RoundResult};

/// Provider-side hardware operations for one clock node.
///
/// Every method receives only what the hardware needs; framework state such as
/// reference counts and rate caches is owned by [`crate::ClkCore`].
pub trait ClkHwOps: Send + Sync {
    // ---- lifecycle ----

    /// One-time post-registration hook (Linux `clk_ops.init`).
    ///
    /// Runs after the initial rate/accuracy have been cached but before the
    /// clock is published; returning `Err` aborts registration. Default: no-op.
    fn init(&self) -> ClkResult {
        Ok(())
    }

    // ---- gating: two-phase (prepare may sleep, enable is atomic) ----

    /// Slow, sleepable preparation step. Default: no-op.
    fn prepare(&self) -> ClkResult {
        Ok(())
    }

    /// Reverse of [`prepare`](Self::prepare). Default: no-op.
    fn unprepare(&self) -> ClkResult {
        Ok(())
    }

    /// Whether the clock is currently prepared. Default: `false`.
    fn is_prepared(&self) -> ClkResult<bool> {
        Ok(false)
    }

    /// Atomic fast-path enable. Default: no-op.
    fn enable(&self) -> ClkResult {
        Ok(())
    }

    /// Reverse of [`enable`](Self::enable). Default: no-op.
    fn disable(&self) {}

    /// Whether the clock is currently enabled. Default: `false`.
    fn is_enabled(&self) -> ClkResult<bool> {
        Ok(false)
    }

    // ---- rate ----

    /// Recompute the rate from the parent rate (read divider/multiplier regs).
    /// Default: pass the parent rate through unchanged.
    fn recalc_rate(&self, parent_rate: Rate) -> ClkResult<Rate> {
        Ok(parent_rate)
    }

    /// Round a rate request without changing anything.
    ///
    /// Returns the closest achievable rate plus, optionally, the rate the
    /// parent should output so this node can reach it. `parent_target` is
    /// honored only when the node carries
    /// [`ClkFlags::SET_RATE_PARENT`](crate::ClkFlags::SET_RATE_PARENT).
    ///
    /// # Contract for `parent_target`
    ///
    /// Returning a `parent_target` *declares that this node's rate is fully
    /// determined by its parent* (it is rate-transparent — a mux, a fixed
    /// factor, or a node with no programmable field of its own). When the
    /// framework honors such a request it reprograms the parent and then
    /// reports this node's new rate via [`recalc_rate`](Self::recalc_rate); it
    /// will **not** call [`set_rate`](Self::set_rate) on this node. A node that
    /// both wants its parent changed *and* has its own programmable divider
    /// should not use `parent_target`: it should set its parent via
    /// `SET_RATE_PARENT` separately and program its own field through
    /// `set_rate` in the non-upward branch.
    ///
    /// Default: the node cannot change rate; report the parent rate and do not
    /// request a parent change.
    fn round_rate(&self, _rate: Rate, parent_rate: Rate) -> ClkResult<RoundResult> {
        Ok(RoundResult::passthrough(parent_rate))
    }

    /// Determine the closest achievable rate for a request, optionally
    /// selecting a best parent.
    ///
    /// This is the multi-parent-capable analogue of [`round_rate`](Self::round_rate),
    /// mirroring Linux's `clk_ops.determine_rate`. The core seeds `req` with the
    /// clamped request rate and the currently selected parent, then calls this
    /// once with the full candidate parent list (`(parent_index, parent_rate)`
    /// for every registered, non-orphan parent). The provider fills in
    /// `req.rate`, and may set `req.best_parent_index`/`req.best_parent_rate`
    /// to switch parents and/or `req.parent_target` to ask the framework to
    /// change the selected parent's rate.
    ///
    /// Default: delegates to [`round_rate`](Self::round_rate) against the
    /// currently selected parent's rate, preserving the legacy
    /// `parent_target` contract.
    fn determine_rate(&self, req: &mut ClkRateRequest, _parents: &[(u8, Rate)]) -> ClkResult<()> {
        let rounded = self.round_rate(req.rate, req.best_parent_rate)?;
        req.rate = rounded.rate;
        req.parent_target = rounded.parent_target;
        Ok(())
    }

    /// Program a new rate and return the rate now in effect.
    ///
    /// Returning an error must leave the clock's programmable state unchanged;
    /// the framework may need to roll back a parent-selection transaction and
    /// emits `RateChangeAborted` under that contract. Default: not supported.
    fn set_rate(&self, _rate: Rate, _parent_rate: Rate) -> ClkResult<Rate> {
        Err(ClkError::NotSupported)
    }

    // ---- parent selection (mux) ----

    /// Number of selectable parents. Default: one.
    fn num_parents(&self) -> u8 {
        1
    }

    /// Index of the currently selected parent. Default: zero.
    fn get_parent(&self) -> ClkResult<u8> {
        Ok(0)
    }

    /// Select parent `index`. Must be quick and non-sleeping: topology commits
    /// call this while holding the IRQ-saving enable lock. Returning an error
    /// must leave the hardware parent unchanged. Default: not supported.
    fn set_parent(&self, _index: u8) -> ClkResult {
        Err(ClkError::NotSupported)
    }

    // ---- accuracy ----

    /// Recompute accuracy (parts per billion) from the parent accuracy.
    /// Default: pass the parent accuracy through unchanged.
    fn recalc_accuracy(&self, parent_accuracy: u32) -> ClkResult<u32> {
        Ok(parent_accuracy)
    }
}
