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
    ClkBuilder, ClkError, ClkEvent, ClkFlags, ClkHandle, ClkHwOps, ClkRegAccess, ClkTable, Divider,
    DividerFlags, FixedFactor, FixedRate, Gate, GateFlags, Mux, NotifierResult, basic::Composite,
    clk_get_by_index, clk_get_by_name, reset_clk_provider, set_clk_provider,
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

#[def_test]
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

#[def_test]
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

#[def_test]
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

#[def_test]
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

#[def_test]
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

#[def_test]
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

#[def_test]
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

#[def_test]
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

#[def_test]
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

#[def_test]
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

#[def_test]
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

#[def_test]
fn provider_resolves_by_name_and_index() {
    let _serial = PROVIDER_SERIAL.lock();
    static TABLE: ClkTable = ClkTable::new();
    TABLE.clear();
    reset_clk_provider();

    let osc = ClkBuilder::new("osc24m", FixedRate::new(24_000_000).into_ops())
        .build()
        .unwrap();
    TABLE.register(0, "osc24m", osc);

    set_clk_provider(&TABLE);

    let by_name = clk_get_by_name("osc24m").expect("name lookup should resolve");
    assert_eq!(by_name.get_rate(), 24_000_000);

    let by_index = clk_get_by_index(0).expect("index lookup should resolve");
    assert_eq!(by_index.name(), "osc24m");

    // Missing clocks yield None without panic.
    assert!(clk_get_by_name("nope").is_none());
    assert!(clk_get_by_index(99).is_none());

    reset_clk_provider();
}

#[def_test]
fn consumer_handle_prepare_enable_through_provider() {
    let _serial = PROVIDER_SERIAL.lock();
    static TABLE: ClkTable = ClkTable::new();
    TABLE.clear();
    reset_clk_provider();

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

    set_clk_provider(&TABLE);

    // A consumer resolves the clock the way a driver would, by DT index.
    let clk = clk_get_by_index(5).expect("uart_clk should resolve");
    assert_eq!(clk.is_enabled().unwrap(), false);
    clk.prepare_enable().unwrap();
    assert!(clk.is_enabled().unwrap());
    assert_eq!(reg.word(0) & (1 << 3), 1 << 3);

    reset_clk_provider();
}

// ---------------------------------------------------------------------------
// SET_RATE_PARENT propagation through a pass-through chain
// ---------------------------------------------------------------------------

#[def_test]
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

#[def_test]
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

#[def_test]
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

#[def_test]
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
