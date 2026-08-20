// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicU64, Ordering};

use int_ratio::Ratio;
use klazy::Once;
use log::{info, warn};
use raw_cpuid::CpuId;
use x86_64::instructions::port::Port;

use crate::TimerSource;

const LAPIC_TICKS_PER_SEC: u64 = 1_000_000_000;
const MIN_TSC_FREQUENCY_HZ: u64 = 1_000_000;
const MAX_TSC_FREQUENCY_HZ: u64 = 100_000_000_000;
const PIT_FREQUENCY_HZ: u64 = 1_193_182;
const PIT_CALIBRATION_TICKS: u16 = 59_659;
const PIT_CALIBRATION_ROUNDS: usize = 3;
static INIT_TICK: AtomicU64 = AtomicU64::new(0);
static TSC_FREQUENCY_HZ: AtomicU64 = AtomicU64::new(0);
static TSC_TICKS_TO_NANOS_RATIO: Once<Ratio> = Once::new();
static NANOS_TO_TSC_TICKS_RATIO: Once<Ratio> = Once::new();
static NANOS_TO_LAPIC_TICKS_RATIO: Once<Ratio> = Once::initialized(Ratio::new(
    LAPIC_TICKS_PER_SEC as u32,
    ktime_types::NANOS_PER_SEC as u32,
));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TscFrequencySource {
    CpuidLeaf15,
    PitChannel2,
    Platform(TimerSource),
}

#[kplat::impl_dev_interface]
impl khal::time::MonotonicTimerIf {
    fn now_ticks() -> khal::time::TimerTicks {
        now_ticks()
    }

    fn ticks_to_span(ticks: khal::time::TimerTicks) -> ktime_types::TimeSpan {
        ktime_types::TimeSpan::from_nanos(ticks_to_nanos(ticks.as_raw()))
    }

    fn freq() -> u64 {
        freq()
    }

    fn span_to_ticks(span: ktime_types::TimeSpan) -> khal::time::TimerTicks {
        khal::time::TimerTicks::from_raw(nanos_to_ticks(span.as_nanos_u64_saturating()))
    }

    fn interrupt_id() -> usize {
        interrupt_id()
    }

    fn arm_timer(deadline: ktime_types::MonotonicInstant) {
        arm_timer(deadline)
    }

    fn disarm_timer() {
        disarm_timer()
    }

    fn handle_idle_return(_previous_ticks: khal::time::TimerTicks) -> bool {
        false
    }
}

/// Configuration used when architectural TSC calibration is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerConfig {
    /// Platform estimate used as a bounded-calibration timeout and fallback.
    pub nominal_frequency_hz: u64,
    /// Provenance of the platform estimate.
    pub source: TimerSource,
}

impl TimerConfig {
    /// Creates a configuration backed by a static platform frequency.
    pub const fn platform_static(nominal_frequency_hz: u64) -> Self {
        Self {
            nominal_frequency_hz,
            source: TimerSource::PlatformStatic,
        }
    }
}

/// Calibrates the TSC and establishes the monotonic timer conversion ratios.
///
/// # Panics
///
/// Panics when the platform fallback falls outside the supported frequency
/// range or the selected frequency cannot be represented by the fixed-point
/// conversion ratios.
pub fn early_init(config: TimerConfig) {
    assert!(
        is_valid_tsc_frequency(config.nominal_frequency_hz),
        "x86 LAPIC/TSC timer frequency must be between 1MHz and 100GHz"
    );
    let (frequency_hz, source) = detect_tsc_frequency(config);
    let frequency_khz = frequency_hz / 1_000;
    assert!(
        u32::try_from(frequency_khz).is_ok(),
        "TSC frequency must fit in u32 kHz"
    );
    TSC_FREQUENCY_HZ.store(frequency_hz, Ordering::Relaxed);
    let frequency_khz = frequency_khz as u32;
    TSC_TICKS_TO_NANOS_RATIO
        .call_once(|| Ratio::new(ktime_types::NANOS_PER_MILLIS as u32, frequency_khz));
    NANOS_TO_TSC_TICKS_RATIO
        .call_once(|| Ratio::new(frequency_khz, ktime_types::NANOS_PER_MILLIS as u32));
    info!("TSC frequency: {} Hz ({:?})", frequency_hz, source);
    // SAFETY: `_rdtsc` is available on the x86_64 target and is used here during
    // early timer initialization to capture the monotonic TSC baseline.
    INIT_TICK.store(unsafe { core::arch::x86_64::_rdtsc() }, Ordering::Relaxed);
}

