// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Lightweight AIMD-style adaptive poll budgets.

use ktime_types::TimeSpan;

use super::PollBudget;

/// Round wall-time limit used for overrun detection and growth decisions.
pub(crate) const ROUND_TIME_CAP: TimeSpan = TimeSpan::from_millis(1);

/// Number of completed work items between monotonic time checks.
pub(crate) const TIME_CHECK_INTERVAL: usize = 16;

const GROW_WINDOW_ROUNDS: u8 = 8;
const GROW_EXHAUSTED_ROUNDS: u8 = 5;
const COOLDOWN_ROUNDS: u8 = 8;

const RX_BUDGET_MIN: usize = 64;
const RX_BUDGET_INIT: usize = 256;
const RX_BUDGET_MAX: usize = 1024;

const TX_BUDGET_MIN: usize = 32;
const TX_BUDGET_INIT: usize = 128;
const TX_BUDGET_MAX: usize = 512;

/// Timer budget stays fixed at the minimum: each round runs at most one
/// `Interface::poll`, so expanding the timer slot has no effect.
const TIMER_BUDGET: usize = 8;

/// Selects fixed compile-time budgets or the adaptive controller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BudgetMode {
    Fixed,
    Adaptive,
}

/// Which budget class hit the round time limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BudgetKind {
    Rx,
    Tx,
    Timer,
}

/// Round feedback consumed by [`AdaptiveBudgetController::feed`].
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PollRoundReport {
    pub rx_exhausted: bool,
    pub tx_exhausted: bool,
    pub timer_exhausted: bool,
    pub elapsed: TimeSpan,
    pub overrun: Option<BudgetKind>,
    /// `true` when the round observed no completed work and no immediate
    /// backlog. Idle rounds keep current budgets and do not advance growth
    /// windows.
    pub is_idle: bool,
}

struct ClassBudget {
    current: usize,
    min: usize,
    max: usize,
    window_rounds: u8,
    window_exhausted: u8,
    window_max_elapsed_us: u64,
    cooldown: u8,
}

impl ClassBudget {
    const fn new(min: usize, init: usize, max: usize) -> Self {
        Self {
            current: init,
            min,
            max,
            window_rounds: 0,
            window_exhausted: 0,
            window_max_elapsed_us: 0,
            cooldown: 0,
        }
    }

    fn shrink_half(&mut self) -> bool {
        let next = (self.current >> 1).max(self.min);
        let changed = next != self.current;
        self.current = next;
        self.cooldown = COOLDOWN_ROUNDS;
        // A shrink resets the growth window so a later grow decision does not
        // reuse pre-shrink samples.
        self.window_rounds = 0;
        self.window_exhausted = 0;
        self.window_max_elapsed_us = 0;
        changed
    }

    fn observe(&mut self, exhausted: bool, elapsed_us: u64) -> Option<isize> {
        // Cooldown is sampled before the decrement so a shrink is followed by
        // a full COOLDOWN_ROUNDS of growth suppression.
        let is_cooling = self.cooldown > 0;
        if is_cooling {
            self.cooldown -= 1;
        }

        self.window_rounds = self.window_rounds.saturating_add(1);
        if exhausted {
            self.window_exhausted = self.window_exhausted.saturating_add(1);
        }
        if elapsed_us > self.window_max_elapsed_us {
            self.window_max_elapsed_us = elapsed_us;
        }

        if self.window_rounds < GROW_WINDOW_ROUNDS {
            return None;
        }

        let can_grow = !is_cooling && self.window_exhausted >= GROW_EXHAUSTED_ROUNDS;
        let max_elapsed = self.window_max_elapsed_us;
        self.window_rounds = 0;
        self.window_exhausted = 0;
        self.window_max_elapsed_us = 0;

        if !can_grow {
            return None;
        }

        let half = round_time_cap_us() >> 1;
        let three_quarters = round_time_cap_us() - (round_time_cap_us() >> 2);
        let previous = self.current;
        if max_elapsed < half {
            self.current = (self.current << 1).min(self.max);
        } else if max_elapsed < three_quarters {
            self.current = self.current.saturating_add(self.min).min(self.max);
        }
        // 75%..100%: hold.
        if self.current > previous {
            Some((self.current - previous) as isize)
        } else if self.current < previous {
            Some(-((previous - self.current) as isize))
        } else {
            None
        }
    }
}

/// Per-class adaptive budgets with TCP-inspired AIMD growth and cooldown.
pub(crate) struct AdaptiveBudgetController {
    rx: ClassBudget,
    tx: ClassBudget,
    timer: ClassBudget,
}

impl AdaptiveBudgetController {
    pub(crate) const fn new() -> Self {
        Self {
            rx: ClassBudget::new(RX_BUDGET_MIN, RX_BUDGET_INIT, RX_BUDGET_MAX),
            tx: ClassBudget::new(TX_BUDGET_MIN, TX_BUDGET_INIT, TX_BUDGET_MAX),
            timer: ClassBudget::new(TIMER_BUDGET, TIMER_BUDGET, TIMER_BUDGET),
        }
    }

