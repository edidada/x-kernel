// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Systematic invariant and failure-injection tests.
//!
//! Instead of one example per behavior (as `tests.rs` does), this module
//! enumerates the state space combinatorially:
//!
//! - **INV-1 (refcount conservation)**: a prepared (enabled) child implies its
//!   resolved parent is prepared (enabled); after a full teardown every node's
//!   counts return to zero and the hardware op counters balance
//!   (prepare == unprepare, enable == disable).
//! - **INV-2 (cache coherence)**: after any mutation (success or failure),
//!   every node's cached rate still equals `recalc_rate(parent rate)`.
//!
//! [`FailOps`] is a fully programmable clock: it counts hardware ops (like
//! `CountingOps`) and can be armed to fail exactly one named op, exercising
//! the framework's rollback paths instead of only the happy path.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

use kspin::SpinNoIrq;
use unittest::{TestResult, assert_eq, def_test, ktest_println};

use crate::{ClkBuilder, ClkError, ClkFlags, ClkHandle, ClkHwOps, ClkResult, Rate, RoundResult};

/// A programmable clock node: counts every hardware op and can be armed to
/// fail the next invocation of a named op exactly once.
struct FailOps {
    prepare: SpinNoIrq<u32>,
    unprepare: SpinNoIrq<u32>,
    enable: SpinNoIrq<u32>,
    disable: SpinNoIrq<u32>,
    set_rate: SpinNoIrq<u32>,
    /// `Some(op)` → the next `op` returns `Err(Io)`, then disarms.
    fail: SpinNoIrq<Option<&'static str>>,
    /// Current rate; `set_rate` updates it, `recalc_rate` reports it.
    rate: SpinNoIrq<Rate>,
    /// Hardware gate state toggled by enable/disable.
    enabled: SpinNoIrq<bool>,
}

impl FailOps {
    fn new(rate: Rate) -> Arc<Self> {
        Arc::new(Self {
            prepare: SpinNoIrq::new(0),
            unprepare: SpinNoIrq::new(0),
            enable: SpinNoIrq::new(0),
            disable: SpinNoIrq::new(0),
            set_rate: SpinNoIrq::new(0),
            fail: SpinNoIrq::new(None),
            rate: SpinNoIrq::new(rate),
            enabled: SpinNoIrq::new(false),
        })
    }

    /// Arm a one-shot failure: the next `op` invocation returns `Err(Io)`.
    fn fail_next(&self, op: &'static str) {
        *self.fail.lock() = Some(op);
    }

    fn armed(&self, op: &str) -> bool {
        let mut f = self.fail.lock();
        if *f == Some(op) {
            *f = None;
            true
        } else {
            false
        }
    }

    fn counters(&self) -> (u32, u32, u32, u32, u32) {
        (
            *self.prepare.lock(),
            *self.unprepare.lock(),
            *self.enable.lock(),
            *self.disable.lock(),
            *self.set_rate.lock(),
        )
    }

    fn into_ops(self: Arc<Self>) -> Arc<dyn ClkHwOps> {
        self
    }
}

impl ClkHwOps for FailOps {
    fn prepare(&self) -> ClkResult<()> {
        if self.armed("prepare") {
            return Err(ClkError::Io);
        }
        *self.prepare.lock() += 1;
        Ok(())
    }

    fn unprepare(&self) -> ClkResult<()> {
        *self.unprepare.lock() += 1;
        Ok(())
    }

    fn is_prepared(&self) -> ClkResult<bool> {
        Ok(*self.prepare.lock() > *self.unprepare.lock())
    }

    fn enable(&self) -> ClkResult<()> {
        if self.armed("enable") {
            return Err(ClkError::Io);
        }
        *self.enable.lock() += 1;
        *self.enabled.lock() = true;
        Ok(())
    }

    fn disable(&self) {
        *self.disable.lock() += 1;
        *self.enabled.lock() = false;
    }

    fn is_enabled(&self) -> ClkResult<bool> {
        Ok(*self.enabled.lock())
    }

    fn recalc_rate(&self, _parent_rate: Rate) -> ClkResult<Rate> {
        Ok(*self.rate.lock())
    }

    fn round_rate(&self, rate: Rate, _parent_rate: Rate) -> ClkResult<RoundResult> {
        // A rate-programmable node: it satisfies the request itself.
        Ok(RoundResult {
            rate,
            parent_target: None,
        })
    }

