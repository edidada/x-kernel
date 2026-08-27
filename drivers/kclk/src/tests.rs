// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unit tests for the clock framework core and basic types.
//!
//! Register-backed types are exercised through an array-backed [`MockReg`]
//! ([`ClkRegAccess`]) so divider/gate/mux field math is validated without real
//! hardware. The provider registry is replaceable under `--cfg unittest`, so
//! each provider-scoped test installs its own [`ClkTable`].

use alloc::{sync::Arc, vec::Vec};

use kspin::SpinNoIrq;
use unittest::{assert_eq, def_test};

use crate::{
    ClkBranch, ClkBuilder, ClkError, ClkEvent, ClkFlags, ClkHandle, ClkHwOps, ClkRegAccess,
    ClkTable, Divider, DividerFlags, FixedFactor, FixedRate, Gate, GateFlags, Mux, NotifierResult,
    basic::Composite, clk_disable_unused, clk_get_by_index, clk_get_by_name, register_provider,
    register_table, reset_providers, unregister_provider,
};

/// Array-backed register accessor for testing field math.
struct MockReg {
    mem: SpinNoIrq<[u32; 8]>,
}

impl MockReg {
    fn new() -> Self {
        Self {
            mem: SpinNoIrq::new([0u32; 8]),
        }
    }

    fn word(&self, index: usize) -> u32 {
        self.mem.lock()[index]
    }
}

impl ClkRegAccess for MockReg {
    fn reg_read(&self, offset: usize) -> u32 {
        self.mem.lock()[offset / 4]
    }

    fn reg_write(&self, offset: usize, value: u32) {
        self.mem.lock()[offset / 4] = value;
    }
}

/// Records how many times each gate op fires, to assert reference-counted
/// propagation up the parent tree.
#[derive(Default)]
struct CountState {
    prepare: u32,
    unprepare: u32,
    enable: u32,
    disable: u32,
}

struct CountingOps {
    st: SpinNoIrq<CountState>,
}

impl CountingOps {
    fn new() -> Self {
        Self {
            st: SpinNoIrq::new(CountState::default()),
        }
    }

    fn snapshot(&self) -> (u32, u32, u32, u32) {
        let s = self.st.lock();
        (s.prepare, s.unprepare, s.enable, s.disable)
    }
}

impl crate::ClkHwOps for CountingOps {
    fn prepare(&self) -> crate::ClkResult<()> {
        self.st.lock().prepare += 1;
        Ok(())
    }

    fn unprepare(&self) -> crate::ClkResult<()> {
        self.st.lock().unprepare += 1;
        Ok(())
    }

    fn enable(&self) -> crate::ClkResult<()> {
        self.st.lock().enable += 1;
        Ok(())
    }

    fn disable(&self) {
        self.st.lock().disable += 1;
    }
}

// Serializes tests that touch the global provider registry.
static PROVIDER_SERIAL: SpinNoIrq<()> = SpinNoIrq::new(());

// ---------------------------------------------------------------------------
// Fixed-rate and fixed-factor nodes
// ---------------------------------------------------------------------------

#[def_test(serial)]
fn fixed_rate_reports_immutable_rate() {
    let osc = ClkBuilder::new("osc24m", FixedRate::new(24_000_000).into_ops())
        .build()
        .unwrap();
    assert_eq!(osc.get_rate(), 24_000_000);

    // A fixed clock cannot be reprogrammed.
    let err = osc.set_rate(48_000_000).unwrap_err();
    assert_eq!(err, ClkError::NotSupported);
    // round_rate yields the fixed rate regardless of the request.
    assert_eq!(osc.round_rate(99_999_999).unwrap(), 24_000_000);
}

#[def_test(serial)]
fn fixed_factor_scales_parent_rate() {
    let osc = ClkBuilder::new("osc", FixedRate::new(24_000_000).into_ops())
        .build()
        .unwrap();
    // mult=50, div=1 -> 1.2 GHz
    let pll = ClkBuilder::new("pll", FixedFactor::new(50, 1).unwrap().into_ops())
        .parent(osc.clone())
        .build()
        .unwrap();
    assert_eq!(pll.get_rate(), 1_200_000_000);

    // mult=1, div=6 -> 4 MHz from 24 MHz.
    let frac = ClkBuilder::new("frac", FixedFactor::new(1, 6).unwrap().into_ops())
        .parent(osc.clone())
        .build()
        .unwrap();
    assert_eq!(frac.get_rate(), 4_000_000);

    // Zero factors are rejected.
    assert!(FixedFactor::new(0, 1).is_err());
    assert!(FixedFactor::new(1, 0).is_err());
}

// ---------------------------------------------------------------------------
// Tree rate caching across three levels
// ---------------------------------------------------------------------------

