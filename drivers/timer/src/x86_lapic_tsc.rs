// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86 monotonic clock and local APIC clock-event driver.
//!
//! The driver discovers the TSC frequency during bootstrap, publishes TSC-based
//! monotonic time, and programs the per-CPU LAPIC timer for one-shot deadlines.

use core::sync::atomic::{AtomicU64, Ordering};

use int_ratio::Ratio;
use klazy::Once;
use log::{info, warn};
use raw_cpuid::{CpuId, Hypervisor};
use x86_64::instructions::port::{Port, PortWriteOnly};

use crate::TimerSource;

const LAPIC_TICKS_PER_SEC: u64 = 1_000_000_000;
const PIT_TICK_RATE_HZ: u64 = 1_193_182;
const PIT_CALIBRATION_TICKS: u16 = (PIT_TICK_RATE_HZ / 100) as u16;
const PIT_CALIBRATION_SAMPLES: usize = 3;
const PIT_MIN_POLL_ITERATIONS: usize = 1_000;
const PIT_TCG_MIN_POLL_ITERATIONS: usize = 1;
const PIT_MAX_POLL_ITERATIONS: usize = 1_000_000;
const PIT_MAX_POLL_DELTA_RATIO: u64 = 10;
const MIN_TSC_FREQUENCY_HZ: u64 = 1_000_000;
static INIT_TICK: AtomicU64 = AtomicU64::new(0);
static TSC_FREQUENCY_KHZ: AtomicU64 = AtomicU64::new(0);
static TSC_TICKS_TO_NANOS_RATIO: Once<Ratio> = Once::new();
static NANOS_TO_TSC_TICKS_RATIO: Once<Ratio> = Once::new();
static NANOS_TO_LAPIC_TICKS_RATIO: Once<Ratio> = Once::initialized(Ratio::new(
    LAPIC_TICKS_PER_SEC as u32,
    ktime_types::NANOS_PER_SEC as u32,
));

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

/// Platform-provided fallback configuration for the x86 timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerConfig {
    /// Nominal TSC frequency used when runtime discovery fails.
    pub nominal_frequency_hz: u64,
    /// Origin of the nominal fallback value.
    pub source: TimerSource,
}