    fn set_rate(&self, rate: Rate, _parent_rate: Rate) -> ClkResult<Rate> {
        if self.armed("set_rate") {
            return Err(ClkError::Io);
        }
        *self.set_rate.lock() += 1;
        *self.rate.lock() = rate;
        Ok(rate)
    }
}

// ---------------------------------------------------------------------------
// Invariant checkers
// ---------------------------------------------------------------------------

/// INV-1: a prepared/enabled child implies its resolved parent is
/// prepared/enabled.
fn assert_refcount_implication(child: &crate::ClkCore, parent: &crate::ClkCore) -> TestResult {
    let (cp, ce) = child.counts();
    let (pp, pe) = parent.counts();
    if cp > 0 && pp == 0 {
        ktest_println!(
            "INV-1 FAIL: {} prepared but parent {} is not",
            child.name(),
            parent.name()
        );
        return TestResult::Failed;
    }
    if ce > 0 && pe == 0 {
        ktest_println!(
            "INV-1 FAIL: {} enabled but parent {} is not",
            child.name(),
            parent.name()
        );
        return TestResult::Failed;
    }
    TestResult::Ok
}

/// INV-1 (balance half): counts at zero and hardware ops balanced.
fn assert_fully_released(nodes: &[(&crate::ClkCore, &FailOps)]) -> TestResult {
    for (core, ops) in nodes {
        let (p, e) = core.counts();
        if (p, e) != (0, 0) {
            ktest_println!("INV-1 FAIL: {} still holds refs ({},{})", core.name(), p, e);
            return TestResult::Failed;
        }
        let (pr, un, en, dis, _sr) = ops.counters();
        if pr != un || en != dis {
            ktest_println!(
                "INV-1 FAIL: {} unbalanced ({pr}/{un} prep, {en}/{dis} en)",
                core.name()
            );
            return TestResult::Failed;
        }
    }
    TestResult::Ok
}

/// INV-2: cached rate still equals recalc_rate(parent rate) everywhere.
fn assert_cache_coherent(nodes: &[&crate::ClkCore]) -> TestResult {
    for core in nodes {
        if !core.cache_coherent() {
            ktest_println!(
                "INV-2 FAIL: {} rate cache diverged from recalc",
                core.name()
            );
            return TestResult::Failed;
        }
    }
    TestResult::Ok
}

/// A three-level FailOps chain: root(1 GHz) -> mid(600 MHz) -> leaf(300 MHz).
struct Chain {
    root_ops: Arc<FailOps>,
    mid_ops: Arc<FailOps>,
    leaf_ops: Arc<FailOps>,
    root: Arc<crate::ClkCore>,
    mid: Arc<crate::ClkCore>,
    leaf: Arc<crate::ClkCore>,
}

impl Chain {
    fn new() -> Self {
        let root_ops = FailOps::new(1_000_000_000);
        let mid_ops = FailOps::new(600_000_000);
        let leaf_ops = FailOps::new(300_000_000);
        let root = ClkBuilder::new("inv_root", root_ops.clone().into_ops())
            .build()
            .unwrap();
        let mid = ClkBuilder::new("inv_mid", mid_ops.clone().into_ops())
            .parent(root.clone())
            .build()
            .unwrap();
        let leaf = ClkBuilder::new("inv_leaf", leaf_ops.clone().into_ops())
            .parent(mid.clone())
            .build()
            .unwrap();
        Self {
            root_ops,
            mid_ops,
            leaf_ops,
            root,
            mid,
            leaf,
        }
    }

    fn check(&self) -> TestResult {
        for r in [
            assert_refcount_implication(&self.leaf, &self.mid),
            assert_refcount_implication(&self.mid, &self.root),
            assert_cache_coherent(&[&self.root, &self.mid, &self.leaf]),
        ] {
            if r != TestResult::Ok {
                return r;
            }
        }
        TestResult::Ok
    }

    fn fully_released(&self) -> TestResult {
        assert_fully_released(&[
            (&self.root, &self.root_ops),
            (&self.mid, &self.mid_ops),
            (&self.leaf, &self.leaf_ops),
        ])
    }
}

// ---------------------------------------------------------------------------
// Happy-path invariant sweep
// ---------------------------------------------------------------------------

