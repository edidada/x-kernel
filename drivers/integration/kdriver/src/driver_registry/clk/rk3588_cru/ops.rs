// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Hand-written RK3588 clock hardware operations — the SoC-specific clock
//! types the Linux rockchip driver layers on top of the generic CCF helpers,
//! translated onto `kclk::ClkHwOps`:
//!
//! - [`Rk3588Pll`] — `clk-pll.c` `pll_rk3588` / `pll_rk3588_core`
//! - [`ArmClk`] — `clk-cpu.c` `rockchip_cpuclk` (armclk_l/b01/b23)
//! - [`FracBranch`] — `clk.c` `rockchip_clk_register_frac_branch` +
//!   `clk-fractional-divider.c` (+ the remux-on-rate-change dance)
//! - [`HalfDivComposite`] — `clk-half-divider.c`
//!
//! Every hardware sequence mirrors the reference line-by-line; the numbered
//! steps in comments refer to the listing in
//! `docs/ai/skills/linux-driver-port/examples/rk3588-cru/portrait.md`.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::cell::Cell;

use kclk::{
    ClkBranch, ClkError, ClkEvent, ClkFlags, ClkHwOps, ClkIndex, ClkRegAccess, ClkResult, Mux,
    MuxFlags, NotifierResult, Rate, RoundResult,
};
use kspin::SpinNoIrq;

use super::{
    CustomSpec, Field, FracChild, PllKind, PllRate, PllSpec,
    data::{self, CpuRegData},
};

/// `HIWORD_UPDATE(val, mask, shift)`: value in the low half, write-enable mask
/// in the high half — the Rockchip CRU write convention (clk.h:26).
const fn hiword_update(val: u32, mask: u32, shift: u8) -> u32 {
    (val << shift) | (mask << (shift + 16))
}

// ---------------------------------------------------------------------------
// RK3588 PLL (clk-pll.c:853-1205)
// ---------------------------------------------------------------------------

/// Register offsets inside the PLL parameter block (RK3588_PLLCON(i) = i*4).
const PLLCON_CON0: usize = 0x0;
const PLLCON_CON1: usize = 0x4;
const PLLCON_CON2: usize = 0x8;
const PLLCON_CON6: usize = 0x18;

/// RK3588_PLLCON0_M: feedback divider m, [9:0].
const PLLCON0_M_MASK: u32 = 0x3ff;
/// RK3588_PLLCON1_P: post divider p, [5:0].
const PLLCON1_P_MASK: u32 = 0x3f;
/// RK3588_PLLCON1_S: output divide exponent s, [8:6].
const PLLCON1_S_MASK: u32 = 0x7;
const PLLCON1_S_SHIFT: u8 = 6;
/// RK3588_PLLCON2_K: fractional numerator k over 65535, [15:0].
const PLLCON2_K_MASK: u32 = 0xffff;
/// RK3588_PLLCON1_PWRDOWN: PLL power gate, bit 13.
const PLLCON1_PWRDOWN: u32 = 1 << 13;
/// RK3588_PLLCON6_LOCK_STATUS: lock status, bit 15.
const PLLCON6_LOCK_STATUS: u32 = 1 << 15;

/// Mode mux encoding shared by all RK3588 PLLs (clk.h:20-23): 0 = slow
/// (xin24m), 1 = normal (pll output), 2 = deep slow.
const PLL_MODE_SLOW: u8 = 0;
const PLL_MODE_NORM: u8 = 1;

/// Linux lock-wait budget: typical 250µs, "very safe" 1000µs
/// (clk-pll.c:868-872), polled in 10µs busy-wait slices.
const PLL_LOCK_TIMEOUT_US: u64 = 1000;
const PLL_LOCK_POLL_US: u64 = 10;

/// One RK3588 PLL (the internal `pll_<name>` node).
pub(crate) struct Rk3588Pll {
    spec: &'static PllSpec,
    reg: Arc<dyn ClkRegAccess>,
}

impl Rk3588Pll {
    pub fn new(spec: &'static PllSpec, reg: Arc<dyn ClkRegAccess>) -> Self {
        Self { spec, reg }
    }