#[def_test(serial)]
fn three_level_tree_caches_rates_from_leaf_to_root() {
    let osc = ClkBuilder::new("osc24m", FixedRate::new(24_000_000).into_ops())
        .build()
        .unwrap();
    let pll = ClkBuilder::new("pll1200", FixedFactor::new(50, 1).unwrap().into_ops())
        .parent(osc)
        .build()
        .unwrap();
    assert_eq!(pll.get_rate(), 1_200_000_000);

    // Divider at offset 0, 3-bit field, one-based; register reset value 0 -> div 1.
    let reg = Arc::new(MockReg::new());
    let cpu = ClkBuilder::new(
        "cpu_div",
        Divider::into_ops(reg, 0, 0, 3, None, DividerFlags::ONE_BASED),
    )
    .parent(pll)
    .build()
    .unwrap();
    // div == 1 -> pass-through.
    assert_eq!(cpu.get_rate(), 1_200_000_000);
}

// ---------------------------------------------------------------------------
// Divider set_rate programs the register and recalculates the rate
// ---------------------------------------------------------------------------

#[def_test(serial)]
fn divider_set_rate_writes_field_and_updates_rate() {
    let parent = ClkBuilder::new("p", FixedRate::new(1_200_000_000).into_ops())
        .build()
        .unwrap();
    let reg = Arc::new(MockReg::new());
    let div = ClkBuilder::new(
        "div",
        Divider::into_ops(reg.clone(), 0, 0, 3, None, DividerFlags::ONE_BASED),
    )
    .parent(parent)
    .build()
    .unwrap();

    // 1.2 GHz / 400 MHz = 3 -> one-based raw = 2.
    let actual = div.set_rate(400_000_000).unwrap();
    assert_eq!(actual, 400_000_000);
    assert_eq!(reg.word(0), 2);
    assert_eq!(div.get_rate(), 400_000_000);

    // Requesting 600 MHz -> div 2 -> raw 1.
    div.set_rate(600_000_000).unwrap();
    assert_eq!(reg.word(0), 1);
    assert_eq!(div.get_rate(), 600_000_000);

    // round_rate for 700 MHz on a 1.2 GHz parent -> closest div 2 (600 MHz).
    assert_eq!(div.round_rate(700_000_000).unwrap(), 600_000_000);
}

// ---------------------------------------------------------------------------
// Gate enable/disable toggles the register bit
// ---------------------------------------------------------------------------

#[def_test(serial)]
fn gate_enable_writes_bit_and_reports_state() {
    let reg = Arc::new(MockReg::new());
    let gate = Gate::new(reg.clone(), 0, 5, GateFlags::SET_TO_ENABLE);

    assert_eq!(gate.is_enabled().unwrap(), false);
    gate.enable().unwrap();
    assert_eq!(reg.word(0), 1 << 5);
    assert!(gate.is_enabled().unwrap());

    gate.disable();
    assert_eq!(reg.word(0), 0);
    assert_eq!(gate.is_enabled().unwrap(), false);
}

#[def_test(serial)]
fn gate_inverted_polarity_clears_bit_to_enable() {
    let reg = Arc::new(MockReg::new());
    // set_to_enable=false: the bit is an "is_disabled" flag, so enabling clears it.
    let gate = Gate::new(reg.clone(), 0, 2, GateFlags::empty());

    reg.mem.lock()[0] = 1 << 2; // initially disabled
    assert_eq!(gate.is_enabled().unwrap(), false);
    gate.enable().unwrap();
    assert_eq!(reg.word(0), 0);
    assert!(gate.is_enabled().unwrap());
}

// ---------------------------------------------------------------------------
// Mux parent selection
// ---------------------------------------------------------------------------

#[def_test(serial)]
fn mux_selects_parent_and_reports_index() {
    let reg = Arc::new(MockReg::new());
    let mux = Mux::new(reg.clone(), 0, 0, 0x3, None);

    assert_eq!(mux.num_parents(), 4);
    assert_eq!(mux.get_parent().unwrap(), 0);

    mux.set_parent(3).unwrap();
    assert_eq!(reg.word(0), 3);
    assert_eq!(mux.get_parent().unwrap(), 3);

    // Setting a parent beyond the field width is clamped by the field mask
    // (value 5 & 0x3 = 1), so get_parent returns the encoded value, not 5.
    mux.set_parent(5).unwrap();
    assert_eq!(mux.get_parent().unwrap(), 1);
}

// ---------------------------------------------------------------------------
// set_parent on a ClkCore refreshes the cached rate from the new parent
// ---------------------------------------------------------------------------

#[def_test(serial)]
fn core_set_parent_recomputes_rate_from_new_parent() {
    let osc24 = ClkBuilder::new("osc24", FixedRate::new(24_000_000).into_ops())
        .build()
        .unwrap();
    let osc32 = ClkBuilder::new("osc32", FixedRate::new(32_000_000).into_ops())
        .build()
        .unwrap();
    // A rate-transparent mux feeding a CPU clock.
    let mux_ops: Arc<dyn crate::ClkHwOps> =
        Arc::new(Mux::new(Arc::new(MockReg::new()), 0, 0, 0x3, None));
    let cpu = ClkBuilder::new("cpu", mux_ops)
        .parent(osc24.clone())
        .parent(osc32.clone())
        .build()
        .unwrap();

    // Initial selection is parent 0 (osc24).
    assert_eq!(cpu.get_rate(), 24_000_000);

    cpu.set_parent(1).unwrap();
    assert_eq!(cpu.get_rate(), 32_000_000);

    // Out-of-range parent is rejected.
    let err = cpu.set_parent(5).unwrap_err();
    assert_eq!(err, ClkError::InvalidParent);
}