fn detect_tsc_frequency(config: TimerConfig) -> (u64, TscFrequencySource) {
    let cpuid = CpuId::new();
    let has_invariant_tsc = cpuid
        .get_advanced_power_mgmt_info()
        .is_some_and(|info| info.has_invariant_tsc());
    if !has_invariant_tsc {
        // TCG qemu64 and similar guests omit invariant TSC. Calibrating that
        // variable counter against CPUID 0x15 or PIT still installs a
        // fixed-ratio clock, and the measured rate is often far from the
        // historical platform fallback. Userspace CLOCK_MONOTONIC waits then
        // expire early. Stay on the platform estimate instead of treating a
        // non-constant TSC as a calibrated monotonic source.
        warn!(
            "CPU does not report invariant TSC; using platform frequency {} Hz ({:?})",
            config.nominal_frequency_hz, config.source
        );
        return select_tsc_frequency(false, None, None, config);
    }

    let leaf15_hz = cpuid
        .get_tsc_info()
        .and_then(|info| info.tsc_frequency())
        .filter(|frequency_hz| is_valid_tsc_frequency(*frequency_hz));
    let pit_hz = calibrate_tsc_with_pit(config.nominal_frequency_hz);
    select_tsc_frequency(true, leaf15_hz, pit_hz, config)
}

fn select_tsc_frequency(
    has_invariant_tsc: bool,
    leaf15_hz: Option<u64>,
    pit_hz: Option<u64>,
    config: TimerConfig,
) -> (u64, TscFrequencySource) {
    if !has_invariant_tsc {
        return (
            config.nominal_frequency_hz,
            TscFrequencySource::Platform(config.source),
        );
    }

    match (leaf15_hz, pit_hz) {
        (Some(cpuid_hz), Some(measured_hz)) if tsc_frequencies_agree(cpuid_hz, measured_hz) => {
            (cpuid_hz, TscFrequencySource::CpuidLeaf15)
        }
        (Some(cpuid_hz), Some(measured_hz)) => {
            warn!(
                "CPUID 0x15 TSC frequency {} Hz disagrees with PIT {} Hz; using PIT",
                cpuid_hz, measured_hz
            );
            (measured_hz, TscFrequencySource::PitChannel2)
        }
        (Some(cpuid_hz), None) => (cpuid_hz, TscFrequencySource::CpuidLeaf15),
        (None, Some(measured_hz)) => (measured_hz, TscFrequencySource::PitChannel2),
        (None, None) => {
            warn!(
                "TSC calibration unavailable; using platform frequency {} Hz ({:?})",
                config.nominal_frequency_hz, config.source
            );
            (
                config.nominal_frequency_hz,
                TscFrequencySource::Platform(config.source),
            )
        }
    }
}

fn tsc_frequencies_agree(left_hz: u64, right_hz: u64) -> bool {
    let larger_hz = u128::from(left_hz.max(right_hz));
    let smaller_hz = u128::from(left_hz.min(right_hz));
    if larger_hz == 0 {
        return false;
    }
    // 10% of the larger value. PIT jitter is typically well below this; a
    // lying or scaled CPUID 0x15 leaf is typically well above it.
    10 * (larger_hz - smaller_hz) <= larger_hz
}

fn calibrate_tsc_with_pit(nominal_frequency_hz: u64) -> Option<u64> {
    calibrate_tsc(|| measure_tsc_with_pit(nominal_frequency_hz))
}

fn calibrate_tsc(mut measure_delta: impl FnMut() -> Option<u64>) -> Option<u64> {
    let mut minimum_frequency_hz = None;
    for _ in 0..PIT_CALIBRATION_ROUNDS {
        let Some(delta_tsc) = measure_delta() else {
            continue;
        };
        let Some(frequency_hz) = tsc_frequency_from_pit_delta(delta_tsc) else {
            continue;
        };
        if !is_valid_tsc_frequency(frequency_hz) {
            warn!("PIT produced an invalid TSC frequency: {} Hz", frequency_hz);
            continue;
        }
        minimum_frequency_hz = Some(
            minimum_frequency_hz.map_or(frequency_hz, |minimum: u64| minimum.min(frequency_hz)),
        );
    }
    minimum_frequency_hz
}