    fn con(&self, off: usize) -> u32 {
        self.reg.reg_read(self.spec.con_offset + off)
    }

    fn set_con(&self, off: usize, value: u32) {
        self.reg.reg_write(self.spec.con_offset + off, value)
    }

    /// Read m/p/s/k from the parameter registers (clk-pll.c:887-901).
    fn get_params(&self) -> (u32, u32, u32, u32) {
        let m = self.con(PLLCON_CON0) & PLLCON0_M_MASK;
        let con1 = self.con(PLLCON_CON1);
        let p = con1 & PLLCON1_P_MASK;
        let s = (con1 >> PLLCON1_S_SHIFT) & PLLCON1_S_MASK;
        let k = self.con(PLLCON_CON2) & PLLCON2_K_MASK;
        (m, p, s, k)
    }

    /// `rate = prate*m/p (+ prate*k/(p*65535)) >> s` (clk-pll.c:903-920).
    fn calc_rate(&self, prate: Rate, m: u32, p: u32, s: u32, k: u32) -> Rate {
        let mut rate = prate.saturating_mul(m as u64) / p.max(1) as u64;
        if k != 0 {
            let frac = prate.saturating_mul(k as u64) / ((p.max(1) as u64) * 65535);
            rate += frac;
        }
        rate >> s
    }

    /// Poll CON6 LOCK_STATUS, bounded like `rockchip_rk3588_pll_wait_lock`.
    fn wait_lock(&self) -> ClkResult {
        let mut waited = 0u64;
        while waited < PLL_LOCK_TIMEOUT_US {
            if self.con(PLLCON_CON6) & PLLCON6_LOCK_STATUS != 0 {
                return Ok(());
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(PLL_LOCK_POLL_US));
            waited += PLL_LOCK_POLL_US;
        }
        log::error!("clk-rk3588: pll {} lock timeout", self.spec.pll_name);
        Err(ClkError::Io)
    }

    /// Program the rate table parameters — sequence ①-⑨ in the portrait
    /// (clk-pll.c:930-983). `pll_rk3588_core` never touches the mode mux
    /// (skips ①⑨): consumers of the core PLLs cannot tolerate a slow gap.
    fn set_params(&self, rate: &PllRate) -> ClkResult {
        // Snapshot the CURRENT parameters first: the lock-timeout restore path
        // (⑧) must re-program the old values, not the failed ones
        // (clk-pll.c:936-941).
        let (cur_m, cur_p, cur_s, cur_k) = self.get_params();
        let mut remuxed = false;
        if self.spec.kind == PllKind::Normal && self.mode_parent() == Some(PLL_MODE_NORM) {
            // ① slow the mode mux while parameters change
            self.set_mode(PLL_MODE_SLOW);
            remuxed = true;
        }

        // ② power down
        self.set_con(
            PLLCON_CON1,
            hiword_update(PLLCON1_PWRDOWN, PLLCON1_PWRDOWN, 0),
        );
        // ③④⑤ m / p+s / k
        self.set_con(
            PLLCON_CON0,
            hiword_update(rate.m & PLLCON0_M_MASK, PLLCON0_M_MASK, 0),
        );
        self.set_con(
            PLLCON_CON1,
            hiword_update(rate.p & PLLCON1_P_MASK, PLLCON1_P_MASK, 0)
                | hiword_update(rate.s & PLLCON1_S_MASK, PLLCON1_S_MASK, PLLCON1_S_SHIFT),
        );
        self.set_con(
            PLLCON_CON2,
            hiword_update(rate.k & PLLCON2_K_MASK, PLLCON2_K_MASK, 0),
        );
        // ⑥ power up
        self.set_con(PLLCON_CON1, hiword_update(0, PLLCON1_PWRDOWN, 0));
        // ⑦ wait for lock
        let ret = self.wait_lock();
        if ret.is_err() {
            // ⑧ best-effort restore of the previous parameters (Linux ignores
            // the nested failure too, clk-pll.c:977-980); the original lock
            // timeout is returned below.
            log::warn!(
                "clk-rk3588: pll {} update unsuccessful, restoring old params",
                self.spec.pll_name
            );
            let cur = PllRate {
                rate: 0,
                m: cur_m,
                p: cur_p,
                s: cur_s,
                k: cur_k,
            };
            let _ = self.set_params(&cur);
        }

        // ⑨ back to the normal parent
        if self.spec.kind == PllKind::Normal && remuxed {
            self.set_mode(PLL_MODE_NORM);
        }
        ret
    }

