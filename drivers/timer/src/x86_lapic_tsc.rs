// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicU8, AtomicU64, Ordering, fence};

use int_ratio::Ratio;
use khal::mem::{VirtAddr, v2p};
use klazy::Once;
use log::{info, warn};
use raw_cpuid::CpuId;
use x86_64::{instructions::port::Port, registers::model_specific::Msr};

use crate::TimerSource;

const LAPIC_TICKS_PER_SEC: u64 = 1_000_000_000;
const PIT_FREQUENCY_HZ: u64 = 1_193_182;
const PIT_CALIBRATION_ROUND_NANOS: u64 = 50_000_000;
const PIT_CALIBRATION_ROUNDS: u64 = 10;
const PIT_CALIBRATION_MAX_POLLS: usize = 10_000_000;
const NANOS_PER_SECOND: u128 = 1_000_000_000;
const KVM_SIGNATURE_LEAF: u32 = 0x4000_0000;
const KVM_FEATURES_LEAF: u32 = 0x4000_0001;
const KVM_FEATURE_CLOCKSOURCE: u32 = 0;
const KVM_FEATURE_CLOCKSOURCE2: u32 = 3;
const KVM_SYSTEM_TIME_MSR: u32 = 0x12;
const KVM_SYSTEM_TIME_NEW_MSR: u32 = 0x4b56_4d01;
const KVM_MSR_ENABLED: u64 = 1;
const PVCLOCK_READ_RETRIES: usize = 1_000;
static INIT_TICK: AtomicU64 = AtomicU64::new(0);
static CPU_FREQ_KHZ: AtomicU64 = AtomicU64::new(0);
static CPU_FREQ_SOURCE: AtomicU8 = AtomicU8::new(TscFrequencySource::Uninitialized as u8);
static TSC_TICKS_TO_NANOS_RATIO: Once<Ratio> = Once::new();
static NANOS_TO_TSC_TICKS_RATIO: Once<Ratio> = Once::new();
static NANOS_TO_LAPIC_TICKS_RATIO: Once<Ratio> =
    Once::initialized(Ratio::new(LAPIC_TICKS_PER_SEC as u32, 1_000_000_000u32));
static mut KVM_PVCLOCK: AlignedPvclockVcpuTimeInfo =
    AlignedPvclockVcpuTimeInfo(PvclockVcpuTimeInfo::ZERO);

struct X86LapicTscMonotonicTimer;

#[kplat::impl_dev_interface]
impl khal::time::MonotonicTimerIf for X86LapicTscMonotonicTimer {
    fn now_ticks() -> u64 {
        now_ticks()
    }

    fn t2ns(ticks: u64) -> u64 {
        t2ns(ticks)
    }

    fn freq() -> u64 {
        freq()
    }

    fn ns2t(nanos: u64) -> u64 {
        ns2t(nanos)
    }

    fn interrupt_id() -> usize {
        interrupt_id()
    }

