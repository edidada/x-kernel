// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use klazy::Once;
use ktime_types::Frequency;
use riscv::register::time;

use crate::{TimerSource, tick_scale::TickScale};

static TIMER_IRQ: AtomicUsize = AtomicUsize::new(0);
static TIMER_FREQ_HZ: AtomicU64 = AtomicU64::new(0);
static TICK_SCALE: Once<TickScale> = Once::new();

#[kiface::provide]
impl khal::time::CounterSourceIf {
    fn now_ticks() -> khal::time::TimerTicks {
        now_ticks()
    }

    fn resume_ticks() -> khal::time::TimerTicks {
        now_ticks()
    }

    fn ticks_to_span(ticks: khal::time::TimerTicks) -> ktime_types::TimeSpan {
        ktime_types::TimeSpan::from_nanos(ticks_to_nanos(ticks.as_raw()))
    }

    fn frequency() -> Frequency {
        frequency()
    }

    fn span_to_ticks(span: ktime_types::TimeSpan) -> khal::time::TimerTicks {
        khal::time::TimerTicks::from_raw(nanos_to_ticks(span.as_nanos_u64_saturating()))
    }
}

#[kiface::provide]
impl khal::time::ClockEventIf {
    fn interrupt_id() -> usize {
        interrupt_id()
    }

    fn arm_timer_after(interval: ktime_types::TimeSpan) {
        arm_timer_after(interval)
    }

    fn restore_local_timer() {
        // SBI owns comparator access; platform PM restores hart IRQ enables.
        disarm_timer();
    }

    fn disarm_timer() {
        disarm_timer()
    }
}

#[kiface::provide]
impl kiface_time_api::ClockResumeIf {
    fn handle_idle_return(_previous_ticks: khal::time::TimerTicks) -> bool {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerConfig {
    pub irq: usize,
    pub frequency: Frequency,
    pub source: TimerSource,
}

impl TimerConfig {
    /// Builds the platform-static timer descriptor for one IRQ line and
    /// a known counter frequency.
    pub const fn platform_static(irq: usize, frequency: Frequency) -> Self {
        Self {
            irq,
            frequency,
            source: TimerSource::PlatformStatic,
        }
    }

    /// Builds the timer descriptor for one IRQ line and a counter frequency
    /// read from the device tree.
    pub const fn device_tree(irq: usize, frequency: Frequency) -> Self {
        Self {
            irq,
            frequency,
            source: TimerSource::DeviceTree,
        }
    }
}

/// Initializes the SBI timer backend: validates the IRQ and frequency
/// and installs the count-to-nanoseconds conversion.
///
/// # Panics
///
/// Panics for a zero IRQ, a zero frequency, or a frequency that does not
/// fit in `u32` Hz.
pub fn init(config: TimerConfig) {
    assert!(config.irq != 0, "RISC-V SBI timer IRQ must be non-zero");
    let scale = TickScale::new(config.frequency);
    TIMER_IRQ.store(config.irq, Ordering::Relaxed);
    TIMER_FREQ_HZ.store(config.frequency.as_hz(), Ordering::Relaxed);
    TICK_SCALE.call_once(|| scale);
}

/// Enables supervisor timer interrupts on the current hart; event
/// delivery goes through the SBI `set_timer` call on each re-arm.
pub fn init_percpu() {
    sbi_rt::set_timer(0);
}

#[inline]
fn now_ticks() -> khal::time::TimerTicks {
    khal::time::TimerTicks::from_raw(read_timer_ticks_raw())
}

#[inline]
fn read_timer_ticks_raw() -> u64 {
    time::read() as u64
}

#[inline]
fn tick_scale() -> &'static TickScale {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: SBI timer initialization installs the \
                  conversion scale before publishing clock and timer operations"
    )]
    TICK_SCALE
        .get()
        .expect("RISC-V SBI timer conversion scale is not initialized")
}

#[inline]
fn ticks_to_nanos(ticks: u64) -> u64 {
    tick_scale().ticks_to_nanos(ticks)
}

#[inline]
fn nanos_to_ticks(nanos: u64) -> u64 {
    tick_scale().nanos_to_ticks(nanos)
}

#[inline]
fn frequency() -> Frequency {
    Frequency::from_hz(TIMER_FREQ_HZ.load(Ordering::Relaxed))
}

#[inline]
fn interrupt_id() -> usize {
    let irq = TIMER_IRQ.load(Ordering::Relaxed);
    assert!(irq != 0, "RISC-V SBI timer not initialized");
    irq
}

fn arm_timer_after(interval: ktime_types::TimeSpan) {
    // SBI takes an absolute hardware deadline, never a logical clock value.
    // Rounding up keeps a sub-tick remainder from firing before the deadline.
    let ticks = tick_scale()
        .nanos_to_ticks_ceil(interval.as_nanos_u64_saturating())
        .max(1);
    sbi_rt::set_timer(read_timer_ticks_raw().saturating_add(ticks));
}

fn disarm_timer() {
    // A compare value far in the future disables practical timer IRQs until re-armed.
    sbi_rt::set_timer(u64::MAX);
}