// ---------------------------------------------------------------------------
// Two-phase gating propagates with reference counting
// ---------------------------------------------------------------------------

#[def_test(serial)]
fn prepare_enable_propagates_to_parent_once_per_refcount() {
    let parent_ops = Arc::new(CountingOps::new());
    let parent = ClkBuilder::new("parent", parent_ops.clone())
        .build()
        .unwrap();
    let child_ops = Arc::new(CountingOps::new());
    let child = ClkBuilder::new("child", child_ops.clone())
        .parent(parent)
        .build()
        .unwrap();
    let handle = ClkHandle::new(child);

    // First enable: parent prepared and enabled exactly once.
    handle.prepare_enable().unwrap();
    let (prep, unprep, en, dis) = parent_ops.snapshot();
    assert_eq!((prep, unprep, en, dis), (1, 0, 1, 0));

    // Second enable: reference count rises; no extra hardware call.
    handle.prepare_enable().unwrap();
    let (prep, _, en, _) = parent_ops.snapshot();
    assert_eq!((prep, en), (1, 1));

    // Tear down both references: parent disabled/unprepared exactly once.
    handle.disable_unprepare();
    handle.disable_unprepare();
    let (_, unprep, _, dis) = parent_ops.snapshot();
    assert_eq!((unprep, dis), (1, 1));
}

#[def_test(serial)]
fn prepare_failure_rolls_back_parent() {
    // A parent that succeeds, with a child whose prepare always fails.
    let parent_ops = Arc::new(CountingOps::new());
    let parent = ClkBuilder::new("parent", parent_ops.clone())
        .build()
        .unwrap();

    struct FailOps;
    impl crate::ClkHwOps for FailOps {
        fn prepare(&self) -> crate::ClkResult<()> {
            Err(ClkError::Io)
        }
    }
    let child = ClkBuilder::new("child", Arc::new(FailOps) as Arc<dyn crate::ClkHwOps>)
        .parent(parent)
        .build()
        .unwrap();

    let handle = ClkHandle::new(child);
    let err = handle.prepare_enable().unwrap_err();
    assert_eq!(err, ClkError::Io);

    // The parent was prepared, then rolled back.
    let (prep, unprep, ..) = parent_ops.snapshot();
    assert_eq!((prep, unprep), (1, 1));
}

// ---------------------------------------------------------------------------
// Composite node combines mux + divider + gate
// ---------------------------------------------------------------------------

#[def_test(serial)]
fn composite_dispatches_rate_and_gate_to_sub_nodes() {
    let reg = Arc::new(MockReg::new());
    let parent = ClkBuilder::new("p", FixedRate::new(1_000_000_000).into_ops())
        .build()
        .unwrap();

    let mux = Mux::into_ops(reg.clone(), 0, 0, 0x1, None);
    let div = Divider::into_ops(reg.clone(), 0, 4, 2, None, DividerFlags::ONE_BASED);
    let gate = Gate::into_ops(reg.clone(), 0, 8, GateFlags::SET_TO_ENABLE);

    let comp = ClkBuilder::new("comp", Composite::new(Some(mux), Some(div), Some(gate)))
        .parent(parent)
        .build()
        .unwrap();

    // Divider field reset value 0 -> one-based div 1 -> pass-through 1 GHz.
    assert_eq!(comp.get_rate(), 1_000_000_000);

    comp.set_rate(500_000_000).unwrap();
    assert_eq!(comp.get_rate(), 500_000_000);

    // Gate bit lives at offset 8 in the same word.
    assert_eq!(comp.is_enabled().unwrap(), false);
    comp.enable().unwrap();
    assert!(comp.is_enabled().unwrap());
    assert_eq!(reg.word(0) & (1 << 8), 1 << 8);
}

// ---------------------------------------------------------------------------
// Provider registry: name and index lookup
// ---------------------------------------------------------------------------