    fn arm_timer(deadline_ns: u64) {
        arm_timer(deadline_ns)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerConfig {
    pub nominal_frequency_hz: u64,
    pub source: TimerSource,
}

impl TimerConfig {
    pub const fn platform_static(nominal_frequency_hz: u64) -> Self {
        Self {
            nominal_frequency_hz,
            source: TimerSource::PlatformStatic,
        }
    }
}

pub fn early_init(config: TimerConfig) {
    assert!(
        config.nominal_frequency_hz >= 1_000_000,
        "x86 LAPIC/TSC timer frequency must be at least 1MHz"
    );
    let (freq_khz, source) = resolve_tsc_frequency_khz(config);
    assert!(
        u32::try_from(freq_khz).is_ok(),
        "TSC frequency must fit in u32 kHz"
    );
    CPU_FREQ_KHZ.store(freq_khz, Ordering::Relaxed);
    CPU_FREQ_SOURCE.store(source.as_u8(), Ordering::Relaxed);
    let freq_khz_u32 = freq_khz as u32;
    TSC_TICKS_TO_NANOS_RATIO.call_once(|| Ratio::new(1_000_000u32, freq_khz_u32));
    NANOS_TO_TSC_TICKS_RATIO.call_once(|| Ratio::new(freq_khz_u32, 1_000_000u32));
    info!(
        "TSC frequency: {} kHz ({})",
        CPU_FREQ_KHZ.load(Ordering::Relaxed),
        source.as_str()
    );
    if source == TscFrequencySource::PlatformStatic {
        warn!(
            "TSC frequency fallback to platform static value: {} kHz",
            CPU_FREQ_KHZ.load(Ordering::Relaxed)
        );
    }
    INIT_TICK.store(read_tsc(), Ordering::Relaxed);
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TscFrequencySource {
    Uninitialized = 0,
    HypervisorInfo,
    KvmPvClock,
    TscInfo,
    PitCalibration,
    PlatformStatic,
}

impl TscFrequencySource {
    fn as_u8(self) -> u8 {
        self as u8
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::HypervisorInfo,
            2 => Self::KvmPvClock,
            3 => Self::TscInfo,
            4 => Self::PitCalibration,
            5 => Self::PlatformStatic,
            _ => Self::Uninitialized,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Uninitialized => "uninitialized",
            Self::HypervisorInfo => "hypervisor cpuid",
            Self::KvmPvClock => "kvm pvclock",
            Self::TscInfo => "cpuid 0x15",
            Self::PitCalibration => "pit calibration",
            Self::PlatformStatic => "platform static",
        }
    }
}

fn resolve_tsc_frequency_khz(config: TimerConfig) -> (u64, TscFrequencySource) {
    if let Some(freq_khz) = tsc_frequency_from_hypervisor_khz() {
        return (freq_khz, TscFrequencySource::HypervisorInfo);
    }

    if let Some(freq_khz) = tsc_frequency_from_kvm_pvclock_khz() {
        return (freq_khz, TscFrequencySource::KvmPvClock);
    }

    if let Some(freq_khz) = tsc_frequency_from_cpuid_khz() {
        return (freq_khz, TscFrequencySource::TscInfo);
    }

    if let Some(freq_khz) = calibrate_tsc_frequency_with_pit() {
        return (freq_khz, TscFrequencySource::PitCalibration);
    }

    (
        config.nominal_frequency_hz / 1_000,
        TscFrequencySource::PlatformStatic,
    )
}

fn tsc_frequency_from_hypervisor_khz() -> Option<u64> {
    CpuId::new()
        .get_hypervisor_info()
        .and_then(|info| info.tsc_frequency())
        .map(u64::from)
        .filter(|freq_khz| *freq_khz > 0)
}

fn tsc_frequency_from_kvm_pvclock_khz() -> Option<u64> {
    let system_time_msr = kvm_system_time_msr()?;
    // SAFETY: this only takes the raw address of the static pvclock buffer.
    // The buffer is initialized and registered below during single-CPU early
    // timer initialization.
    let info_ptr = unsafe { core::ptr::addr_of_mut!(KVM_PVCLOCK.0) };

    // SAFETY: `info_ptr` points to the static pvclock buffer owned by this
    // module. During early timer initialization no other CPU can access it yet.
    unsafe {
        info_ptr.write_volatile(PvclockVcpuTimeInfo::ZERO);
    }
    let info_paddr = v2p(VirtAddr::from_mut_ptr(info_ptr)).as_usize() as u64;

    // SAFETY: KVM reports the selected system-time MSR as available. The value
    // publishes the physical address of the static pvclock buffer with the
    // enable bit set, matching the KVM pvclock ABI used by Linux.
    unsafe {
        let mut msr = Msr::new(system_time_msr);
        msr.write(info_paddr | KVM_MSR_ENABLED);
    }
    fence(Ordering::SeqCst);

    let freq_khz = read_stable_pvclock_tsc_khz(info_ptr);
    if freq_khz.is_none() {
        // SAFETY: `info_ptr` points to the static pvclock buffer registered
        // above. The volatile reads report the last values supplied by KVM.
        let version = unsafe { core::ptr::addr_of!((*info_ptr).version).read_volatile() };
        // SAFETY: same as above.
        let tsc_to_system_mul =
            unsafe { core::ptr::addr_of!((*info_ptr).tsc_to_system_mul).read_volatile() };
        // SAFETY: same as above.
        let tsc_shift = unsafe { core::ptr::addr_of!((*info_ptr).tsc_shift).read_volatile() };
        warn!(
            "KVM pvclock frequency unavailable: msr={:#x} paddr={:#x} version={} mul={} shift={}",
            system_time_msr, info_paddr, version, tsc_to_system_mul, tsc_shift
        );
    }
    freq_khz
}

fn kvm_system_time_msr() -> Option<u32> {
    let signature = cpuid(KVM_SIGNATURE_LEAF);
    if signature.ebx != 0x4b4d_564b || signature.ecx != 0x564b_4d56 || signature.edx != 0x0000_004d
    {
        if signature.eax >= KVM_SIGNATURE_LEAF
            || signature.ebx != 0
            || signature.ecx != 0
            || signature.edx != 0
        {
            warn!(
                "KVM pvclock unavailable: hypervisor max_leaf={:#x} signature={:#x}:{:#x}:{:#x}",
                signature.eax, signature.ebx, signature.ecx, signature.edx
            );
        }
        return None;
    }

    let features = cpuid(KVM_FEATURES_LEAF).eax;
    if has_bit(features, KVM_FEATURE_CLOCKSOURCE2) {
        Some(KVM_SYSTEM_TIME_NEW_MSR)
    } else if has_bit(features, KVM_FEATURE_CLOCKSOURCE) {
        Some(KVM_SYSTEM_TIME_MSR)
    } else {
        warn!(
            "KVM pvclock unavailable: features={:#x} lacks clocksource bits",
            features
        );
        None
    }
}

fn read_stable_pvclock_tsc_khz(info: *const PvclockVcpuTimeInfo) -> Option<u64> {
    for _ in 0..PVCLOCK_READ_RETRIES {
        // SAFETY: `info` points to the static pvclock buffer registered with
        // KVM. Volatile reads are used because the hypervisor updates this
        // memory asynchronously outside Rust's aliasing model.
        let version = unsafe { core::ptr::addr_of!((*info).version).read_volatile() };
        if version == 0 || version % 2 != 0 {
            core::hint::spin_loop();
            continue;
        }
        fence(Ordering::SeqCst);

        // SAFETY: same as above; fields are read between the pvclock version
        // checks required by the ABI.
        let tsc_to_system_mul =
            unsafe { core::ptr::addr_of!((*info).tsc_to_system_mul).read_volatile() };
        // SAFETY: same as above.
        let tsc_shift = unsafe { core::ptr::addr_of!((*info).tsc_shift).read_volatile() };
        fence(Ordering::SeqCst);

        // SAFETY: same as above.
        let checked_version = unsafe { core::ptr::addr_of!((*info).version).read_volatile() };
        if version == checked_version && checked_version % 2 == 0 {
            return pvclock_tsc_khz(tsc_to_system_mul, tsc_shift);
        }
    }

    None
}

fn pvclock_tsc_khz(tsc_to_system_mul: u32, tsc_shift: i8) -> Option<u64> {
    if tsc_to_system_mul == 0 {
        return None;
    }

    let mut freq_khz = (1_000_000u64 << 32) / u64::from(tsc_to_system_mul);
    if tsc_shift < 0 {
        freq_khz = freq_khz.checked_shl((-tsc_shift) as u32)?;
    } else {
        freq_khz >>= tsc_shift as u32;
    }

    Some(freq_khz).filter(|freq_khz| *freq_khz > 0)
}

#[inline]
fn cpuid(leaf: u32) -> core::arch::x86_64::CpuidResult {
    core::arch::x86_64::__cpuid(leaf)
}

#[inline]
fn has_bit(value: u32, bit: u32) -> bool {
    value & (1 << bit) != 0
}

#[repr(align(64))]
struct AlignedPvclockVcpuTimeInfo(PvclockVcpuTimeInfo);

#[repr(C)]
#[derive(Clone, Copy)]
struct PvclockVcpuTimeInfo {
    version: u32,
    pad0: u32,
    tsc_timestamp: u64,
    system_time: u64,
    tsc_to_system_mul: u32,
    tsc_shift: i8,
    flags: u8,
    pad: [u8; 2],
}

impl PvclockVcpuTimeInfo {
    const ZERO: Self = Self {
        version: 0,
        pad0: 0,
        tsc_timestamp: 0,
        system_time: 0,
        tsc_to_system_mul: 0,
        tsc_shift: 0,
        flags: 0,
        pad: [0; 2],
    };
}

fn tsc_frequency_from_cpuid_khz() -> Option<u64> {
    CpuId::new()
        .get_tsc_info()
        .and_then(|info| info.tsc_frequency())
        .map(|freq_hz| freq_hz / 1_000)
        .filter(|freq_khz| *freq_khz > 0)
}

fn calibrate_tsc_frequency_with_pit() -> Option<u64> {
    let mut pit = PitChannel2::new();
    let target_pit_delta = pit_ticks_for_nanos(PIT_CALIBRATION_ROUND_NANOS)?;
    let mut total_pit_delta = 0u64;
    let mut total_tsc_delta = 0u64;
    let mut min_freq_khz = u64::MAX;
    let mut max_freq_khz = 0u64;

    for _ in 0..PIT_CALIBRATION_ROUNDS {
        let (pit_delta, tsc_delta) = measure_pit_calibration_round(&mut pit, target_pit_delta)?;
        let round_freq_khz = pit_tsc_frequency_khz(tsc_delta, pit_delta)?;

        total_pit_delta = total_pit_delta.checked_add(pit_delta)?;
        total_tsc_delta = total_tsc_delta.checked_add(tsc_delta)?;
        min_freq_khz = min_freq_khz.min(round_freq_khz);
        max_freq_khz = max_freq_khz.max(round_freq_khz);
    }

    let freq_khz = pit_tsc_frequency_khz(total_tsc_delta, total_pit_delta)?;
    info!(
        "PIT calibration: rounds={}, target_ticks={}, total_ticks={}, tsc_delta={}, freq={} kHz, \
         range={}..{} kHz",
        PIT_CALIBRATION_ROUNDS,
        target_pit_delta,
        total_pit_delta,
        total_tsc_delta,
        freq_khz,
        min_freq_khz,
        max_freq_khz
    );
    Some(freq_khz)
}

fn measure_pit_calibration_round(
    pit: &mut PitChannel2,
    target_pit_delta: u64,
) -> Option<(u64, u64)> {
    let original_gate = pit.start(u16::MAX);
    let start_pit = pit.read_counter();
    let start_tsc = read_tsc();
    let mut end_pit = start_pit;

    for _ in 0..PIT_CALIBRATION_MAX_POLLS {
        end_pit = pit.read_counter();
        if pit_elapsed_ticks(start_pit, end_pit) >= target_pit_delta {
            break;
        }
        core::hint::spin_loop();
    }
    let end_tsc = read_tsc();
    pit.restore_gate(original_gate);

    let pit_delta = pit_elapsed_ticks(start_pit, end_pit);
    let tsc_delta = end_tsc.wrapping_sub(start_tsc);
    if pit_delta < target_pit_delta || tsc_delta == 0 {
        return None;
    }
    Some((pit_delta, tsc_delta))
}

fn pit_tsc_frequency_khz(tsc_delta: u64, pit_delta: u64) -> Option<u64> {
    let freq_hz = u128::from(tsc_delta)
        .checked_mul(u128::from(PIT_FREQUENCY_HZ))?
        .checked_div(u128::from(pit_delta))?;
    u64::try_from(freq_hz / 1_000).ok().filter(|freq| *freq > 0)
}

fn pit_ticks_for_nanos(nanos: u64) -> Option<u64> {
    let ticks = u128::from(nanos)
        .checked_mul(u128::from(PIT_FREQUENCY_HZ))?
        .checked_div(NANOS_PER_SECOND)?;
    u64::try_from(ticks).ok().filter(|ticks| *ticks > 0)
}

fn pit_elapsed_ticks(start: u16, current: u16) -> u64 {
    u64::from(start.wrapping_sub(current))
}

struct PitChannel2 {
    control: Port<u8>,
    counter: Port<u8>,
    gate: Port<u8>,
}

impl PitChannel2 {
    fn new() -> Self {
        Self {
            control: Port::new(0x43),
            counter: Port::new(0x42),
            gate: Port::new(0x61),
        }
    }

    fn start(&mut self, count: u16) -> u8 {
        // SAFETY: Ports 0x43, 0x42, and 0x61 are the legacy PIT control,
        // channel 2 data, and channel 2 gate ports on the x86 PC platform.
        unsafe {
            let gate = self.gate.read();
            self.gate.write((gate & !0x02) | 0x01);
            self.control.write(0xB0);
            self.counter.write(count as u8);
            self.counter.write((count >> 8) as u8);
            gate
        }
    }

    fn restore_gate(&mut self, gate: u8) {
        // SAFETY: Port 0x61 is the legacy PIT channel 2 gate port on the x86
        // PC platform, and `gate` was captured from the same port before
        // calibration.
        unsafe {
            self.gate.write(gate);
        }
    }

    fn read_counter(&mut self) -> u16 {
        // SAFETY: Ports 0x43 and 0x42 are the legacy PIT control and channel 2
        // data ports on the x86 PC platform. The latch command snapshots the
        // counter before the low and high bytes are read.
        unsafe {
            self.control.write(0x80);
            let low = self.counter.read();
            let high = self.counter.read();
            u16::from(low) | (u16::from(high) << 8)
        }
    }
}

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

#[inline]
pub fn now_ticks() -> u64 {
    read_tsc() - INIT_TICK.load(Ordering::Relaxed)
}

#[inline]
fn read_tsc() -> u64 {
    // SAFETY: `_rdtsc` is available on the x86_64 target and provides the
    // current monotonically increasing TSC value used by this timer source.
    unsafe { core::arch::x86_64::_rdtsc() }
}

#[inline]
pub fn t2ns(ticks: u64) -> u64 {
    TSC_TICKS_TO_NANOS_RATIO
        .get()
        .expect("x86 LAPIC/TSC timer conversion ratio is not initialized")
        .mul_trunc(ticks)
}

#[inline]
pub fn ns2t(nanos: u64) -> u64 {
    NANOS_TO_TSC_TICKS_RATIO
        .get()
        .expect("x86 LAPIC/TSC timer conversion ratio is not initialized")
        .mul_trunc(nanos)
}

#[inline]
pub fn freq() -> u64 {
    cpu_freq_khz()
        .checked_mul(1_000)
        .expect("TSC frequency overflow")
}

#[inline]
fn cpu_freq_khz() -> u64 {
    let freq_khz = CPU_FREQ_KHZ.load(Ordering::Relaxed);
    assert!(freq_khz != 0, "x86 LAPIC/TSC timer is not initialized");
    freq_khz
}

/// Returns the TSC frequency selected during early timer initialization.
#[inline]
pub fn frequency_khz() -> u64 {
    cpu_freq_khz()
}

/// Returns the source used to select the TSC frequency.
#[inline]
pub fn frequency_source() -> &'static str {
    TscFrequencySource::from_u8(CPU_FREQ_SOURCE.load(Ordering::Relaxed)).as_str()
}

#[inline]
pub fn interrupt_id() -> usize {
    x86_apic::APIC_TIMER_VECTOR as usize
}

pub fn arm_timer(deadline_ns: u64) {
    let now_ns = t2ns(now_ticks());
    // SAFETY: the local APIC timer is initialized before deadline programming, and
    // the helper holds the LAPIC lock while updating the timer registers.
    unsafe {
        x86_apic::with_local_apic(|lapic| {
            if now_ns < deadline_ns {
                let apic_ticks = NANOS_TO_LAPIC_TICKS_RATIO
                    .get()
                    .expect("x86 LAPIC deadline ratio is not initialized")
                    .mul_trunc(deadline_ns - now_ns);
                assert!(apic_ticks <= u32::MAX as u64);
                lapic.set_timer_initial(apic_ticks.max(1) as u32);
            } else {
                lapic.set_timer_initial(1);
            }
        });
    }
}
