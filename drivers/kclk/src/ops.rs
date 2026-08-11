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
//! Clock enabling is split into two phases:
//!
//! - [`prepare`](ClkHwOps::prepare) / [`unprepare`](ClkHwOps::unprepare) carry
//!   the slow, controller-global setup;
//! - [`enable`](ClkHwOps::enable) / [`disable`](ClkHwOps::disable) toggle only
//!   a gate bit and stay usable where blocking is impossible.
//!
//! **Both phases run with IRQs/preemption intact but cannot sleep.** All
//! callbacks execute under the framework's global clock lock plus the node's
//! spinlock (see `core.rs` and the crate design doc); a callback that blocks
//! or sleeps will stall every other CPU that touches the clock tree. Slow work
//! that genuinely needs to wait (a PLL lock) must be implemented as a
//! **bounded busy-wait** that polls a status bit with a timeout, never a sleep.
//!
//! This is intentionally narrower than Linux's `may sleep` contract: the
//! framework uses a spinlock, not a sleepable mutex, to avoid pulling the
//! scheduler into the clock core. The two phases still earn their keep because
//! `enable`/`disable` deliberately bypass the global lock (they may run from
//! contexts that cannot take it), whereas `prepare`/`unprepare` are serialized
//! by it.
//!
//! # Re-entrancy
//!
//! All callbacks run under the framework's global clock lock (for the
//! prepare/rate/parent paths) plus the per-node spinlock. A callback must not:
//! - sleep or block (no scheduler waits; use bounded busy-wait for PLL lock);
//! - re-enter the clock framework (`clk_get_*`, `set_rate`, `set_parent`,
//!   `prepare`, `enable`) — the lock deadlocks;
//! - acquire any lock that could be held in reverse order during tree
//!   propagation.
//!
//! Touching the node's own registers is the expected work.

use crate::{ClkError, ClkResult, Rate, RoundResult};

/// Provider-side hardware operations for one clock node.
///
/// Every method receives only what the hardware needs; framework state such as
/// reference counts and rate caches is owned by [`crate::ClkCore`].
pub trait ClkHwOps: Send + Sync {
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
    /// Default: the node cannot change rate; report the parent rate and do not
    /// request a parent change.
    fn round_rate(&self, _rate: Rate, parent_rate: Rate) -> ClkResult<RoundResult> {
        Ok(RoundResult::passthrough(parent_rate))
    }

    /// Program a new rate and return the rate now in effect.
    /// Default: not supported.
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

    /// Select parent `index`. Default: not supported.
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
