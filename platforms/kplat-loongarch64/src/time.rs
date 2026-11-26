// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use khal::time::{ClockEventIf, CounterSourceIf, TimerTicks};
use ktime_types::{Frequency, TimeSpan};
use lazyinit::LazyInit;
use loongArch64::{register::tcfg, time::Time};
use timer_driver::tick_scale::TickScale;

const TIMER_IRQ: usize = 11;
const MIN_TIMER_TICKS: u64 = 4;

static TICK_SCALE: LazyInit<TickScale> = LazyInit::new();

#[inline]
fn read_timer_ticks_raw() -> u64 {
    Time::read() as _
}

#[inline]
fn ticks_to_nanos(ticks: u64) -> u64 {
    TICK_SCALE.ticks_to_nanos(ticks)
}

#[inline]
fn nanos_to_ticks(nanos: u64) -> u64 {
    TICK_SCALE.nanos_to_ticks(nanos)
}

pub(super) fn init_percpu() {
    tcfg::set_init_val(0);
    tcfg::set_periodic(false);
    tcfg::set_en(true);
    kirq::enable(TIMER_IRQ, true);
}

/// Installs the counter conversion for the stable-counter frequency.
///
/// # Panics
///
/// Panics when the reported frequency is zero or does not fit in `u32` Hz.
pub(super) fn early_init() {
    TICK_SCALE.init_once(TickScale::new(Frequency::from_hz(
        loongArch64::time::get_timer_freq() as u64,
    )));
}

#[kiface::provide]
impl CounterSourceIf {
    fn now_ticks() -> TimerTicks {
        TimerTicks::from_raw(read_timer_ticks_raw())
    }

    fn resume_ticks() -> TimerTicks {
        TimerTicks::from_raw(read_timer_ticks_raw())
    }

    fn ticks_to_span(ticks: TimerTicks) -> ktime_types::TimeSpan {
        ktime_types::TimeSpan::from_nanos(ticks_to_nanos(ticks.as_raw()))
    }

    fn span_to_ticks(span: ktime_types::TimeSpan) -> TimerTicks {
        TimerTicks::from_raw(nanos_to_ticks(span.as_nanos_u64_saturating()))
    }

    fn frequency() -> Frequency {
        Frequency::from_hz(loongArch64::time::get_timer_freq() as u64)
    }
}

#[kiface::provide]
impl ClockEventIf {
    fn interrupt_id() -> usize {
        TIMER_IRQ
    }

    fn arm_timer_after(interval: TimeSpan) {
        // Rounding up keeps a sub-tick remainder from firing before the deadline.
        let init_value = TICK_SCALE
            .nanos_to_ticks_ceil(interval.as_nanos_u64_saturating())
            .max(MIN_TIMER_TICKS)
            .saturating_add(MIN_TIMER_TICKS - 1)
            / MIN_TIMER_TICKS
            * MIN_TIMER_TICKS;
        tcfg::set_init_val(init_value as _);
        tcfg::set_en(true);
    }

    fn restore_local_timer() {
        // Restore one-shot mode without the boot path's zero-count event.
        tcfg::set_en(false);
        tcfg::set_periodic(false);
        tcfg::set_init_val(0);
        kirq::enable(TIMER_IRQ, true);
    }

    fn disarm_timer() {
        tcfg::set_en(false);
    }
}

#[kiface::provide]
impl kiface_time_api::ClockResumeIf {
    fn handle_idle_return(_previous_ticks: TimerTicks) -> bool {
        false
    }
}