#[def_test(serial)]
fn invariants_hold_across_happy_path_sequence() {
    let c = Chain::new();
    let h = ClkHandle::new(c.leaf.clone());
    if c.check() != TestResult::Ok {
        return TestResult::Failed;
    }

    h.prepare_enable().unwrap();
    if c.check() != TestResult::Ok {
        return TestResult::Failed;
    }
    h.enable().unwrap(); // second ref: no new parent propagate
    if c.check() != TestResult::Ok {
        return TestResult::Failed;
    }
    h.set_rate(123_000_000).unwrap();
    if c.check() != TestResult::Ok {
        return TestResult::Failed;
    }
    h.disable();
    if c.check() != TestResult::Ok {
        return TestResult::Failed;
    }
    h.disable_unprepare();
    if c.fully_released() != TestResult::Ok {
        return TestResult::Failed;
    }
    if c.check() != TestResult::Ok {
        return TestResult::Failed;
    }
}

// ---------------------------------------------------------------------------
// Failure-injection matrix: arm each op at each level, run prepare_enable,
// assert invariants hold and no reference leaks; then a clean retry succeeds.
// ---------------------------------------------------------------------------

#[def_test(serial)]
fn prepare_enable_injects_failures_at_every_level() {
    for (level, name) in [("root", 0), ("mid", 1), ("leaf", 2)] {
        for op in ["prepare", "enable"] {
            let c = Chain::new();
            let ops = [c.root_ops.clone(), c.mid_ops.clone(), c.leaf_ops.clone()];
            ops[name].fail_next(op);

            let h = ClkHandle::new(c.leaf.clone());
            let res = h.prepare_enable();
            assert!(res.is_err(), "{level}.{op}: expected failure");
            // Whatever the failure point, the tree must stay consistent.
            if c.check() != TestResult::Ok {
                return TestResult::Failed;
            }
            // And the retry (failure is one-shot) must succeed and balance.
            h.prepare_enable().unwrap();
            if c.check() != TestResult::Ok {
                return TestResult::Failed;
            }
            h.disable_unprepare();
            if c.fully_released() != TestResult::Ok {
                return TestResult::Failed;
            }
        }
    }
}

#[def_test(serial)]
fn set_rate_failure_leaves_cache_and_refs_coherent() {
    let c = Chain::new();
    let h = ClkHandle::new(c.leaf.clone());
    h.prepare_enable().unwrap();
    let before = c.leaf.get_rate();

    c.leaf_ops.fail_next("set_rate");
    assert_eq!(h.set_rate(77_000_000).unwrap_err(), ClkError::Io);
    // Cache untouched by the failed program.
    assert_eq!(c.leaf.get_rate(), before);
    if c.check() != TestResult::Ok {
        return TestResult::Failed;
    }

    // Retry succeeds and coherence still holds.
    h.set_rate(77_000_000).unwrap();
    assert_eq!(c.leaf.get_rate(), 77_000_000);
    if c.check() != TestResult::Ok {
        return TestResult::Failed;
    }

    h.disable_unprepare();
    if c.fully_released() != TestResult::Ok {
        return TestResult::Failed;
    }
}

#[def_test(serial)]
fn upward_set_rate_failure_rolls_back_parent_programming() {
    // leaf carries SET_RATE_PARENT and asks its parent (mid) to change rate.
    let root_ops = FailOps::new(1_000_000_000);
    let root = ClkBuilder::new("up_root", root_ops.clone().into_ops())
        .build()
        .unwrap();
    let mid_ops = FailOps::new(500_000_000);
    let mid = ClkBuilder::new("up_mid", mid_ops.clone().into_ops())
        .parent(root.clone())
        .build()
        .unwrap();
    // A transparent pass-through ops on the leaf so every rate request
    // climbs to mid.
    let leaf = ClkBuilder::new("up_leaf", PassThroughOps.into_ops())
        .flags(ClkFlags::SET_RATE_PARENT)
        .parent(mid.clone())
        .build()
        .unwrap();
    let h = ClkHandle::new(leaf.clone());
    let mid_before = mid.get_rate();
    let leaf_before = leaf.get_rate();

    mid_ops.fail_next("set_rate");
    assert!(h.set_rate(250_000_000).is_err());
    // Neither mid nor leaf changed; caches coherent.
    assert_eq!(mid.get_rate(), mid_before);
    assert_eq!(leaf.get_rate(), leaf_before);
    if assert_cache_coherent(&[&root, &mid, &leaf]) != TestResult::Ok {
        return TestResult::Failed;
    }

    // Retry programs mid and the leaf follows.
    h.set_rate(250_000_000).unwrap();
    assert_eq!(mid.get_rate(), 250_000_000);
    assert_eq!(leaf.get_rate(), 250_000_000);
    if assert_cache_coherent(&[&root, &mid, &leaf]) != TestResult::Ok {
        return TestResult::Failed;
    }
}