#[def_test(serial)]
fn provider_resolves_by_name_and_index() {
    let _serial = PROVIDER_SERIAL.lock();
    static TABLE: ClkTable = ClkTable::new(0x10);
    TABLE.clear();
    reset_providers();

    let osc = ClkBuilder::new("osc24m", FixedRate::new(24_000_000).into_ops())
        .build()
        .unwrap();
    TABLE.register(0, "osc24m", osc);

    register_provider(&TABLE);

    let by_name = clk_get_by_name("osc24m").expect("name lookup should resolve");
    assert_eq!(by_name.get_rate(), 24_000_000);

    let by_index = clk_get_by_index(0x10, 0).expect("index lookup should resolve");
    assert_eq!(by_index.name(), "osc24m");

    // Missing clocks yield None without panic.
    // Missing clock name (provider registered) -> NotFound.
    assert_eq!(clk_get_by_name("nope").unwrap_err(), ClkError::NotFound);
    // Missing index in a registered provider -> NotFound.
    assert_eq!(clk_get_by_index(0x10, 99).unwrap_err(), ClkError::NotFound);
    // Provider not registered -> NoProvider (defer, not permanent failure).
    assert_eq!(clk_get_by_index(0x99, 0).unwrap_err(), ClkError::NoProvider);

    reset_providers();
}

#[def_test(serial)]
fn consumer_handle_prepare_enable_through_provider() {
    let _serial = PROVIDER_SERIAL.lock();
    static TABLE: ClkTable = ClkTable::new(0x20);
    TABLE.clear();
    reset_providers();

    let reg = Arc::new(MockReg::new());
    let parent = ClkBuilder::new("p", FixedRate::new(100_000_000).into_ops())
        .build()
        .unwrap();
    let gate = ClkBuilder::new(
        "uart_clk",
        Gate::into_ops(reg.clone(), 0, 3, GateFlags::SET_TO_ENABLE),
    )
    .parent(parent)
    .build()
    .unwrap();
    TABLE.register(5, "uart_clk", gate);

    register_provider(&TABLE);

    // A consumer resolves the clock the way a driver would, by DT index.
    let clk = clk_get_by_index(0x20, 5).expect("uart_clk should resolve");
    assert_eq!(clk.is_enabled().unwrap(), false);
    clk.prepare_enable().unwrap();
    assert!(clk.is_enabled().unwrap());
    assert_eq!(reg.word(0) & (1 << 3), 1 << 3);

    reset_providers();
}

// ---------------------------------------------------------------------------
// SET_RATE_PARENT propagation through a pass-through chain
// ---------------------------------------------------------------------------

#[def_test(serial)]
fn set_rate_propagates_to_parent_when_flag_set() {
    let reg = Arc::new(MockReg::new());
    // A parent divider that can change its own rate.
    let root = ClkBuilder::new("root", FixedRate::new(1_000_000_000).into_ops())
        .build()
        .unwrap();
    let div = ClkBuilder::new(
        "div",
        Divider::into_ops(reg, 0, 0, 4, None, DividerFlags::ONE_BASED),
    )
    .parent(root)
    .build()
    .unwrap();

    // A pass-through mux carrying SET_RATE_PARENT; it has no own set_rate, so a
    // rate request must climb to the divider parent.
    let mux = Mux::into_ops(Arc::new(MockReg::new()), 0, 0, 0x1, None);
    let passthrough = ClkBuilder::new("pass", mux)
        .flags(ClkFlags::SET_RATE_PARENT)
        .parent(div.clone())
        .build()
        .unwrap();

    // Request 200 MHz: the divider (parent of the mux) reprograms to div 5.
    let actual = passthrough.set_rate(200_000_000).unwrap();
    assert_eq!(actual, 200_000_000);
    assert_eq!(passthrough.get_rate(), 200_000_000);
}

// ---------------------------------------------------------------------------
// Rate-change notifiers
// ---------------------------------------------------------------------------

#[def_test(serial)]
fn notifier_fires_pre_and_post_on_set_rate() {
    let reg = Arc::new(MockReg::new());
    let parent = ClkBuilder::new("p", FixedRate::new(1_000_000_000).into_ops())
        .build()
        .unwrap();
    let div = ClkBuilder::new(
        "div",
        Divider::into_ops(reg, 0, 0, 4, None, DividerFlags::ONE_BASED),
    )
    .parent(parent)
    .build()
    .unwrap();
    let clk = ClkHandle::new(div);

    // Record every event delivered to the subscriber.
    let log: Arc<SpinNoIrq<Vec<ClkEvent>>> = Arc::new(SpinNoIrq::new(Vec::new()));
    let log_cb = log.clone();
    clk.register_notifier(Arc::new(move |event| {
        log_cb.lock().push(*event);
        NotifierResult::Proceed
    }));

    // 1 GHz / 2 = 500 MHz.
    clk.set_rate(500_000_000).unwrap();

    let events = log.lock();
    assert_eq!(events.len(), 2);
    match events[0] {
        ClkEvent::PreRateChange { old, new } => {
            assert_eq!((old, new), (1_000_000_000, 500_000_000));
        }
        _ => panic!("expected PreRateChange first, got {:?}", events[0]),
    }
    match events[1] {
        ClkEvent::PostRateChange { old, new } => {
            assert_eq!((old, new), (1_000_000_000, 500_000_000));
        }
        _ => panic!("expected PostRateChange second, got {:?}", events[1]),
    }
}