    /// Read the mode mux's current hardware selector.
    fn mode_parent(&self) -> Option<u8> {
        let v = self.reg.reg_read(self.spec.mode_offset);
        Some(((v >> self.spec.mode_shift) & 0x3) as u8)
    }

    fn set_mode(&self, sel: u8) {
        let v = hiword_update(sel as u32, 0x3, self.spec.mode_shift);
        self.reg.reg_write(self.spec.mode_offset, v);
    }

    /// Closest table entry for `rate` (Linux `rockchip_get_pll_settings`).
    fn table_entry(&self, rate: Rate) -> Option<&'static PllRate> {
        data::PLL_RATES.iter().min_by_key(|r| r.rate.abs_diff(rate))
    }
}

impl ClkHwOps for Rk3588Pll {
    /// Recalculate from the parameter registers (prate = xin24m).
    fn recalc_rate(&self, parent_rate: Rate) -> ClkResult<Rate> {
        let (m, p, s, k) = self.get_params();
        Ok(self.calc_rate(parent_rate, m, p, s, k))
    }

    fn round_rate(&self, rate: Rate, _parent_rate: Rate) -> ClkResult<RoundResult> {
        // Only the rate table is reachable (Linux determine_rate on table
        // PLLs). No parent change: the 24 MHz reference is fixed.
        match self.table_entry(rate) {
            Some(entry) => Ok(RoundResult {
                rate: entry.rate,
                parent_target: None,
            }),
            None => Err(ClkError::InvalidRate),
        }
    }

    fn set_rate(&self, rate: Rate, _parent_rate: Rate) -> ClkResult<Rate> {
        // Get the required settings from the table (clk-pll.c:993-1005).
        let entry = self
            .table_entry(rate)
            .filter(|e| e.rate == rate)
            .ok_or(ClkError::InvalidRate)?;
        self.set_params(entry)?;
        Ok(entry.rate)
    }

    /// `prepare` may sleep: powering up + waiting for lock is the slow part
    /// (rk3588_pll_enable, clk-pll.c:1008-1015).
    fn prepare(&self) -> ClkResult {
        self.set_con(PLLCON_CON1, hiword_update(0, PLLCON1_PWRDOWN, 0));
        self.wait_lock()
    }

    fn unprepare(&self) -> ClkResult {
        // rk3588_pll_disable (clk-pll.c:1018-1023).
        self.set_con(
            PLLCON_CON1,
            hiword_update(PLLCON1_PWRDOWN, PLLCON1_PWRDOWN, 0),
        );
        Ok(())
    }

    fn is_prepared(&self) -> ClkResult<bool> {
        Ok(self.con(PLLCON_CON1) & PLLCON1_PWRDOWN == 0)
    }

    fn enable(&self) -> ClkResult {
        // The gate work happens in prepare; the atomic fast path is a no-op.
        Ok(())
    }

    fn is_enabled(&self) -> ClkResult<bool> {
        Ok(self.con(PLLCON_CON1) & PLLCON1_PWRDOWN == 0)
    }
}

// ---------------------------------------------------------------------------
// armclk / cpuclk (clk-cpu.c)
// ---------------------------------------------------------------------------

/// The three RK3588 CPU clocks (armclk_l, armclk_b01, armclk_b23).
///
/// Like Linux, each armclk is a single-parent clock on its main PLL with
/// `SET_RATE_PARENT`; the CPU source mux is *not* a framework mux — it is
/// driven by the safe-switch dance in the parent-rate notifier
/// (`rockchip_cpuclk_pre/post_rate_change`). The alternate parent (xin24m) is
/// prepared+enabled for the controller's lifetime.
pub(crate) struct ArmClk {
    name: &'static str,
    data: &'static CpuRegData,
    rates: &'static [data::CpuRate],
    reg: Arc<dyn ClkRegAccess>,
    /// Rate-table settings for the in-flight change (notifier state).
    pending: SpinNoIrq<Cell<Option<&'static data::CpuRate>>>,
}