/// A minimal software mux: transparent rate, parents selectable in software
/// (a `Cell` holds the selection; enough for reparent/reparent-failure tests
/// that do not need register semantics).
struct SoftMuxOps {
    sel: kspin::SpinNoIrq<u8>,
}
impl SoftMuxOps {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            sel: kspin::SpinNoIrq::new(0),
        })
    }

    fn into_ops(self: Arc<Self>) -> Arc<dyn ClkHwOps> {
        self
    }
}
impl ClkHwOps for SoftMuxOps {
    fn recalc_rate(&self, parent_rate: Rate) -> ClkResult<Rate> {
        Ok(parent_rate)
    }

    fn round_rate(&self, rate: Rate, parent_rate: Rate) -> ClkResult<RoundResult> {
        Ok(RoundResult {
            rate: parent_rate,
            parent_target: Some(rate),
        })
    }

    fn num_parents(&self) -> u8 {
        2
    }

    fn get_parent(&self) -> ClkResult<u8> {
        Ok(*self.sel.lock())
    }

    fn set_parent(&self, index: u8) -> ClkResult<()> {
        *self.sel.lock() = index;
        Ok(())
    }
}

/// A rate-transparent node: its rate is its parent's, and satisfying a request
/// always means asking the parent to change.
struct PassThroughOps;
impl PassThroughOps {
    fn into_ops(self) -> Arc<dyn ClkHwOps> {
        Arc::new(self)
    }
}
impl ClkHwOps for PassThroughOps {
    fn recalc_rate(&self, parent_rate: Rate) -> ClkResult<Rate> {
        Ok(parent_rate)
    }

    fn round_rate(&self, rate: Rate, parent_rate: Rate) -> ClkResult<RoundResult> {
        Ok(RoundResult {
            rate: parent_rate,
            parent_target: Some(rate),
        })
    }
}

#[def_test(serial)]
fn reparent_failure_rolls_back_and_migration_is_atomic() {
    // leaf selects between osc (CountingOps-like FailOps) and alt; failing
    // alt.prepare during the switch must leave the leaf on osc with osc's
    // references intact and alt untouched.
    let osc_ops = FailOps::new(24_000_000);
    let osc = ClkBuilder::new("rp_osc", osc_ops.clone().into_ops())
        .build()
        .unwrap();
    let alt_ops = FailOps::new(32_000_000);
    let alt = ClkBuilder::new("rp_alt", alt_ops.clone().into_ops())
        .build()
        .unwrap();
    // Use a PassThrough mux-like node: two parents, transparent rate.
    let mux_ops = SoftMuxOps::new();
    let leaf = ClkBuilder::new("rp_leaf", mux_ops.clone().into_ops())
        .parent(osc.clone())
        .parent(alt.clone())
        .build()
        .unwrap();
    let h = ClkHandle::new(leaf.clone());
    h.prepare_enable().unwrap();
    assert_eq!(osc_ops.counters(), (1, 0, 1, 0, 0));

    // Fail the new parent's prepare: the switch must abort before the mux.
    alt_ops.fail_next("prepare");
    assert!(h.set_parent(1).is_err());
    // Still on osc; osc keeps its references; alt never prepared.
    assert_eq!(osc_ops.counters(), (1, 0, 1, 0, 0));
    assert_eq!(alt_ops.counters(), (0, 0, 0, 0, 0));
    if assert_cache_coherent(&[&osc, &alt, &leaf]) != TestResult::Ok {
        return TestResult::Failed;
    }
    if assert_refcount_implication(&leaf, &osc) != TestResult::Ok {
        return TestResult::Failed;
    }

    // Retry succeeds and migrates the references atomically.
    h.set_parent(1).unwrap();
    assert_eq!(osc_ops.counters(), (1, 1, 1, 1, 0)); // torn down
    assert_eq!(alt_ops.counters(), (1, 0, 1, 0, 0)); // brought up
    if assert_refcount_implication(&leaf, &alt) != TestResult::Ok {
        return TestResult::Failed;
    }
    if assert_cache_coherent(&[&osc, &alt, &leaf]) != TestResult::Ok {
        return TestResult::Failed;
    }

    h.disable_unprepare();
    // Everything balances on the new parent chain.
    assert_eq!(alt_ops.counters(), (1, 1, 1, 1, 0));
    assert_eq!(osc_ops.counters(), (1, 1, 1, 1, 0));
}