#[def_test(serial)]
fn notifier_veto_aborts_set_rate() {
    let reg = Arc::new(MockReg::new());
    let parent = ClkBuilder::new("p", FixedRate::new(1_000_000_000).into_ops())
        .build()
        .unwrap();
    let div = ClkBuilder::new(
        "div",
        Divider::into_ops(reg, 0, 0, 4, None, DividerFlags::ONE_BASED),
    )
    .parent(parent)
    .build()
    .unwrap();
    let clk = ClkHandle::new(div);

    // Veto every upcoming rate change.
    clk.register_notifier(Arc::new(|event| {
        if matches!(event, ClkEvent::PreRateChange { .. }) {
            NotifierResult::Stop
        } else {
            NotifierResult::Proceed
        }
    }));

    let err = clk.set_rate(500_000_000).unwrap_err();
    assert_eq!(err, ClkError::Aborted);
    // The rate is unchanged after an aborted change.
    assert_eq!(clk.get_rate(), 1_000_000_000);
}

// ---------------------------------------------------------------------------
// Divider-aware upward propagation: a FixedFactor asks its divider parent to
// reprogram so it can reach the requested rate.
// ---------------------------------------------------------------------------

#[def_test(serial)]
fn fixed_factor_propagates_rate_to_parent_via_flag() {
    let reg = Arc::new(MockReg::new());
    let root = ClkBuilder::new("root", FixedRate::new(1_000_000_000).into_ops())
        .build()
        .unwrap();
    let div = ClkBuilder::new(
        "div",
        Divider::into_ops(reg, 0, 0, 4, None, DividerFlags::ONE_BASED),
    )
    .parent(root)
    .build()
    .unwrap();
    // mult=1, div=2: to output 100 MHz the parent must supply 200 MHz.
    let ff = ClkBuilder::new("ff", FixedFactor::new(1, 2).unwrap().into_ops())
        .flags(ClkFlags::SET_RATE_PARENT)
        .parent(div.clone())
        .build()
        .unwrap();

    let actual = ff.set_rate(100_000_000).unwrap();
    assert_eq!(actual, 100_000_000);
    assert_eq!(ff.get_rate(), 100_000_000);
    // The divider parent was reprogrammed to supply 200 MHz.
    assert_eq!(div.get_rate(), 200_000_000);
}

// ---------------------------------------------------------------------------
// Multiple providers: two controllers each publishing index 0 must not collide.
// Validates the multi-provider fix — the DT phandle disambiguates providers.
// ---------------------------------------------------------------------------

#[def_test(serial)]
fn two_providers_index_zero_do_not_collide() {
    let _serial = PROVIDER_SERIAL.lock();
    static CCU: ClkTable = ClkTable::new(0x10);
    static PMUCRU: ClkTable = ClkTable::new(0x11);
    CCU.clear();
    PMUCRU.clear();
    reset_providers();

    // Both controllers publish their own index-0 clock at different rates.
    let ccu_clk = ClkBuilder::new("ccu_osc", FixedRate::new(24_000_000).into_ops())
        .build()
        .unwrap();
    let pmu_clk = ClkBuilder::new("pmu_osc", FixedRate::new(32_768).into_ops())
        .build()
        .unwrap();
    CCU.register(0, "ccu_osc", ccu_clk);
    PMUCRU.register(0, "pmu_osc", pmu_clk);

    register_provider(&CCU);
    register_provider(&PMUCRU);

    // Same index 0, different provider id -> different clocks, no overwrite.
    assert_eq!(clk_get_by_index(0x10, 0).unwrap().get_rate(), 24_000_000);
    assert_eq!(clk_get_by_index(0x11, 0).unwrap().get_rate(), 32_768);

    // Names stay globally unique and resolve regardless of provider.
    assert_eq!(clk_get_by_name("pmu_osc").unwrap().get_rate(), 32_768);

    // Unregistering one provider removes only its clocks.
    unregister_provider(0x10);
    // After unregistering provider 0x10, its index 0 -> NoProvider.
    assert_eq!(clk_get_by_index(0x10, 0).unwrap_err(), ClkError::NoProvider);
    assert!(clk_get_by_index(0x11, 0).is_ok());

    reset_providers();
}

// ---------------------------------------------------------------------------
// Additive API surface: split two-phase, get_parent, rate range, notifier
// unregister, init hook, and clk_disable_unused.
// ---------------------------------------------------------------------------

#[def_test(serial)]
fn separate_prepare_enable_apis_work() {
    let ops = Arc::new(CountingOps::new());
    let clk = ClkHandle::new(ClkBuilder::new("c", ops.clone()).build().unwrap());

    clk.prepare().unwrap();
    clk.enable().unwrap();
    let (prep, _, en, _) = ops.snapshot();
    assert_eq!((prep, en), (1, 1));

    // Unbalanced fast-path only: disable/enable without touching prepare.
    clk.disable();
    clk.enable().unwrap();
    let (_, _, en, dis) = ops.snapshot();
    assert_eq!((en, dis), (2, 1));

    clk.disable();
    clk.unprepare();
    let (_, unprep, _, dis) = ops.snapshot();
    assert_eq!((unprep, dis), (1, 2));
}