    pub(crate) fn snapshot(&self) -> PollBudget {
        PollBudget {
            rx_packets: self.rx.current,
            tx_packets: self.tx.current,
            timer_events: self.timer.current,
        }
    }

    /// Applies one round of feedback. Returns which classes grew (+) or
    /// shrank (-) so callers can update counters without holding other locks.
    pub(crate) fn feed(&mut self, report: &PollRoundReport) -> FeedDelta {
        let mut delta = FeedDelta::default();
        if report.is_idle {
            return delta;
        }

        let elapsed_us = elapsed_us(report.elapsed);
        let mut skip_rx = false;
        let mut skip_tx = false;
        let mut skip_timer = false;

        if let Some(kind) = report.overrun {
            match kind {
                BudgetKind::Rx => {
                    if self.rx.shrink_half() {
                        delta.rx_shrinks = 1;
                    }
                    // The shrink round arms cooldown and must not also count as
                    // a cooling/growth sample for the same class.
                    skip_rx = true;
                }
                BudgetKind::Tx => {
                    if self.tx.shrink_half() {
                        delta.tx_shrinks = 1;
                    }
                    skip_tx = true;
                }
                BudgetKind::Timer => {
                    // Timer budget is fixed (min == init == max). Skipping
                    // shrink avoids arming a no-op cooldown while keeping the
                    // BudgetKind::Timer overrun counter path in stats.
                    skip_timer = true;
                }
            }
        }

        if !skip_rx
            && self
                .rx
                .observe(report.rx_exhausted, elapsed_us)
                .is_some_and(|d| d > 0)
        {
            delta.rx_grows = 1;
        }
        if !skip_tx
            && self
                .tx
                .observe(report.tx_exhausted, elapsed_us)
                .is_some_and(|d| d > 0)
        {
            delta.tx_grows = 1;
        }
        if !skip_timer
            && self
                .timer
                .observe(report.timer_exhausted, elapsed_us)
                .is_some_and(|d| d > 0)
        {
            delta.timer_grows = 1;
        }

        delta
    }

    #[cfg(unittest)]
    pub(crate) fn current_rx(&self) -> usize {
        self.rx.current
    }

    #[cfg(unittest)]
    pub(crate) fn current_tx(&self) -> usize {
        self.tx.current
    }

    #[cfg(unittest)]
    pub(crate) fn current_timer(&self) -> usize {
        self.timer.current
    }

    #[cfg(unittest)]
    pub(crate) fn rx_cooldown(&self) -> u8 {
        self.rx.cooldown
    }
}

/// Budget change counts produced by one [`AdaptiveBudgetController::feed`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct FeedDelta {
    pub rx_grows: u64,
    pub tx_grows: u64,
    pub timer_grows: u64,
    pub rx_shrinks: u64,
    pub tx_shrinks: u64,
    pub timer_shrinks: u64,
}

const fn round_time_cap_us() -> u64 {
    // ROUND_TIME_CAP is 1 ms; keep the microsecond form as a const for shifts.
    1_000
}