// ---------------------------------------------------------------------------
// Matrix extensions: unprepare failure, orphan-active injection, notifier
// interplay with failure paths, SET_RATE_GATE+Busy retry loop.
// ---------------------------------------------------------------------------

#[def_test(serial)]
fn unprepare_failure_surfaces_error_but_completes_release() {
    // A failing hardware unprepare must surface the error while still
    // decrementing the count and propagating to the parent (the release is
    // committed; the caller learns the HW may lag).
    let root_ops = FailOps::new(1_000_000_000);
    let root = ClkBuilder::new("upf_root", root_ops.clone().into_ops())
        .build()
        .unwrap();
    let mid_ops = FailOps::new(500_000_000);
    let mid = ClkBuilder::new("upf_mid", mid_ops.clone().into_ops())
        .parent(root.clone())
        .build()
        .unwrap();
    let h = ClkHandle::new(mid.clone());
    h.prepare_enable().unwrap();

    mid_ops.fail_next("unprepare");
    assert_eq!(h.disable_unprepare(), ());
    // Software counts released, parent unwound; the error was dropped by the
    // convenience wrapper (documented) but the release completed.
    assert_eq!(mid.counts(), (0, 0));
    assert_eq!(root.counts(), (0, 0));
    assert_cache_coherent_ok(&[&root, &mid]);
}

fn assert_cache_coherent_ok(nodes: &[&crate::ClkCore]) {
    if assert_cache_coherent(nodes) != TestResult::Ok {
        panic!("INV-2");
    }
}

#[def_test(serial)]
fn orphan_active_then_rate_change_invariants() {
    // Build the child orphaned and ACTIVE (counts>0, no parent), then adopt
    // via the parent's registration, then reprogram the parent's rate: the
    // child must follow (INV-2 through adoption + cascade).
    let parent_ops = FailOps::new(400_000_000);
    let child = ClkBuilder::new("oa_child", SoftMuxOps::new().into_ops())
        .parent_name("oa_parent")
        .build()
        .unwrap();
    let h = ClkHandle::new(child.clone());
    // Active orphan: prepared/enabled with no parent.
    h.prepare_enable().unwrap();
    assert_eq!(child.counts(), (1, 1));
    assert_cache_coherent_ok(&[&child]);

    // Adopt: the parent registers and migrates the references.
    let parent = ClkBuilder::new("oa_parent", parent_ops.clone().into_ops())
        .build()
        .unwrap();
    assert_eq!(parent.counts(), (1, 1));
    assert_refcount_ok(&child, &parent);
    assert_cache_coherent_ok(&[&parent, &child]);

    // Reprogram the parent: the child's cache follows (B1 cascade).
    parent.set_rate(200_000_000).unwrap();
    assert_eq!(child.get_rate(), 200_000_000);
    assert_cache_coherent_ok(&[&parent, &child]);

    h.disable_unprepare();
    assert_eq!(parent.counts(), (0, 0));
    assert_eq!(child.counts(), (0, 0));
}

fn assert_refcount_ok(child: &crate::ClkCore, parent: &crate::ClkCore) {
    if assert_refcount_implication(child, parent) != TestResult::Ok {
        panic!("INV-1");
    }
}