fn measure_tsc_with_pit(nominal_frequency_hz: u64) -> Option<u64> {
    let timeout_ticks = nominal_frequency_hz.clamp(1_000_000_000, 100_000_000_000) / 2;

    // SAFETY: ports 0x42, 0x43, and 0x61 are the legacy PIT channel 2 data,
    // PIT command, and speaker/gate control ports on the x86 PC platform.
    // `early_init` runs on the bootstrap CPU before other code can use PIT
    // channel 2. The original gate/speaker value is restored before return.
    let (delta_tsc, is_completed) = unsafe {
        let mut control = Port::<u8>::new(0x61);
        let saved_control = control.read();
        control.write(saved_control & !0x03);

        let mut command = Port::<u8>::new(0x43);
        let mut data = Port::<u8>::new(0x42);
        command.write(0xb0);
        data.write(PIT_CALIBRATION_TICKS as u8);
        data.write((PIT_CALIBRATION_TICKS >> 8) as u8);

        let tsc_start = core::arch::x86_64::_rdtsc();
        control.write((saved_control & !0x02) | 0x01);
        let mut is_completed = false;
        loop {
            if control.read() & 0x20 != 0 {
                is_completed = true;
                break;
            }
            if core::arch::x86_64::_rdtsc().wrapping_sub(tsc_start) > timeout_ticks {
                break;
            }
            core::hint::spin_loop();
        }
        let tsc_end = core::arch::x86_64::_rdtsc();
        control.write(saved_control);
        (tsc_end.wrapping_sub(tsc_start), is_completed)
    };

    if !is_completed {
        warn!("PIT TSC calibration timed out");
        return None;
    }
    (delta_tsc != 0).then_some(delta_tsc)
}

fn tsc_frequency_from_pit_delta(delta_tsc: u64) -> Option<u64> {
    if delta_tsc == 0 {
        return None;
    }
    let frequency_hz =
        u128::from(delta_tsc) * u128::from(PIT_FREQUENCY_HZ) / u128::from(PIT_CALIBRATION_TICKS);
    u64::try_from(frequency_hz).ok()
}

const fn is_valid_tsc_frequency(frequency_hz: u64) -> bool {
    frequency_hz >= MIN_TSC_FREQUENCY_HZ && frequency_hz <= MAX_TSC_FREQUENCY_HZ
}

/// Programs the bootstrap CPU's local APIC timer in one-shot mode.
pub fn init_primary() {
    // SAFETY: the bootstrap CPU has already initialized the local APIC, and the
    // helper holds its lock while programming the LAPIC timer registers.
    unsafe {
        use x2apic::lapic::{TimerDivide, TimerMode};
        x86_apic::with_local_apic(|lapic| {
            lapic.set_timer_mode(TimerMode::OneShot);
            lapic.set_timer_divide(TimerDivide::Div1);
            lapic.enable_timer();
        });
    }
}

/// Programs the current secondary CPU's local APIC timer in one-shot mode.
pub fn init_secondary() {
    // SAFETY: secondary CPUs call this only after LAPIC bring-up; the helper
    // programs the current CPU's private LAPIC timer state to match the BSP's
    // one-shot / divide-by-1 configuration.
    unsafe {
        use x2apic::lapic::{TimerDivide, TimerMode};
        x86_apic::with_local_apic(|lapic| {
            lapic.set_timer_mode(TimerMode::OneShot);
            lapic.set_timer_divide(TimerDivide::Div1);
            lapic.enable_timer();
        })
    }
}

/// Reads the current monotonic TSC tick count relative to early initialization.
#[inline]
pub fn now_ticks() -> khal::time::TimerTicks {
    khal::time::TimerTicks::from_raw(read_tsc_ticks_raw())
}

#[inline]
fn read_tsc_ticks_raw() -> u64 {
    // SAFETY: `_rdtsc` is available on the x86_64 target and provides the current
    // monotonically increasing TSC value used by this timer source.
    unsafe { core::arch::x86_64::_rdtsc() - INIT_TICK.load(Ordering::Relaxed) }
}

#[inline]
pub(crate) fn ticks_to_nanos(ticks: u64) -> u64 {
    TSC_TICKS_TO_NANOS_RATIO
        .get()
        .expect("x86 LAPIC/TSC timer conversion ratio is not initialized")
        .mul_trunc(ticks)
}

#[inline]
pub(crate) fn nanos_to_ticks(nanos: u64) -> u64 {
    NANOS_TO_TSC_TICKS_RATIO
        .get()
        .expect("x86 LAPIC/TSC timer conversion ratio is not initialized")
        .mul_trunc(nanos)
}

/// Returns the calibrated TSC frequency in hertz.
#[inline]
pub fn freq() -> u64 {
    tsc_frequency_hz()
}

#[inline]
fn tsc_frequency_hz() -> u64 {
    let frequency_hz = TSC_FREQUENCY_HZ.load(Ordering::Relaxed);
    assert!(frequency_hz != 0, "x86 LAPIC/TSC timer is not initialized");
    frequency_hz
}

/// Returns the IRQ vector assigned to the local APIC timer.
#[inline]
pub fn interrupt_id() -> usize {
    x86_apic::APIC_TIMER_VECTOR as usize
}