pub(crate) fn elapsed_us(elapsed: TimeSpan) -> u64 {
    let micros = elapsed.as_micros();
    if micros > u64::MAX as u128 {
        u64::MAX
    } else {
        micros as u64
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, def_test};

    use super::*;

    fn busy_report(exhausted_rx: bool, elapsed: TimeSpan) -> PollRoundReport {
        PollRoundReport {
            rx_exhausted: exhausted_rx,
            tx_exhausted: false,
            timer_exhausted: false,
            elapsed,
            overrun: None,
            is_idle: false,
        }
    }

    #[def_test]
    fn idle_round_keeps_budget() {
        let mut ctrl = AdaptiveBudgetController::new();
        let before = ctrl.snapshot();
        ctrl.feed(&PollRoundReport {
            is_idle: true,
            ..PollRoundReport::default()
        });
        assert!(ctrl.snapshot() == before);
    }

    #[def_test]
    fn needs_five_exhausted_rounds_to_grow() {
        let mut ctrl = AdaptiveBudgetController::new();
        let init = ctrl.current_rx();
        let fast = TimeSpan::from_micros(100);

        for _ in 0..4 {
            ctrl.feed(&busy_report(true, fast));
        }
        // Fourth of eight: still below the grow gate even with exhaustion.
        for _ in 0..4 {
            ctrl.feed(&busy_report(false, fast));
        }
        assert!(ctrl.current_rx() == init);

        for _ in 0..5 {
            ctrl.feed(&busy_report(true, fast));
        }
        for _ in 0..3 {
            ctrl.feed(&busy_report(false, fast));
        }
        assert!(ctrl.current_rx() == (init << 1).min(RX_BUDGET_MAX));
    }

    #[def_test]
    fn grows_by_min_unit_in_mid_band() {
        let mut ctrl = AdaptiveBudgetController::new();
        let init = ctrl.current_rx();
        let init_tx = ctrl.current_tx();
        let mid = TimeSpan::from_micros(600);

        for _ in 0..5 {
            ctrl.feed(&busy_report(true, mid));
        }
        for _ in 0..3 {
            ctrl.feed(&busy_report(false, mid));
        }
        assert!(ctrl.current_rx() == (init + RX_BUDGET_MIN).min(RX_BUDGET_MAX));
        // TX did not exhaust, so it stays at the initial budget.
        assert!(ctrl.current_tx() == init_tx);
    }

    #[def_test]
    fn holds_budget_in_high_band() {
        let mut ctrl = AdaptiveBudgetController::new();
        let init = ctrl.current_rx();
        let high = TimeSpan::from_micros(800);

        for _ in 0..5 {
            ctrl.feed(&busy_report(true, high));
        }
        for _ in 0..3 {
            ctrl.feed(&busy_report(false, high));
        }
        assert!(ctrl.current_rx() == init);
    }

    #[def_test]
    fn overrun_halves_and_starts_cooldown() {
        let mut ctrl = AdaptiveBudgetController::new();
        let init = ctrl.current_rx();
        ctrl.feed(&PollRoundReport {
            elapsed: ROUND_TIME_CAP,
            overrun: Some(BudgetKind::Rx),
            is_idle: false,
            ..PollRoundReport::default()
        });
        assert!(ctrl.current_rx() == (init >> 1).max(RX_BUDGET_MIN));
        assert!(ctrl.rx_cooldown() == COOLDOWN_ROUNDS);
    }

    #[def_test]
    fn cooldown_blocks_growth_but_allows_shrink() {
        let mut ctrl = AdaptiveBudgetController::new();
        // Force a shrink into cooldown.
        ctrl.feed(&PollRoundReport {
            elapsed: ROUND_TIME_CAP,
            overrun: Some(BudgetKind::Rx),
            is_idle: false,
            ..PollRoundReport::default()
        });
        let after_shrink = ctrl.current_rx();
        let fast = TimeSpan::from_micros(100);
        for _ in 0..5 {
            ctrl.feed(&busy_report(true, fast));
        }
        for _ in 0..3 {
            ctrl.feed(&busy_report(false, fast));
        }
        assert!(ctrl.current_rx() == after_shrink);

        // Another overrun may shrink further while cooling down.
        ctrl.feed(&PollRoundReport {
            elapsed: ROUND_TIME_CAP,
            overrun: Some(BudgetKind::Rx),
            is_idle: false,
            ..PollRoundReport::default()
        });
        assert!(ctrl.current_rx() == (after_shrink >> 1).max(RX_BUDGET_MIN));
    }

    #[def_test]
    fn clamps_to_min_and_max() {
        let mut ctrl = AdaptiveBudgetController::new();
        // Shrink repeatedly to the floor.
        for _ in 0..16 {
            ctrl.feed(&PollRoundReport {
                elapsed: ROUND_TIME_CAP,
                overrun: Some(BudgetKind::Rx),
                is_idle: false,
                ..PollRoundReport::default()
            });
        }
        assert!(ctrl.current_rx() == RX_BUDGET_MIN);

        // Drain cooldown with idle-looking non-exhausted rounds that still
        // advance observe() — use non-idle empty exhaustion reports.
        let fast = TimeSpan::from_micros(100);
        for _ in 0..COOLDOWN_ROUNDS {
            ctrl.feed(&busy_report(false, fast));
        }
        // Grow repeatedly to the ceiling.
        for _ in 0..64 {
            for _ in 0..5 {
                ctrl.feed(&busy_report(true, fast));
            }
            for _ in 0..3 {
                ctrl.feed(&busy_report(false, fast));
            }
        }
        assert!(ctrl.current_rx() == RX_BUDGET_MAX);
    }

    #[def_test]
    fn timer_budget_stays_at_minimum() {
        let mut ctrl = AdaptiveBudgetController::new();
        assert!(ctrl.current_timer() == TIMER_BUDGET);
        let fast = TimeSpan::from_micros(100);
        for _ in 0..5 {
            ctrl.feed(&PollRoundReport {
                timer_exhausted: true,
                elapsed: fast,
                is_idle: false,
                ..PollRoundReport::default()
            });
        }
        for _ in 0..3 {
            ctrl.feed(&busy_report(false, fast));
        }
        assert!(ctrl.current_timer() == TIMER_BUDGET);

        ctrl.feed(&PollRoundReport {
            elapsed: ROUND_TIME_CAP,
            overrun: Some(BudgetKind::Timer),
            is_idle: false,
            ..PollRoundReport::default()
        });
        assert!(ctrl.current_timer() == TIMER_BUDGET);
        // Fixed timer budget must not arm cooldown on overrun.
        assert!(ctrl.rx_cooldown() == 0);
    }
}