#[def_test(serial)]
fn notifier_failure_interplay_keeps_invariants() {
    // A notifier veto aborts set_rate BEFORE any programming; invariants must
    // hold and a later un-vetoed change must succeed.
    let root_ops = FailOps::new(1_000_000_000);
    let root = ClkBuilder::new("ni_root", root_ops.clone().into_ops())
        .build()
        .unwrap();
    let leaf_ops = FailOps::new(500_000_000);
    let leaf = ClkBuilder::new("ni_leaf", leaf_ops.clone().into_ops())
        .parent(root.clone())
        .build()
        .unwrap();
    let h = ClkHandle::new(leaf.clone());
    h.prepare_enable().unwrap();
    let before = leaf.get_rate();

    let veto = Arc::new(SpinNoIrq::new(true));
    let v = veto.clone();
    h.register_notifier(Arc::new(move |_| {
        if *v.lock() {
            crate::NotifierResult::Stop
        } else {
            crate::NotifierResult::Proceed
        }
    }));
    assert_eq!(h.set_rate(123_000_000).unwrap_err(), ClkError::Aborted);
    assert_eq!(leaf.get_rate(), before);
    assert_cache_coherent_ok(&[&root, &leaf]);
    assert_refcount_ok(&leaf, &root);

    // Un-veto and retry.
    *veto.lock() = false;
    h.set_rate(123_000_000).unwrap();
    assert_eq!(leaf.get_rate(), 123_000_000);
    assert_cache_coherent_ok(&[&root, &leaf]);

    h.disable_unprepare();
    assert_eq!(leaf.counts(), (0, 0));
    assert_eq!(root.counts(), (0, 0));
    assert_eq!(leaf_ops.counters(), (1, 1, 1, 1, 1));
    assert_eq!(root_ops.counters(), (1, 1, 1, 1, 0));
}

#[def_test(serial)]
fn set_rate_gate_busy_then_retry_after_disable() {
    // SET_RATE_GATE: enabled -> Busy; disable -> change allowed; the counts
    // and counters still balance through the loop.
    let root_ops = FailOps::new(1_000_000_000);
    let root = ClkBuilder::new("g_root", root_ops.clone().into_ops())
        .build()
        .unwrap();
    let leaf_ops = FailOps::new(500_000_000);
    let leaf = ClkBuilder::new("g_leaf", leaf_ops.clone().into_ops())
        .parent(root.clone())
        .flags(ClkFlags::SET_RATE_GATE)
        .build()
        .unwrap();
    let h = ClkHandle::new(leaf.clone());

    h.prepare_enable().unwrap();
    assert_eq!(h.set_rate(250_000_000).unwrap_err(), ClkError::Busy);
    assert_cache_coherent_ok(&[&root, &leaf]);

    // Disable (keep prepared), change now succeeds, re-enable.
    h.disable();
    h.set_rate(250_000_000).unwrap();
    assert_eq!(leaf.get_rate(), 250_000_000);
    h.enable().unwrap();
    assert_cache_coherent_ok(&[&root, &leaf]);
    assert_refcount_ok(&leaf, &root);

    h.disable_unprepare();
    assert_eq!(leaf.counts(), (0, 0));
    assert_eq!(root.counts(), (0, 0));
}

#[def_test(serial)]
fn register_table_injection_matrix() {
    // A table whose LAST descriptor is invalid (zero fixed-factor) must fail
    // before any global state; the two earlier valid nodes never leak into
    // the provider table and the global list length is unchanged.
    let _serial = crate::tests::provider_serial_lock();
    let before = crate::clocks_len();
    let table = crate::tests::scratch_table(0x88);
    table.clear();
    crate::reset_providers();

    let reg: Arc<dyn crate::ClkRegAccess> = Arc::new(crate::tests::MockRegShared::new());
    let branches = [
        crate::ClkBranch::fixed_rate(0, "tm_a", 24_000_000),
        crate::ClkBranch::fixed_factor(1, "tm_b", 2, 1, &["tm_a"]),
        // Invalid: zero mult -> build_ops fails on the LAST descriptor.
        crate::ClkBranch::fixed_factor(2, "tm_bad", 0, 1, &["tm_a"]),
    ];
    assert!(crate::register_table(table, reg, &branches).is_err());
    assert_eq!(crate::clocks_len(), before);
    // Nothing published.
    assert!(crate::clk_get_by_index(0x88, 0).is_err());
    assert!(crate::clk_get_by_index(0x88, 1).is_err());
    crate::reset_providers();
}