#[def_test(serial)]
fn get_parent_returns_parent_node() {
    let osc = ClkBuilder::new("osc", FixedRate::new(24_000_000).into_ops())
        .build()
        .unwrap();
    let child = ClkBuilder::new("child", FixedFactor::new(2, 1).unwrap().into_ops())
        .parent(osc.clone())
        .build()
        .unwrap();
    let parent = child.get_parent().expect("child has a parent");
    assert_eq!(parent.get_rate(), 24_000_000);
    // A root has no parent.
    assert!(osc.get_parent().is_none());
}

#[def_test(serial)]
fn rate_range_clamps_set_and_round() {
    let reg = Arc::new(MockReg::new());
    let root = ClkBuilder::new("root", FixedRate::new(1_000_000_000).into_ops())
        .build()
        .unwrap();
    let div = ClkBuilder::new(
        "div",
        Divider::into_ops(reg, 0, 0, 4, None, DividerFlags::ONE_BASED),
    )
    .parent(root)
    .build()
    .unwrap();

    div.set_rate_range(200_000_000, 400_000_000).unwrap();
    assert_eq!(div.get_rate_range(), (200_000_000, 400_000_000));

    let actual = div.set_rate(100_000_000).unwrap();
    assert!((200_000_000..=400_000_000).contains(&actual));
    let rounded = div.round_rate(50_000_000).unwrap();
    assert!((200_000_000..=400_000_000).contains(&rounded));

    // An inverted range is rejected.
    assert_eq!(
        div.set_rate_range(400_000_000, 200_000_000),
        Err(ClkError::InvalidRate)
    );
}

#[def_test(serial)]
fn notifier_unregister_stops_delivery() {
    let reg = Arc::new(MockReg::new());
    let root = ClkBuilder::new("root", FixedRate::new(1_000_000_000).into_ops())
        .build()
        .unwrap();
    let div = ClkBuilder::new(
        "div",
        Divider::into_ops(reg, 0, 0, 4, None, DividerFlags::ONE_BASED),
    )
    .parent(root)
    .build()
    .unwrap();
    let clk = ClkHandle::new(div);

    let log: Arc<SpinNoIrq<Vec<ClkEvent>>> = Arc::new(SpinNoIrq::new(Vec::new()));
    let log_cb = log.clone();
    let id = clk.register_notifier(Arc::new(move |event| {
        log_cb.lock().push(*event);
        NotifierResult::Proceed
    }));

    clk.set_rate(500_000_000).unwrap();
    assert_eq!(log.lock().len(), 2); // Pre + Post

    clk.unregister_notifier(id);
    clk.set_rate(400_000_000).unwrap();
    assert_eq!(log.lock().len(), 2); // no further events
}

#[def_test(serial)]
fn init_hook_runs_at_build() {
    let flag: Arc<SpinNoIrq<bool>> = Arc::new(SpinNoIrq::new(false));

    struct InitOps(Arc<SpinNoIrq<bool>>);
    impl ClkHwOps for InitOps {
        fn init(&self) -> crate::ClkResult<()> {
            *self.0.lock() = true;
            Ok(())
        }
    }

    let _ = ClkBuilder::new("c", Arc::new(InitOps(flag.clone())) as Arc<dyn ClkHwOps>)
        .build()
        .unwrap();
    assert!(*flag.lock());
}

#[def_test(serial)]
fn clk_disable_unused_gates_idle_and_skips_critical() {
    let reg = Arc::new(MockReg::new());
    reg.mem.lock()[0] = 1; // bootloader left the gate on
    let idle = ClkBuilder::new(
        "idle",
        Gate::into_ops(reg.clone(), 0, 0, GateFlags::SET_TO_ENABLE),
    )
    .build()
    .unwrap();
    assert!(idle.is_enabled().unwrap());

    let reg2 = Arc::new(MockReg::new());
    reg2.mem.lock()[0] = 1;
    let _critical = ClkBuilder::new(
        "crit",
        Gate::into_ops(reg2.clone(), 0, 0, GateFlags::SET_TO_ENABLE),
    )
    .flags(ClkFlags::IS_CRITICAL)
    .build()
    .unwrap();

    clk_disable_unused();
    assert_eq!(reg.mem.lock()[0], 0);
    assert_eq!(reg2.mem.lock()[0], 1);
}

#[def_test(serial)]
fn mux_determine_rate_selects_closest_parent() {
    let osc24 = ClkBuilder::new("osc24", FixedRate::new(24_000_000).into_ops())
        .build()
        .unwrap();
    let osc32 = ClkBuilder::new("osc32", FixedRate::new(32_000_000).into_ops())
        .build()
        .unwrap();
    let mux = Mux::into_ops(Arc::new(MockReg::new()), 0, 0, 0x1, None);
    let clk = ClkBuilder::new("mux", mux)
        .parent(osc24)
        .parent(osc32)
        .build()
        .unwrap();

    assert_eq!(clk.get_rate(), 24_000_000);
    // 30 MHz is closer to the 32 MHz parent: the mux switches sources.
    let actual = clk.set_rate(30_000_000).unwrap();
    assert_eq!(actual, 32_000_000);
    assert_eq!(clk.get_parent().unwrap().get_rate(), 32_000_000);
}