impl TimerConfig {
    /// Creates a configuration backed by a platform-static fallback frequency.
    pub const fn platform_static(nominal_frequency_hz: u64) -> Self {
        Self {
            nominal_frequency_hz,
            source: TimerSource::PlatformStatic,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TscFrequencySource {
    Pit,
    Hypervisor,
    Cpuid,
    PlatformStatic,
}

impl TscFrequencySource {
    const fn name(self) -> &'static str {
        match self {
            Self::Pit => "PIT calibration",
            Self::Hypervisor => "hypervisor CPUID",
            Self::Cpuid => "CPUID",
            Self::PlatformStatic => "platform static fallback",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TscFrequencyCandidates {
    is_qemu_tcg: bool,
    pit_frequency_hz: Option<u64>,
    hypervisor_frequency_hz: Option<u64>,
    cpuid_frequency_hz: Option<u64>,
    platform_frequency_hz: u64,
}

struct PitChannel2 {
    system_control: Port<u8>,
    command: PortWriteOnly<u8>,
    channel_data: PortWriteOnly<u8>,
    original_system_control: u8,
}

impl PitChannel2 {
    fn acquire() -> Self {
        let mut system_control = Port::new(0x61);
        // SAFETY: bootstrap runs at CPL0 on the BSP and owns legacy PIT channel 2.
        // Port 0x61 is the PC system-control register associated with this channel.
        let original_system_control = unsafe { system_control.read() };
        Self {
            system_control,
            command: PortWriteOnly::new(0x43),
            channel_data: PortWriteOnly::new(0x42),
            original_system_control,
        }
    }

    fn start_one_shot(&mut self, ticks: u16) {
        assert_ne!(ticks, 0, "PIT one-shot reload must be nonzero");
        let gated_system_control = (self.original_system_control & !0x02) | 0x01;
        // SAFETY: `acquire` gives this bootstrap path exclusive ownership of PIT
        // channel 2. The command selects channel 2, mode 0, binary LSB/MSB writes;
        // the following two writes provide the complete nonzero reload value.
        unsafe {
            self.system_control.write(gated_system_control);
            self.command.write(0xb0);
            self.channel_data.write(ticks as u8);
            self.channel_data.write((ticks >> 8) as u8);
        }
    }

    fn has_elapsed(&mut self) -> bool {
        // SAFETY: `acquire` established exclusive bootstrap ownership, and bit 5
        // of port 0x61 is the read-only output state of PIT channel 2.
        unsafe { self.system_control.read() & 0x20 != 0 }
    }
}

impl Drop for PitChannel2 {
    fn drop(&mut self) {
        // SAFETY: the guard still owns the same port-0x61 control state and
        // restores the speaker and gate bits captured before calibration.
        unsafe { self.system_control.write(self.original_system_control) };
    }
}

/// Initializes TSC conversion state during bootstrap on the primary CPU.
///
/// The platform must call this function exactly once from primary-CPU early
/// driver initialization, before secondary CPUs or device drivers can access
/// legacy PIT channel 2.
///
/// # Panics
///
/// Panics if the platform fallback is below 1 MHz or the selected frequency
/// cannot be represented by the timer conversion ratios.
pub fn early_init(config: TimerConfig) {
    assert!(
        config.nominal_frequency_hz >= MIN_TSC_FREQUENCY_HZ,
        "x86 LAPIC/TSC timer frequency must be at least 1MHz"
    );
    let (frequency_hz, source) = discover_tsc_frequency(config.nominal_frequency_hz);
    let frequency_khz = frequency_hz / 1_000;
    assert!(
        u32::try_from(frequency_khz).is_ok(),
        "TSC frequency must fit in u32 kHz"
    );
    TSC_FREQUENCY_KHZ.store(frequency_khz, Ordering::Relaxed);
    let frequency_khz_u32 = frequency_khz as u32;
    TSC_TICKS_TO_NANOS_RATIO
        .call_once(|| Ratio::new(ktime_types::NANOS_PER_MILLIS as u32, frequency_khz_u32));
    NANOS_TO_TSC_TICKS_RATIO
        .call_once(|| Ratio::new(frequency_khz_u32, ktime_types::NANOS_PER_MILLIS as u32));
    let frequency_khz = TSC_FREQUENCY_KHZ.load(Ordering::Relaxed);
    if source == TscFrequencySource::PlatformStatic {
        warn!(
            "TSC discovery failed; using {} kHz from {}",
            frequency_khz,
            source.name()
        );
    } else {
        info!(
            "TSC frequency: {} kHz, source: {}",
            frequency_khz,
            source.name()
        );
    }
    INIT_TICK.store(read_tsc_raw(), Ordering::Relaxed);
}

fn discover_tsc_frequency(platform_frequency_hz: u64) -> (u64, TscFrequencySource) {
    let cpuid = CpuId::new();
    let is_intel = cpuid
        .get_vendor_info()
        .is_some_and(|vendor| vendor.as_str() == "GenuineIntel");
    let hypervisor_info = cpuid.get_hypervisor_info();
    let hypervisor = hypervisor_info.as_ref().map(|info| info.identify());
    let is_qemu_tcg = matches!(hypervisor.as_ref(), Some(Hypervisor::QEMU));
    let hypervisor_frequency_hz = if hypervisor
        .as_ref()
        .is_some_and(hypervisor_has_tsc_frequency_leaf)
    {
        valid_tsc_frequency(
            hypervisor_info
                .as_ref()
                .and_then(|info| info.tsc_frequency())
                .and_then(|frequency_khz| u64::from(frequency_khz).checked_mul(1_000)),
        )
    } else {
        None
    };
    let cpuid_frequency_hz = valid_tsc_frequency(
        is_intel
            .then(|| {
                cpuid.get_tsc_info().and_then(|tsc_info| {
                    tsc_info.tsc_frequency().or_else(|| {
                        let frequency_info = cpuid.get_processor_frequency_info()?;
                        tsc_frequency_from_processor_base(
                            frequency_info.processor_base_frequency(),
                            tsc_info.numerator(),
                            tsc_info.denominator(),
                        )
                    })
                })
            })
            .flatten(),
    );
    let needs_pit_calibration =
        is_qemu_tcg || (hypervisor_frequency_hz.is_none() && cpuid_frequency_hz.is_none());
    let pit_frequency_hz = needs_pit_calibration
        .then(|| calibrate_tsc_with_pit(platform_frequency_hz, is_qemu_tcg))
        .flatten();

    select_tsc_frequency(TscFrequencyCandidates {
        is_qemu_tcg,
        pit_frequency_hz,
        hypervisor_frequency_hz,
        cpuid_frequency_hz,
        platform_frequency_hz,
    })
}

fn hypervisor_has_tsc_frequency_leaf(hypervisor: &Hypervisor) -> bool {
    // CPUID 0x40000010 is vendor-defined. QEMU publishes TSC kHz there only
    // under its KVM signature, while ACRN defines the same timing leaf.
    matches!(hypervisor, Hypervisor::KVM | Hypervisor::ACRN)
}

fn select_tsc_frequency(candidates: TscFrequencyCandidates) -> (u64, TscFrequencySource) {
    let pit = valid_tsc_frequency(candidates.pit_frequency_hz)
        .map(|frequency_hz| (frequency_hz, TscFrequencySource::Pit));
    let hypervisor = valid_tsc_frequency(candidates.hypervisor_frequency_hz)
        .map(|frequency_hz| (frequency_hz, TscFrequencySource::Hypervisor));
    let cpuid = valid_tsc_frequency(candidates.cpuid_frequency_hz)
        .map(|frequency_hz| (frequency_hz, TscFrequencySource::Cpuid));

    if candidates.is_qemu_tcg {
        pit.or(hypervisor).or(cpuid)
    } else {
        hypervisor.or(cpuid).or(pit)
    }
    .unwrap_or((
        candidates.platform_frequency_hz,
        TscFrequencySource::PlatformStatic,
    ))
}

fn valid_tsc_frequency(frequency_hz: Option<u64>) -> Option<u64> {
    frequency_hz.filter(|frequency_hz| {
        *frequency_hz >= MIN_TSC_FREQUENCY_HZ && *frequency_hz / 1_000 <= u64::from(u32::MAX)
    })
}

fn tsc_frequency_from_processor_base(
    processor_base_frequency_mhz: u16,
    numerator: u32,
    denominator: u32,
) -> Option<u64> {
    if processor_base_frequency_mhz == 0 || numerator == 0 || denominator == 0 {
        return None;
    }

    // Intel CPUID 0x16 supplies a processor base frequency, not a TSC
    // frequency. Reconstruct the crystal clock with the CPUID 0x15 ratio and
    // then derive the TSC frequency, following Linux `native_calibrate_tsc`.
    let crystal_frequency_khz =
        u128::from(processor_base_frequency_mhz) * 1_000 * u128::from(denominator)
            / u128::from(numerator);
    let tsc_frequency_khz = crystal_frequency_khz * u128::from(numerator) / u128::from(denominator);
    u64::try_from(tsc_frequency_khz * 1_000).ok()
}

fn calibrate_tsc_with_pit(nominal_frequency_hz: u64, is_qemu_tcg: bool) -> Option<u64> {
    let mut minimum_frequency_hz: Option<u64> = None;
    for _ in 0..PIT_CALIBRATION_SAMPLES {
        let sample = x86_64::instructions::interrupts::without_interrupts(|| {
            measure_tsc_with_pit(nominal_frequency_hz, is_qemu_tcg)
        });
        if let Some(frequency_hz) = valid_tsc_frequency(sample) {
            minimum_frequency_hz = Some(
                minimum_frequency_hz.map_or(frequency_hz, |current| current.min(frequency_hz)),
            );
        }
    }
    minimum_frequency_hz
}

fn measure_tsc_with_pit(nominal_frequency_hz: u64, is_qemu_tcg: bool) -> Option<u64> {
    let mut pit = PitChannel2::acquire();
    pit.start_one_shot(PIT_CALIBRATION_TICKS);
    let start_tsc = read_tsc_raw();
    let mut previous_tsc = start_tsc;
    let mut minimum_poll_delta = u64::MAX;
    let mut maximum_poll_delta = 0;
    let timeout_tsc_ticks = nominal_frequency_hz / 5;

    for nr_poll_iterations in 0..PIT_MAX_POLL_ITERATIONS {
        if pit.has_elapsed() {
            if !is_pit_sample_usable(
                is_qemu_tcg,
                nr_poll_iterations,
                minimum_poll_delta,
                maximum_poll_delta,
            ) {
                return None;
            }
            let elapsed_tsc_ticks = previous_tsc.wrapping_sub(start_tsc);
            return calculate_tsc_frequency_hz(elapsed_tsc_ticks, u64::from(PIT_CALIBRATION_TICKS));
        }

        let current_tsc = read_tsc_raw();
        let poll_delta = current_tsc.wrapping_sub(previous_tsc);
        previous_tsc = current_tsc;
        minimum_poll_delta = minimum_poll_delta.min(poll_delta);
        maximum_poll_delta = maximum_poll_delta.max(poll_delta);

        if current_tsc.wrapping_sub(start_tsc) >= timeout_tsc_ticks {
            return None;
        }
    }
    None
}

fn is_pit_sample_usable(
    is_qemu_tcg: bool,
    nr_poll_iterations: usize,
    minimum_poll_delta: u64,
    maximum_poll_delta: u64,
) -> bool {
    if is_qemu_tcg {
        // TCG exits to its device model for every PIT read. Host scheduling can
        // reduce the number of reads and make adjacent RDTSC deltas arbitrarily
        // uneven even when the full PIT interval yields a usable measurement.
        return nr_poll_iterations >= PIT_TCG_MIN_POLL_ITERATIONS && minimum_poll_delta != 0;
    }
    let has_enough_progress =
        nr_poll_iterations >= PIT_MIN_POLL_ITERATIONS && minimum_poll_delta != 0;
    has_enough_progress
        && maximum_poll_delta <= minimum_poll_delta.saturating_mul(PIT_MAX_POLL_DELTA_RATIO)
}

fn calculate_tsc_frequency_hz(elapsed_tsc_ticks: u64, elapsed_pit_ticks: u64) -> Option<u64> {
    if elapsed_tsc_ticks == 0 || elapsed_pit_ticks == 0 {
        return None;
    }
    let frequency_hz = u128::from(elapsed_tsc_ticks).checked_mul(u128::from(PIT_TICK_RATE_HZ))?
        / u128::from(elapsed_pit_ticks);
    u64::try_from(frequency_hz).ok()
}

#[inline]
fn read_tsc_raw() -> u64 {
    // SAFETY: `_rdtsc` is available on every x86_64 target supported by this
    // architecture-specific driver and does not access memory.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// Configures the bootstrap CPU's LAPIC timer in one-shot mode.
///
/// The local APIC must be initialized for the bootstrap CPU before this call.
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

/// Configures a secondary CPU's LAPIC timer in one-shot mode.
///
/// The local APIC must be initialized for the calling CPU before this call.
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

/// Returns elapsed TSC ticks since primary timer initialization.
///
/// [`early_init`] must complete before this function is used as a monotonic
/// time source.
#[inline]
pub fn now_ticks() -> khal::time::TimerTicks {
    khal::time::TimerTicks::from_raw(read_tsc_ticks_raw())
}

#[inline]
fn read_tsc_ticks_raw() -> u64 {
    read_tsc_raw().wrapping_sub(INIT_TICK.load(Ordering::Relaxed))
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

/// Returns the discovered TSC frequency in hertz.
///
/// # Panics
///
/// Panics if [`early_init`] has not completed.
#[inline]
pub fn freq() -> u64 {
    tsc_frequency_khz()
        .checked_mul(1_000)
        .expect("TSC frequency overflow")
}

#[inline]
fn tsc_frequency_khz() -> u64 {
    let frequency_khz = TSC_FREQUENCY_KHZ.load(Ordering::Relaxed);
    assert!(frequency_khz != 0, "x86 LAPIC/TSC timer is not initialized");
    frequency_khz
}

/// Returns the architectural LAPIC timer interrupt vector.
#[inline]
pub fn interrupt_id() -> usize {
    x86_apic::APIC_TIMER_VECTOR as usize
}

/// Arms the current CPU's LAPIC timer for a monotonic deadline.
///
/// [`early_init`] and local APIC initialization for the calling CPU must
/// complete before this function is called.
///
/// # Panics
///
/// Panics if the TSC conversion ratio has not been initialized.
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

/// Stops the current CPU's LAPIC timer countdown.
///
/// The local APIC must be initialized for the calling CPU before this call.
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
    use raw_cpuid::Hypervisor;
    use unittest::def_test;

    use super::{
        PIT_MIN_POLL_ITERATIONS, PIT_TCG_MIN_POLL_ITERATIONS, PIT_TICK_RATE_HZ,
        TscFrequencyCandidates, TscFrequencySource, calculate_tsc_frequency_hz,
        hypervisor_has_tsc_frequency_leaf, is_pit_sample_usable, select_tsc_frequency,
        tsc_frequency_from_processor_base,
    };

    #[def_test]
    fn qemu_tcg_prefers_measured_tsc_frequency() {
        let selected = select_tsc_frequency(TscFrequencyCandidates {
            is_qemu_tcg: true,
            pit_frequency_hz: Some(1_000_000_000),
            hypervisor_frequency_hz: Some(4_000_000_000),
            cpuid_frequency_hz: Some(4_000_000_000),
            platform_frequency_hz: 4_000_000_000,
        });

        assert_eq!(selected, (1_000_000_000, TscFrequencySource::Pit));
    }

    #[def_test]
    fn non_qemu_guest_prefers_hypervisor_tsc_frequency() {
        let selected = select_tsc_frequency(TscFrequencyCandidates {
            is_qemu_tcg: false,
            pit_frequency_hz: Some(1_000_000_000),
            hypervisor_frequency_hz: Some(2_500_000_000),
            cpuid_frequency_hz: Some(2_400_000_000),
            platform_frequency_hz: 4_000_000_000,
        });

        assert_eq!(selected, (2_500_000_000, TscFrequencySource::Hypervisor));
    }

    #[def_test]
    fn hypervisor_tsc_frequency_leaf_is_vendor_scoped() {
        assert!(hypervisor_has_tsc_frequency_leaf(&Hypervisor::KVM));
        assert!(hypervisor_has_tsc_frequency_leaf(&Hypervisor::ACRN));
        assert!(!hypervisor_has_tsc_frequency_leaf(&Hypervisor::QEMU));
        assert!(!hypervisor_has_tsc_frequency_leaf(&Hypervisor::VMware));
    }

    #[def_test]
    fn processor_base_frequency_uses_tsc_crystal_ratio() {
        assert_eq!(
            tsc_frequency_from_processor_base(2_400, 7, 3),
            Some(2_399_999_000)
        );
        assert_eq!(tsc_frequency_from_processor_base(2_400, 0, 3), None);
        assert_eq!(tsc_frequency_from_processor_base(2_400, 7, 0), None);
    }

    #[def_test]
    fn pit_sample_converts_reference_ticks_to_hertz() {
        assert_eq!(
            calculate_tsc_frequency_hz(4_000_000_000, PIT_TICK_RATE_HZ),
            Some(4_000_000_000)
        );
    }

    #[def_test]
    fn hardware_pit_sample_rejects_short_or_disturbed_polling() {
        assert!(is_pit_sample_usable(
            false,
            PIT_MIN_POLL_ITERATIONS,
            10,
            100
        ));
        assert!(!is_pit_sample_usable(
            false,
            PIT_MIN_POLL_ITERATIONS - 1,
            10,
            10
        ));
        assert!(!is_pit_sample_usable(
            false,
            PIT_MIN_POLL_ITERATIONS,
            10,
            101
        ));
        assert!(!is_pit_sample_usable(false, PIT_MIN_POLL_ITERATIONS, 0, 0));
    }

    #[def_test]
    fn qemu_tcg_pit_sample_accepts_host_scheduler_jitter() {
        assert!(is_pit_sample_usable(
            true,
            PIT_TCG_MIN_POLL_ITERATIONS,
            10,
            1_000_000
        ));
        assert!(!is_pit_sample_usable(
            true,
            PIT_TCG_MIN_POLL_ITERATIONS - 1,
            10,
            1_000_000
        ));
        assert!(!is_pit_sample_usable(
            true,
            PIT_TCG_MIN_POLL_ITERATIONS,
            0,
            1_000_000
        ));
    }

    #[def_test]
    fn invalid_runtime_frequencies_use_platform_fallback() {
        let selected = select_tsc_frequency(TscFrequencyCandidates {
            is_qemu_tcg: true,
            pit_frequency_hz: Some(0),
            hypervisor_frequency_hz: None,
            cpuid_frequency_hz: Some(999_999),
            platform_frequency_hz: 4_000_000_000,
        });

        assert_eq!(
            selected,
            (4_000_000_000, TscFrequencySource::PlatformStatic)
        );
    }
}