#[def_test(serial)]
fn register_table_cycle_rollback_keeps_global_list_clean() {
    let _serial = crate::tests::provider_serial_lock();
    let before = crate::clocks_len();
    let table = crate::tests::scratch_table(0x89);
    table.clear();
    crate::reset_providers();

    let reg: Arc<dyn crate::ClkRegAccess> = Arc::new(crate::tests::MockRegShared::new());
    let branches = [
        crate::ClkBranch::gate(0, "cy_a", 0, 0, crate::GateFlags::SET_TO_ENABLE, &["cy_b"]),
        crate::ClkBranch::gate(1, "cy_b", 0, 1, crate::GateFlags::SET_TO_ENABLE, &["cy_a"]),
    ];
    assert!(crate::register_table(table, reg, &branches).is_err());
    assert_eq!(crate::clocks_len(), before);
    assert!(crate::clk_get_by_index(0x89, 0).is_err());
    crate::reset_providers();
}

// ---------------------------------------------------------------------------
// P1: runtime concurrency validation. The serial tests above prove the logic;
// these spawn contending kernel tasks and prove the two-lock model live:
// no deadlock, INV-1/INV-2 hold under real contention, counts balance.
// ---------------------------------------------------------------------------

#[def_test(serial)]
fn concurrent_gate_storm_balances() {
    const TASKS: u32 = 4;
    const ITERS: u32 = 64;

    let root_ops = FailOps::new(1_000_000_000);
    let root = ClkBuilder::new("cc_root", root_ops.clone().into_ops())
        .build()
        .unwrap();
    let mid_ops = FailOps::new(500_000_000);
    let mid = ClkBuilder::new("cc_mid", mid_ops.clone().into_ops())
        .parent(root.clone())
        .build()
        .unwrap();
    let leaf_ops = FailOps::new(125_000_000);
    let leaf = ClkBuilder::new("cc_leaf", leaf_ops.clone().into_ops())
        .parent(mid.clone())
        .build()
        .unwrap();

    let done = Arc::new(AtomicU32::new(0));
    let failures = Arc::new(AtomicU32::new(0));

    for _worker in 0..TASKS {
        let done = done.clone();
        let failures = failures.clone();
        let leaf = leaf.clone();
        ktask::spawn(move || {
            let handle = ClkHandle::new(leaf);
            let mut bad = 0u32;
            for _ in 0..ITERS {
                if handle.prepare_enable().is_err() {
                    bad += 1;
                }
                ktask::yield_now();
                handle.disable_unprepare();
                ktask::yield_now();
            }
            if bad > 0 {
                failures.fetch_add(bad, Ordering::Relaxed);
            }
            done.fetch_add(1, Ordering::Release);
        });
    }

    let mut spins = 0u32;
    while done.load(Ordering::Acquire) < TASKS {
        ktask::yield_now();
        spins += 1;
        if spins > 4_000_000 {
            ktest_println!("CONC-DEADLOCK: gate workers did not finish");
            return TestResult::Failed;
        }
    }
    if failures.load(Ordering::Acquire) > 0 {
        ktest_println!("CONC: gate iterations failed");
        return TestResult::Failed;
    }
    let nodes = [
        (&*root, &*root_ops),
        (&*mid, &*mid_ops),
        (&*leaf, &*leaf_ops),
    ];
    if assert_fully_released(&nodes) != TestResult::Ok {
        return TestResult::Failed;
    }
    let tree = [&*root, &*mid, &*leaf];
    if assert_cache_coherent(&tree) != TestResult::Ok {
        return TestResult::Failed;
    }
}