#[def_test(serial)]
fn orphan_is_adopted_when_parent_registers() {
    let child = ClkBuilder::new("child", FixedFactor::new(2, 1).unwrap().into_ops())
        .parent_name("pll")
        .build()
        .unwrap();
    assert_eq!(child.get_rate(), 0);

    let _pll = ClkBuilder::new("pll", FixedRate::new(24_000_000).into_ops())
        .build()
        .unwrap();

    assert_eq!(child.get_parent().unwrap().get_rate(), 24_000_000);
    assert_eq!(child.get_rate(), 48_000_000);
}

#[def_test(serial)]
fn register_table_builds_tree_by_name_in_any_order() {
    let _serial = PROVIDER_SERIAL.lock();
    static TABLE: ClkTable = ClkTable::new(0x50);
    TABLE.clear();
    reset_providers();

    let reg: Arc<dyn ClkRegAccess> = Arc::new(MockReg::new());
    // Children listed BEFORE parents — order-independent, parents by name.
    let branches = [
        ClkBranch::gate(2, "uart_clk", 0, 7, GateFlags::SET_TO_ENABLE, &["pll1200"]),
        ClkBranch::fixed_factor(1, "pll1200", 50, 1, &["osc24m"]),
        ClkBranch::fixed_rate(0, "osc24m", 24_000_000),
    ];
    register_table(&TABLE, reg, &branches).unwrap();
    register_provider(&TABLE);

    assert_eq!(clk_get_by_index(0x50, 0).unwrap().get_rate(), 24_000_000);
    assert_eq!(clk_get_by_index(0x50, 1).unwrap().get_rate(), 1_200_000_000);
    // The gate transparently passes the pll rate through.
    assert_eq!(clk_get_by_index(0x50, 2).unwrap().get_rate(), 1_200_000_000);

    reset_providers();
}

#[def_test(serial)]
fn children_back_edge_tracks_selected_parent() {
    let osc = ClkBuilder::new("osc", FixedRate::new(24_000_000).into_ops())
        .build()
        .unwrap();
    let alt = ClkBuilder::new("alt", FixedRate::new(32_000).into_ops())
        .build()
        .unwrap();
    // A two-parent mux: child selects osc initially.
    let reg = Arc::new(MockReg::new());
    let mux = Mux::into_ops(reg, 0, 0, 0x1, None);
    let child = ClkBuilder::new("child", mux)
        .parent(osc.clone())
        .parent(alt.clone())
        .build()
        .unwrap();
    // The currently selected parent (osc) sees one live child.
    assert_eq!(osc.children_count(), 1);
    assert_eq!(alt.children_count(), 0);
    // Switch the mux: the back-edge moves from osc to alt.
    child.set_parent(1).unwrap();
    assert_eq!(osc.children_count(), 0);
    assert_eq!(alt.children_count(), 1);
    // Switch back.
    child.set_parent(0).unwrap();
    assert_eq!(osc.children_count(), 1);
    assert_eq!(alt.children_count(), 0);
}

#[def_test(serial)]
fn parent_rate_change_cascades_to_descendant_cache() {
    // osc(1GHz fixed) -> div(programmable divider) -> child(÷2 fixed factor).
    // Reprogramming div changes its rate; the grandchild child must pick up the
    // new value without an explicit set_rate on it.
    let reg = Arc::new(MockReg::new());
    let osc = ClkBuilder::new("osc", FixedRate::new(1_000_000_000).into_ops())
        .build()
        .unwrap();
    let div = ClkBuilder::new(
        "div",
        Divider::into_ops(reg, 0, 0, 4, None, DividerFlags::ONE_BASED),
    )
    .parent(osc)
    .build()
    .unwrap();
    let child = ClkBuilder::new("child", FixedFactor::new(1, 2).unwrap().into_ops())
        .parent(div.clone())
        .build()
        .unwrap();
    assert_eq!(div.get_rate(), 1_000_000_000); // div field 0 -> div1
    assert_eq!(child.get_rate(), 500_000_000); // ÷2 of 1 GHz

    // Reprogram div; child is a descendant and must cascade-refresh.
    div.set_rate(200_000_000).unwrap(); // div5 -> 200 MHz
    assert_eq!(div.get_rate(), 200_000_000);
    assert_eq!(child.get_rate(), 100_000_000); // ÷2 of 200 MHz, cascaded
}