impl ArmClk {
    pub(crate) const NAMES: [&'static str; 3] = ["armclk_l", "armclk_b01", "armclk_b23"];

    pub(crate) fn build_all(
        reg: Arc<dyn ClkRegAccess>,
        branches: &mut Vec<ClkBranch>,
        internal_index_base: ClkIndex,
    ) -> Vec<Arc<Self>> {
        let datas: [&'static CpuRegData; 3] =
            [&data::CPU_L_DATA, &data::CPU_B01_DATA, &data::CPU_B23_DATA];
        let rates: [&'static [data::CpuRate]; 3] =
            [&data::CPUL_RATES, &data::CPUB01_RATES, &data::CPUB23_RATES];
        let indexes: [ClkIndex; 3] = [
            data::ARMCLK_L_INDEX,
            data::ARMCLK_B01_INDEX,
            data::ARMCLK_B23_INDEX,
        ];
        let mut out = Vec::with_capacity(3);
        for (i, name) in Self::NAMES.into_iter().enumerate() {
            let _ = indexes;
            let clk = Arc::new(Self {
                name,
                data: datas[i],
                rates: rates[i],
                reg: reg.clone(),
                pending: SpinNoIrq::new(Cell::new(None)),
            });
            // Single parent = the main PLL (init.parent_names[mux_core_main],
            // clk-cpu.c:321-322); SET_RATE_PARENT, never reparented by CCF,
            // rate always re-read (CLK_GET_RATE_NOCACHE, clk-cpu.c:327).
            let parent: &'static [&'static str] = Box::leak(Box::new([clk.data.main_parent]));
            branches.push(
                ClkBranch::custom(
                    internal_index_base + i as ClkIndex,
                    name,
                    clk.clone(),
                    parent,
                    ClkFlags::SET_RATE_PARENT,
                )
                .with_flags(ClkFlags::NO_REPARENT | ClkFlags::GET_RATE_NOCACHE),
            );
            out.push(clk);
        }
        out
    }

    /// After registration: enable the alt parent and hook the safe-switch
    /// notifier on the main parent (clk-cpu.c:40-62).
    pub(crate) fn post_register(clks: &[Arc<Self>]) {
        for clk in clks {
            // The alt parent (xin24m) must be alive while the dance runs.
            if let Ok(alt) = kclk::clk_get_by_name(clk.data.alt_parent)
                && let Err(err) = alt.prepare_enable()
            {
                log::error!(
                    "clk-rk3588: {} alt parent {}: {err:?}",
                    clk.name,
                    clk.data.alt_parent
                );
            }
            if let Ok(parent) = kclk::clk_get_by_name(clk.data.main_parent) {
                let strong = clk.clone();
                parent.register_notifier(Arc::new(move |event| strong.on_parent_event(event)));
            } else {
                log::error!(
                    "clk-rk3588: {} main parent {} missing",
                    clk.name,
                    clk.data.main_parent
                );
            }
        }
    }

    /// Nearest table settings at or under `rate`
    /// (`rockchip_get_cpuclk_settings` rounds down).
    fn settings_for(&self, rate: Rate) -> Option<&'static data::CpuRate> {
        self.rates
            .iter()
            .filter(|r| r.prate <= rate)
            .max_by_key(|r| r.prate)
    }

    fn write_sels(&self, sels: &[data::ClkSel]) {
        for sel in sels {
            self.reg.reg_write(sel.reg, sel.val);
        }
    }

    /// Set the CPU source mux to `sel` (alt or main) — clk-cpu.c:196-204 /
    /// 238-246: mux_core_reg when present, else core_reg[0].
    fn set_mux(&self, sel: u32) {
        let reg = self.data.mux_reg.unwrap_or(self.data.core[0].reg);
        let v = hiword_update(sel, self.data.mux_mask, self.data.mux_shift);
        self.reg.reg_write(reg, v);
    }

    /// Clear every core divider (rockchip_cpuclk_set_dividers writes 0 for
    /// RK3588; the fine dividers live in the post_muxs vals).
    fn clear_dividers(&self) {
        for core in self.data.core.iter() {
            self.reg
                .reg_write(core.reg, hiword_update(0, core.mask, core.shift));
        }
    }

    /// PRE/POST handler — clk-cpu.c:152-266.
    fn on_parent_event(&self, event: &ClkEvent) -> NotifierResult {
        match *event {
            ClkEvent::PreRateChange { old, new } => {
                let Some(rate) = self.settings_for(new) else {
                    log::error!("clk-rk3588: invalid rate {new} for {}", self.name);
                    return NotifierResult::Stop;
                };
                self.pending.lock().set(Some(rate));
                // If the alt parent is faster than the old rate, keep the CPU
                // under old_rate while the switch happens: preload alt dividers
                // (alt_div = DIV_ROUND_UP(alt, old) - 1, clk-cpu.c:177-186).
                let alt_prate = kclk::clk_get_by_name(self.data.alt_parent)
                    .map(|c| c.get_rate())
                    .unwrap_or(0);
                if alt_prate > old {
                    let alt_div = alt_prate.div_ceil(old) - 1;
                    for core in self.data.core.iter() {
                        let d = alt_div.min(core.mask as u64);
                        self.reg
                            .reg_write(core.reg, hiword_update(d as u32, core.mask, core.shift));
                    }
                }
                self.write_sels(rate.pre);
                self.set_mux(self.data.mux_alt);
                NotifierResult::Proceed
            }
            ClkEvent::PostRateChange { old, new } => {
                let Some(rate) = self.pending.lock().take() else {
                    return NotifierResult::Proceed;
                };
                // Rising: ramp up through the post muxs before dropping the
                // alt dividers (clk-cpu.c:249-262).
                if old < new {
                    self.write_sels(rate.post);
                }
                self.set_mux(self.data.mux_main);
                self.write_sels(rate.post);
                self.clear_dividers();
                if old > new {
                    self.write_sels(rate.post);
                }
                NotifierResult::Proceed
            }
            ClkEvent::RateChangeAborted { .. } => {
                // Restore the main source; the rate never changed.
                if let Some(rate) = self.pending.lock().take() {
                    self.write_sels(rate.post);
                    self.set_mux(self.data.mux_main);
                }
                NotifierResult::Proceed
            }
        }
    }
}

impl ClkHwOps for ArmClk {
    /// `parent_rate / (div_core + 1)` (clk-cpu.c:60-70).
    fn recalc_rate(&self, parent_rate: Rate) -> ClkResult<Rate> {
        let core = &self.data.core[0];
        let v = self.reg.reg_read(core.reg);
        let div = ((v >> core.shift) & core.mask) + 1;
        Ok(parent_rate / div as u64)
    }
}

// ---------------------------------------------------------------------------
// fractional divider branch (clk.c:201-290 + clk-fractional-divider.c)
// ---------------------------------------------------------------------------

/// Shared state for one frac branch's remux dance: the child mux's hardware
/// ops (called directly — never through the framework) plus the saved mux
/// position for the in-flight change.
struct FracDance {
    mux_ops: Arc<dyn ClkHwOps>,
    frac_parent_index: u8,
    saved_parent: SpinNoIrq<Cell<Option<u8>>>,
}

/// The fractional-divider node of a frac branch (plus its gate).
pub(crate) struct FracBranch {
    reg: Arc<dyn ClkRegAccess>,
    div: Field,
    gate: Option<Field>,
    name: &'static str,
}

/// Dance state keyed by the child mux clock name.
type DanceEntry = (&'static str, Arc<SpinNoIrq<FracDance>>);

/// Name-keyed dance state, filled by [`FracBranch::new`] and consumed by
/// [`FracBranch::wire_notifiers`] after the whole tree is registered.
static DANCES: SpinNoIrq<Vec<DanceEntry>> = SpinNoIrq::new(Vec::new());

impl FracBranch {
    /// Build the frac ops and the exposed child mux ops for one branch.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        spec: &CustomSpec,
        reg: Arc<dyn ClkRegAccess>,
    ) -> (Arc<dyn ClkHwOps>, Arc<dyn ClkHwOps>) {
        let child = spec.child.as_ref().expect("frac spec has child");
        let child_static: &'static FracChild = Box::leak(Box::new(FracChild {
            index: child.index,
            name: child.name,
            parents: child.parents,
            mux_offset: child.mux_offset,
            mux_shift: child.mux_shift,
            mux_mask: child.mux_mask,
            frac_parent_index: child.frac_parent_index,
        }));
        let frac = Arc::new(FracBranch {
            reg,
            div: spec.div.expect("frac spec has div"),
            gate: spec.gate,
            name: spec.name,
        });
        // The child mux is a plain hardware mux over its own register field.
        let mux_ops = Mux::into_ops(
            frac.reg.clone(),
            child.mux_offset,
            child.mux_shift,
            child.mux_mask,
            None,
            MuxFlags::HIWORD_MASK,
        );
        DANCES.lock().push((
            child_static.name,
            Arc::new(SpinNoIrq::new(FracDance {
                mux_ops: mux_ops.clone(),
                frac_parent_index: child.frac_parent_index as u8,
                saved_parent: SpinNoIrq::new(Cell::new(None)),
            })),
        ));
        (frac, mux_ops)
    }

    /// Register the PRE/POST notifier on the fraction node that switches the
    /// child mux onto the fraction while its rate changes (clk.c:419-456).
    /// Called after the tree is live; ops are invoked directly (the notifier
    /// runs under the framework clock lock and must not re-enter it).
    pub fn wire_notifiers(provider_id: u32, fracs: &[(ClkIndex, &'static FracChild)]) {
        for (frac_index, child) in fracs {
            // The notifier hooks the FRACTION node (its rate is what changes
            // during the dance); `child` describes the exposed mux it drives.
            let Ok(frac) = kclk::clk_get_by_index(provider_id, *frac_index) else {
                log::error!("clk-rk3588: frac {} not found for notifier", child.name);
                continue;
            };
            let dance = DANCES
                .lock()
                .iter()
                .find(|(n, _)| *n == child.name)
                .map(|(_, d)| d.clone());
            let Some(dance) = dance else {
                continue;
            };
            frac.register_notifier(Arc::new(move |event| {
                let dance = dance.lock();
                match event {
                    ClkEvent::PreRateChange { .. } => {
                        // Remux onto the fraction for the duration of the change.
                        let cur = dance.mux_ops.get_parent().unwrap_or(0);
                        if cur != dance.frac_parent_index {
                            dance.mux_ops.set_parent(dance.frac_parent_index).ok();
                            dance.saved_parent.lock().set(Some(cur));
                        }
                        NotifierResult::Proceed
                    }
                    ClkEvent::PostRateChange { .. } | ClkEvent::RateChangeAborted { .. } => {
                        // Back to the original source (also on abort: Linux
                        // leaves the mux on the fraction there — restoring is
                        // strictly safer and the rate never changed).
                        if let Some(prev) = dance.saved_parent.lock().take() {
                            dance.mux_ops.set_parent(prev).ok();
                        }
                        NotifierResult::Proceed
                    }
                }
            }));
        }
    }

    fn write_div(&self, m: u32, n: u32) {
        // m at [31:16], n at [15:0]; the word carries exactly this pair
        // (clk_fd_set_rate's full-field RMW).
        let value = (m << 16) | (n & 0xffff);
        self.reg.reg_modify(self.div.offset, 0xffff_ffff, value);
    }

    fn get_mn(&self) -> (u32, u32) {
        let v = self.reg.reg_read(self.div.offset);
        ((v >> 16) & 0xffff, v & 0xffff)
    }

    /// Rockchip approximation (clk.c): when the request is a large fraction of
    /// the parent and does not divide it, use the *grandparent* rate — the
    /// `_src` divider upstream would otherwise quantize the fraction.
    fn approx_mn(&self, rate: Rate, parent_rate: Rate) -> (u32, u32) {
        let mut effective_parent = parent_rate;
        if rate * 20 > parent_rate
            && !parent_rate.is_multiple_of(rate)
            && let Some(gp) = self.grandparent_rate()
        {
            effective_parent = gp;
        }
        rational_best_approximation(rate, effective_parent, 0xffff, 0xffff)
    }

    /// Grandparent rate through cached core rates (leaf locks only — calling
    /// `get_rate` does not re-enter the framework's CLK_LOCK; Linux reads the
    /// same value via clk_hw_get_rate chains).
    fn grandparent_rate(&self) -> Option<Rate> {
        let core = kclk::clk_get_by_name(self.name).ok()?;
        let parent = core.get_parent()?;
        let gp = parent.get_parent()?;
        Some(gp.get_rate())
    }
}

impl ClkHwOps for FracBranch {
    /// `m or n == 0` → pass-through, else `parent * m / n` (clk_fd_recalc).
    fn recalc_rate(&self, parent_rate: Rate) -> ClkResult<Rate> {
        let (m, n) = self.get_mn();
        if m == 0 || n == 0 {
            return Ok(parent_rate);
        }
        Ok(parent_rate.saturating_mul(m as u64) / n as u64)
    }

    fn round_rate(&self, rate: Rate, parent_rate: Rate) -> ClkResult<RoundResult> {
        // clk_fd_determine_rate: no parent change, best m/n for the request.
        if rate == 0 || rate >= parent_rate {
            return Ok(RoundResult {
                rate: parent_rate,
                parent_target: None,
            });
        }
        let (m, n) = self.approx_mn(rate, parent_rate);
        Ok(RoundResult {
            rate: parent_rate.saturating_mul(m as u64) / n as u64,
            parent_target: None,
        })
    }

    fn set_rate(&self, rate: Rate, parent_rate: Rate) -> ClkResult<Rate> {
        if rate == 0 || rate >= parent_rate {
            return Ok(parent_rate);
        }
        let (m, n) = self.approx_mn(rate, parent_rate);
        self.write_div(m, n);
        Ok(parent_rate.saturating_mul(m as u64) / n as u64)
    }

    fn enable(&self) -> ClkResult {
        if let Some(gate) = &self.gate {
            // GFLAGS = HIWORD | SET_TO_DISABLE: write 0 to the bit = enable.
            self.reg
                .reg_write(gate.offset, hiword_update(0, gate.mask, gate.shift));
        }
        Ok(())
    }

    fn disable(&self) {
        if let Some(gate) = &self.gate {
            self.reg
                .reg_write(gate.offset, hiword_update(gate.mask, gate.mask, gate.shift));
        }
    }

    fn is_enabled(&self) -> ClkResult<bool> {
        let Some(gate) = &self.gate else {
            return Ok(true);
        };
        Ok((self.reg.reg_read(gate.offset) >> gate.shift) & gate.mask == 0)
    }
}

/// `rational_best_approximation` (lib/math/rational.c): best m/n ≤
/// (max_m, max_n) for given_rate/given_parent, via the continued-fraction
/// walk. Translated from the reference implementation.
fn rational_best_approximation(
    given_rate: Rate,
    given_parent: Rate,
    max_numerator: u32,
    max_denominator: u32,
) -> (u32, u32) {
    let (max_n, max_d) = (max_numerator as u64, max_denominator as u64);
    let mut rate = given_rate;
    let mut parent = given_parent;
    let (mut n_pre, mut d_pre) = (0u64, 1u64);
    let (mut n_cur, mut d_cur) = (1u64, 0u64);

    loop {
        let div = rate.checked_div(parent).unwrap_or(0);
        let next_n = div.checked_mul(n_cur).and_then(|v| v.checked_add(n_pre));
        let next_d = div.checked_mul(d_cur).and_then(|v| v.checked_add(d_pre));
        let (Some(next_n), Some(next_d)) = (next_n, next_d) else {
            break;
        };
        // Limit checks (rational.c): the next convergent must stay within the
        // requested numerator/denominator bounds.
        if next_n > max_n || next_d > max_d {
            break;
        }
        n_pre = n_cur;
        d_pre = d_cur;
        n_cur = next_n;
        d_cur = next_d;
        let rem = if parent != 0 { rate % parent } else { 0 };
        if rem == 0 {
            break;
        }
        rate = parent;
        parent = rem;
    }
    (n_cur as u32, d_cur as u32)
}

// ---------------------------------------------------------------------------
// half divider composite (clk-half-divider.c)
// ---------------------------------------------------------------------------

/// Mux + half divider + gate (COMPOSITE_HALFDIV): raw divisor d yields
/// `parent*2/(d*2+3)`.
pub(crate) struct HalfDivComposite {
    reg: Arc<dyn ClkRegAccess>,
    mux: Option<Field>,
    div: Field,
    gate: Option<Field>,
}

impl HalfDivComposite {
    pub fn new(spec: &CustomSpec, reg: Arc<dyn ClkRegAccess>) -> Arc<Self> {
        Arc::new(Self {
            reg,
            mux: spec.mux,
            div: spec.div.expect("halfdiv spec has div"),
            gate: spec.gate,
        })
    }

    fn raw(&self) -> u32 {
        (self.reg.reg_read(self.div.offset) >> self.div.shift) & self.div.mask
    }

    /// `DIV_ROUND_UP_ULL(parent*2, raw*2+3)` (clk_half_divider_recalc_rate).
    fn calc(&self, parent_rate: Rate, raw: u32) -> Rate {
        let d = (raw as u64) * 2 + 3;
        (parent_rate * 2).div_ceil(d)
    }
}

impl ClkHwOps for HalfDivComposite {
    fn recalc_rate(&self, parent_rate: Rate) -> ClkResult<Rate> {
        Ok(self.calc(parent_rate, self.raw()))
    }

    fn round_rate(&self, rate: Rate, parent_rate: Rate) -> ClkResult<RoundResult> {
        // clk_half_divider_bestdiv, no SET_RATE_PARENT / ROUND_CLOSEST:
        // the smallest raw whose rate is ≤ the request (rates fall as raw
        // grows, so that is the highest achievable rate under the request);
        // when nothing fits, the minimum rate (max raw).
        let mut chosen = self.div.mask;
        for raw in 0..=self.div.mask {
            if self.calc(parent_rate, raw) <= rate {
                chosen = raw;
                break;
            }
        }
        Ok(RoundResult {
            rate: self.calc(parent_rate, chosen),
            parent_target: None,
        })
    }

    fn set_rate(&self, rate: Rate, parent_rate: Rate) -> ClkResult<Rate> {
        // value = (DIV_ROUND_UP(parent*2, rate) - 3) / 2, clamped to the field
        // (clk_half_divider_set_rate); hiword write.
        let value = (parent_rate * 2).div_ceil(rate).saturating_sub(3) / 2;
        let value = value.min(self.div.mask as u64) as u32;
        let mask = self.div.mask << (self.div.shift + 16);
        self.reg
            .reg_write(self.div.offset, mask | (value << self.div.shift));
        Ok(self.calc(parent_rate, value))
    }

    fn get_parent(&self) -> ClkResult<u8> {
        let Some(mux) = &self.mux else {
            return Ok(0);
        };
        Ok(((self.reg.reg_read(mux.offset) >> mux.shift) & mux.mask) as u8)
    }

    fn set_parent(&self, index: u8) -> ClkResult {
        let Some(mux) = &self.mux else {
            return Err(ClkError::NotSupported);
        };
        let v = hiword_update(index as u32 & mux.mask, mux.mask, mux.shift);
        self.reg.reg_write(mux.offset, v);
        Ok(())
    }

    fn enable(&self) -> ClkResult {
        if let Some(gate) = &self.gate {
            self.reg
                .reg_write(gate.offset, hiword_update(0, gate.mask, gate.shift));
        }
        Ok(())
    }

    fn disable(&self) {
        if let Some(gate) = &self.gate {
            self.reg
                .reg_write(gate.offset, hiword_update(gate.mask, gate.mask, gate.shift));
        }
    }

    fn is_enabled(&self) -> ClkResult<bool> {
        let Some(gate) = &self.gate else {
            return Ok(true);
        };
        Ok((self.reg.reg_read(gate.offset) >> gate.shift) & gate.mask == 0)
    }
}