#[def_test(serial)]
fn concurrent_rate_vs_gate_keeps_invariants() {
    const TASKS: u32 = 4;
    const ITERS: u32 = 48;

    let root_ops = FailOps::new(1_000_000_000);
    let root = ClkBuilder::new("cr_root", root_ops.clone().into_ops())
        .build()
        .unwrap();
    let leaf_ops = FailOps::new(500_000_000);
    let leaf = ClkBuilder::new("cr_leaf", leaf_ops.clone().into_ops())
        .parent(root.clone())
        .build()
        .unwrap();

    let done = Arc::new(AtomicU32::new(0));
    let failures = Arc::new(AtomicU32::new(0));

    for worker in 0..TASKS {
        let done = done.clone();
        let failures = failures.clone();
        let leaf = leaf.clone();
        ktask::spawn(move || {
            let handle = ClkHandle::new(leaf);
            // Half the workers hammer set_rate/round_rate under CLK_LOCK; the
            // other half hammer the gate under CLK_ENABLE_LOCK — exactly the
            // two-lock interleaving the design must survive.
            let rate_worker = worker % 2 == 0;
            for i in 0..ITERS {
                if rate_worker {
                    let want = 100_000_000 + u64::from(i % 8) * 50_000_000;
                    if handle.set_rate(want).is_err() {
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                    let _ = handle.round_rate(123_456_789);
                } else {
                    if handle.prepare_enable().is_err() {
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                    handle.disable_unprepare();
                }
                ktask::yield_now();
            }
            done.fetch_add(1, Ordering::Release);
        });
    }

    let mut spins = 0u32;
    while done.load(Ordering::Acquire) < TASKS {
        ktask::yield_now();
        spins += 1;
        if spins > 4_000_000 {
            ktest_println!("CONC-DEADLOCK: mixed workers did not finish");
            return TestResult::Failed;
        }
    }
    if failures.load(Ordering::Acquire) > 0 {
        ktest_println!("CONC: operations failed under contention");
        return TestResult::Failed;
    }
    if leaf.get_rate() < 100_000_000 || leaf.get_rate() > 500_000_000 {
        ktest_println!("CONC: leaf rate {} out of band", leaf.get_rate());
        return TestResult::Failed;
    }
    let tree = [&*root, &*leaf];
    if assert_cache_coherent(&tree) != TestResult::Ok {
        return TestResult::Failed;
    }
}

#[def_test(serial)]
fn concurrent_reparent_migration_keeps_refcounts() {
    const TASKS: u32 = 4;
    const ITERS: u32 = 48;

    let osc_ops = FailOps::new(24_000_000);
    let osc = ClkBuilder::new("cp_osc", osc_ops.clone().into_ops())
        .build()
        .unwrap();
    let alt_ops = FailOps::new(32_000_000);
    let alt = ClkBuilder::new("cp_alt", alt_ops.clone().into_ops())
        .build()
        .unwrap();
    let mux = SoftMuxOps::new();
    let leaf = ClkBuilder::new("cp_leaf", mux.clone().into_ops())
        .parent(osc.clone())
        .parent(alt.clone())
        .build()
        .unwrap();

    let done = Arc::new(AtomicU32::new(0));
    let failures = Arc::new(AtomicU32::new(0));

    for worker in 0..TASKS {
        let done = done.clone();
        let failures = failures.clone();
        let leaf = leaf.clone();
        ktask::spawn(move || {
            let handle = ClkHandle::new(leaf);
            for i in 0..ITERS {
                if worker % 2 == 0 {
                    // Switcher: flips the mux under CLK_LOCK (with the ref
                    // migration nesting CLK_ENABLE_LOCK inside it).
                    if handle.set_parent((i % 2) as u8).is_err() {
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                } else {
                    // Gater: enable/disable under CLK_ENABLE_LOCK, racing the
                    // migration across both parents.
                    if handle.prepare_enable().is_err() {
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                    handle.disable_unprepare();
                }
                ktask::yield_now();
            }
            done.fetch_add(1, Ordering::Release);
        });
    }

    let mut spins = 0u32;
    while done.load(Ordering::Acquire) < TASKS {
        ktask::yield_now();
        spins += 1;
        if spins > 4_000_000 {
            ktest_println!("CONC-DEADLOCK: reparent workers did not finish");
            return TestResult::Failed;
        }
    }
    if failures.load(Ordering::Acquire) > 0 {
        ktest_println!("CONC: reparent/gating failed under contention");
        return TestResult::Failed;
    }
    // INV-1: both parents' hardware counters balance — the migration never
    // leaked a count on either side.
    let (op, oup, oe, od, _) = osc_ops.counters();
    if op != oup || oe != od {
        ktest_println!("CONC: osc unbalanced ({op}/{oup} prep, {oe}/{od} en)");
        return TestResult::Failed;
    }
    let (ap, aup, ae, ad, _) = alt_ops.counters();
    if ap != aup || ae != ad {
        ktest_println!("CONC: alt unbalanced ({ap}/{aup} prep, {ae}/{ad} en)");
        return TestResult::Failed;
    }
    let (lp, le) = leaf.counts();
    if lp != 0 || le != 0 {
        ktest_println!("CONC: leaf holds refs ({lp},{le}) after storm");
        return TestResult::Failed;
    }
    // INV-2: whichever parent is selected, the leaf cache matches it.
    let tree = [&*osc, &*alt, &*leaf];
    if assert_cache_coherent(&tree) != TestResult::Ok {
        return TestResult::Failed;
    }
}