#[def_test(serial)]
fn set_parent_migrates_prepare_enable_refs() {
    // Two counting parents; an active mux switches from osc to alt. The
    // prepare/enable refs this node holds must move from osc to alt (new up
    // before the switch, old down after), not leak on either side.
    let osc_ops = Arc::new(CountingOps::new());
    let alt_ops = Arc::new(CountingOps::new());
    let osc = ClkBuilder::new("osc", osc_ops.clone() as Arc<dyn ClkHwOps>)
        .build()
        .unwrap();
    let alt = ClkBuilder::new("alt", alt_ops.clone() as Arc<dyn ClkHwOps>)
        .build()
        .unwrap();
    let reg = Arc::new(MockReg::new());
    let mux = Mux::into_ops(reg, 0, 0, 0x1, None);
    let child = ClkBuilder::new("child", mux)
        .parent(osc.clone())
        .parent(alt.clone())
        .build()
        .unwrap();
    let handle = ClkHandle::new(child);

    handle.prepare_enable().unwrap(); // osc prepared+enabled once
    assert_eq!(osc_ops.snapshot(), (1, 0, 1, 0));

    // Switch the active mux to alt: refs migrate.
    handle.set_parent(1).unwrap();
    // osc is torn down (unprepare+disable), alt is brought up (prepare+enable).
    assert_eq!(osc_ops.snapshot(), (1, 1, 1, 1));
    assert_eq!(alt_ops.snapshot(), (1, 0, 1, 0));

    // Switch back: refs migrate the other way.
    handle.set_parent(0).unwrap();
    assert_eq!(osc_ops.snapshot(), (2, 1, 2, 1)); // prepared again
    assert_eq!(alt_ops.snapshot(), (1, 1, 1, 1)); // torn down

    handle.disable_unprepare();
    // The last ref on osc is released.
    assert_eq!(osc_ops.snapshot(), (2, 2, 2, 2));
}

#[def_test(serial)]
fn hiword_mask_gate_writes_atomic_single_word() {
    // bit 5, SET_TO_ENABLE, hiword-mask: a single reg_write of (mask<<16)|value.
    let reg = Arc::new(MockReg::new());
    let gate = Gate::new(
        reg.clone(),
        0,
        5,
        GateFlags::SET_TO_ENABLE | GateFlags::HIWORD_MASK,
    );

    gate.enable().unwrap();
    // mask = 1<<5 = 0x20; value = 0x20; written = 0x20<<16 | 0x20 = 0x200020.
    assert_eq!(reg.word(0), 0x0020_0020);

    gate.disable();
    // value = 0; written = 0x20<<16 | 0 = 0x200000.
    assert_eq!(reg.word(0), 0x0020_0000);

    // is_enabled reads back the low word as before.
    assert!(!gate.is_enabled().unwrap());
}

#[def_test(serial)]
fn rate_range_aggregates_per_consumer_and_relaxes_on_drop() {
    let reg = Arc::new(MockReg::new());
    let root = ClkBuilder::new("root", FixedRate::new(1_000_000_000).into_ops())
        .build()
        .unwrap();
    let div = ClkBuilder::new(
        "div",
        Divider::into_ops(reg, 0, 0, 4, None, DividerFlags::ONE_BASED),
    )
    .parent(root)
    .build()
    .unwrap();

    let h1 = ClkHandle::new(div.clone());
    let h2 = ClkHandle::new(div.clone());
    h1.set_rate_range(100_000_000, 400_000_000).unwrap();
    assert_eq!(div.get_rate_range(), (100_000_000, 400_000_000));
    // h2 narrows the effective range to the tightest intersection.
    h2.set_rate_range(200_000_000, 300_000_000).unwrap();
    assert_eq!(div.get_rate_range(), (200_000_000, 300_000_000));
    // Dropping h2 removes its vote; the range relaxes back to h1's.
    drop(h2);
    assert_eq!(div.get_rate_range(), (100_000_000, 400_000_000));
    // h1 still holds its vote; dropping it removes the last constraint.
    drop(h1);
    assert_eq!(div.get_rate_range(), (0, u64::MAX));
}

#[def_test(serial)]
fn notifier_is_unregistered_when_handle_drops() {
    let reg = Arc::new(MockReg::new());
    let root = ClkBuilder::new("root", FixedRate::new(1_000_000_000).into_ops())
        .build()
        .unwrap();
    let div = ClkBuilder::new(
        "div",
        Divider::into_ops(reg, 0, 0, 4, None, DividerFlags::ONE_BASED),
    )
    .parent(root)
    .build()
    .unwrap();

    let fired: Arc<SpinNoIrq<u32>> = Arc::new(SpinNoIrq::new(0));
    {
        let h = ClkHandle::new(div.clone());
        let cb = fired.clone();
        h.register_notifier(Arc::new(move |_| {
            *cb.lock() += 1;
            NotifierResult::Proceed
        }));
        div.set_rate(500_000_000).unwrap();
        assert_eq!(*fired.lock(), 2); // Pre + Post delivered
        // Dropping the handle unregisters its notifier.
    }
    div.set_rate(250_000_000).unwrap();
    assert_eq!(*fired.lock(), 2); // no further events after drop
}