/// Arms the current CPU's one-shot local APIC timer for `deadline`.
pub fn arm_timer(deadline: ktime_types::MonotonicInstant) {
    let now_ns = ticks_to_nanos(now_ticks().as_raw());
    let deadline_ns = deadline.as_nanos_u64_saturating();
    // SAFETY: the local APIC timer is initialized before deadline programming, and
    // the helper holds the LAPIC lock while updating the timer registers.
    unsafe {
        x86_apic::with_local_apic(|lapic| {
            if now_ns < deadline_ns {
                let apic_ticks = NANOS_TO_LAPIC_TICKS_RATIO
                    .get()
                    .expect("x86 LAPIC deadline ratio is not initialized")
                    // Initial-count is 32-bit; clamp and re-arbitrate on the early IRQ.
                    .mul_trunc(deadline_ns - now_ns)
                    .clamp(1, u32::MAX as u64);
                lapic.set_timer_initial(apic_ticks as u32);
            } else {
                lapic.set_timer_initial(1);
            }
        });
    }
}

pub fn disarm_timer() {
    // SAFETY: LAPIC bring-up completes before any deadline programming, the
    // same precondition as `arm_timer`. `with_local_apic` holds the local APIC
    // lock, so this CPU exclusively updates the timer registers. Writing 0 to
    // the initial-count register stops the current countdown (Intel SDM).
    unsafe {
        x86_apic::with_local_apic(|lapic| {
            lapic.set_timer_initial(0);
        });
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, assert_eq, def_test};

    use super::*;

    #[def_test]
    fn test_pit_delta_converts_to_tsc_frequency() {
        let frequency_hz = tsc_frequency_from_pit_delta(150_000_000).unwrap();

        assert!((2_999_000_000..=3_001_000_000).contains(&frequency_hz));
        assert_eq!(tsc_frequency_from_pit_delta(0), None);
    }

    #[def_test]
    fn test_tsc_frequency_bounds() {
        assert!(!is_valid_tsc_frequency(MIN_TSC_FREQUENCY_HZ - 1));
        assert!(is_valid_tsc_frequency(MIN_TSC_FREQUENCY_HZ));
        assert!(is_valid_tsc_frequency(MAX_TSC_FREQUENCY_HZ));
        assert!(!is_valid_tsc_frequency(MAX_TSC_FREQUENCY_HZ + 1));
    }

    #[def_test]
    fn test_pit_calibration_skips_failed_rounds() {
        let mut deltas = [None, Some(150_000_000), Some(149_000_000)].into_iter();

        let frequency_hz = calibrate_tsc(|| deltas.next().flatten()).unwrap();

        assert_eq!(
            frequency_hz,
            tsc_frequency_from_pit_delta(149_000_000).unwrap()
        );
    }

    #[def_test]
    fn test_missing_invariant_tsc_uses_platform_frequency() {
        let config = TimerConfig::platform_static(4_000_000_000);

        assert_eq!(
            select_tsc_frequency(false, Some(2_000_000_000), Some(2_000_000_000), config),
            (4_000_000_000, TscFrequencySource::Platform(config.source))
        );
    }

    #[def_test]
    fn test_cpuid_and_pit_agreement_prefers_cpuid() {
        let config = TimerConfig::platform_static(4_000_000_000);

        assert_eq!(
            select_tsc_frequency(true, Some(3_000_000_000), Some(3_100_000_000), config),
            (3_000_000_000, TscFrequencySource::CpuidLeaf15)
        );
    }

    #[def_test]
    fn test_cpuid_and_pit_disagreement_uses_pit() {
        let config = TimerConfig::platform_static(4_000_000_000);

        assert_eq!(
            select_tsc_frequency(true, Some(4_000_000_000), Some(2_000_000_000), config),
            (2_000_000_000, TscFrequencySource::PitChannel2)
        );
    }

    #[def_test]
    fn test_single_calibration_source_is_used() {
        let config = TimerConfig::platform_static(4_000_000_000);

        assert_eq!(
            select_tsc_frequency(true, Some(3_000_000_000), None, config),
            (3_000_000_000, TscFrequencySource::CpuidLeaf15)
        );
        assert_eq!(
            select_tsc_frequency(true, None, Some(2_500_000_000), config),
            (2_500_000_000, TscFrequencySource::PitChannel2)
        );
        assert_eq!(
            select_tsc_frequency(true, None, None, config),
            (4_000_000_000, TscFrequencySource::Platform(config.source))
        );
    }

    #[def_test]
    fn test_tsc_frequencies_agree_within_ten_percent() {
        assert!(tsc_frequencies_agree(4_000_000_000, 4_400_000_000));
        assert!(!tsc_frequencies_agree(4_000_000_000, 4_500_000_000));
        assert!(!tsc_frequencies_agree(0, 1_000_000));
    }
}
