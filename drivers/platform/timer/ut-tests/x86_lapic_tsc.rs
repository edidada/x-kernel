// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use raw_cpuid::{CpuId, CpuIdReader, CpuIdResult};
use unittest::def_test;

use super::{
    Frequency, PIT_CALIBRATION_TICKS, PIT_TICK_RATE, PitObservation, PitTscCalibrator,
    TscFrequency::{Cpuid, Hypervisor, Pit, PlatformStatic},
    cpuid_tsc_frequency, discover_tsc_frequency_with, filter_tsc_frequency,
};

fn cpuid_with_frequencies(
    signature: &'static [u8; 12],
    hypervisor_mhz: u32,
    cpuid_mhz: u32,
) -> CpuId<impl CpuIdReader> {
    CpuId::with_cpuid_reader(move |leaf, _| {
        let [eax, ebx, ecx, edx] = match leaf {
            0 => [
                0x15,
                u32::from_le_bytes(*b"Genu"),
                u32::from_le_bytes(*b"ntel"),
                u32::from_le_bytes(*b"ineI"),
            ],
            1 => [0, 0, 1 << 31, 0],
            0x15 => [1, 1, cpuid_mhz * 1_000_000, 0],
            0x4000_0000 => [
                0x4000_0010,
                u32::from_le_bytes(signature[..4].try_into().unwrap()),
                u32::from_le_bytes(signature[4..8].try_into().unwrap()),
                u32::from_le_bytes(signature[8..].try_into().unwrap()),
            ],
            0x4000_0010 => [hypervisor_mhz * 1_000, 0, 0, 0],
            _ => [0; 4],
        };
        CpuIdResult { eax, ebx, ecx, edx }
    })
}

#[def_test]
fn tsc_frequency_discovery_respects_sources_and_fallback() {
    let pit = Frequency::from_hz(1_000_000_000);
    let cpuid = Frequency::from_hz(2_400_000_000);
    let hypervisor = Frequency::from_hz(2_500_000_000);
    let platform = Frequency::from_hz(4_000_000_000);
    let qemu = b"TCGTCGTCGTCG";
    let kvm = b"KVMKVMKVM\0\0\0";
    let acrn = b"ACRNACRNACRN";
    let vmware = b"VMwareVMware";
    for (signature, hypervisor_mhz, cpuid_mhz, measured, expected, should_calibrate) in [
        (qemu, 4000, 2400, Some(pit), Pit(pit), true),
        (qemu, 4000, 2400, None, Cpuid(cpuid), true),
        (kvm, 2500, 2400, None, Hypervisor(hypervisor), false),
        (acrn, 2500, 2400, None, Hypervisor(hypervisor), false),
        (vmware, 2500, 2400, None, Cpuid(cpuid), false),
        (kvm, 0, 0, Some(pit), Pit(pit), true),
        (qemu, 0, 0, None, PlatformStatic(platform), true),
        (kvm, 0, 0, None, PlatformStatic(platform), true),
    ] {
        let cpuid_reader = cpuid_with_frequencies(signature, hypervisor_mhz, cpuid_mhz);
        let mut has_calibrated = false;
        let selected =
            discover_tsc_frequency_with(&cpuid_reader, platform, |nominal, is_qemu_tcg| {
                assert_eq!(nominal, platform);
                assert_eq!(is_qemu_tcg, signature == b"TCGTCGTCGTCG");
                has_calibrated = true;
                measured
            });
        assert_eq!(selected, expected, "hypervisor: {:?}", signature);
        assert_eq!(has_calibrated, should_calibrate);
    }
}

#[def_test]
fn tsc_frequency_calculations_preserve_reference_units() {
    assert_eq!(
        filter_tsc_frequency(Some(Frequency::from_hz(999_999))),
        None
    );
    for (base_mhz, numerator, denominator, expected) in [
        (2400, 7, 3, Some(Frequency::from_hz(2_400_000_000))),
        (2400, 0, 3, None),
        (2400, 7, 0, None),
        (0, 7, 3, None),
    ] {
        let cpuid = CpuId::with_cpuid_reader(|leaf, _| {
            let [eax, ebx, ecx, edx] = match leaf {
                0 => [0x16, 0x756e6547, 0x6c65746e, 0x49656e69], // GenuineIntel
                0x15 => [denominator, numerator, 0, 0],
                0x16 => [base_mhz, 0, 0, 0],
                _ => [0; 4],
            };
            CpuIdResult { eax, ebx, ecx, edx }
        });
        assert_eq!(cpuid_tsc_frequency(&cpuid), expected);
    }
    assert_eq!(
        PitTscCalibrator::frequency_from_ticks(4_000_000_000, PIT_TICK_RATE.as_hz()),
        Some(Frequency::from_hz(4_000_000_000))
    );
}

#[def_test]
fn cpuid_rejects_out_of_range_crystal_rate_and_uses_base_frequency() {
    // Leaf 0x15 reports a usable ratio but a crystal rate whose product
    // overflows the accepted kHz range, so discovery must fall back to the
    // leaf 0x16 processor base frequency.
    let cpuid = CpuId::with_cpuid_reader(|leaf, _| {
        let [eax, ebx, ecx, edx] = match leaf {
            0 => [0x16, 0x756e6547, 0x6c65746e, 0x49656e69], // GenuineIntel
            0x15 => [1, u32::MAX, u32::MAX, 0],
            0x16 => [2400, 0, 0, 0],
            _ => [0; 4],
        };
        CpuIdResult { eax, ebx, ecx, edx }
    });
    assert_eq!(
        cpuid_tsc_frequency(&cpuid),
        Some(Frequency::from_hz(2_400_000_000))
    );
}

#[def_test]
fn pit_observations_report_the_bounded_midpoint() {
    let interval_ticks = u64::from(PIT_CALIBRATION_TICKS);
    let exact_frequency =
        PitTscCalibrator::frequency_from_ticks(25_000_000, interval_ticks)
            .expect("a plausible interval converts to a frequency");

    // Both reads agree: the estimate is the rate they imply, with no spread.
    let exact = PitTscCalibrator::estimate(Some(PitObservation {
        low_tsc_ticks: 25_000_000,
        high_tsc_ticks: 25_000_000,
    }))
    .expect("a bounded observation is usable");
    assert_eq!(exact.frequency, exact_frequency);
    assert_eq!(exact.low_frequency, exact_frequency);
    assert_eq!(exact.spread_hz, 0);

    // A delayed post-observation read raises the interval; the estimate stays
    // between the two implied rates and reports the full spread.
    let stretched = PitTscCalibrator::estimate(Some(PitObservation {
        low_tsc_ticks: 24_000_000,
        high_tsc_ticks: 26_000_000,
    }))
    .expect("a bounded observation is usable");
    let low = PitTscCalibrator::frequency_from_ticks(24_000_000, interval_ticks)
        .expect("lower bound converts");
    let high = PitTscCalibrator::frequency_from_ticks(26_000_000, interval_ticks)
        .expect("upper bound converts");
    assert!(stretched.frequency > low && stretched.frequency < high);
    assert_eq!(stretched.low_frequency, low);
    assert_eq!(stretched.spread_hz, u128::from(high.as_hz() - low.as_hz()));

    // A post-observation read below the pre-observation read is inconsistent.
    assert!(
        PitTscCalibrator::estimate(Some(PitObservation {
            low_tsc_ticks: 26_000_000,
            high_tsc_ticks: 24_000_000,
        }))
        .is_none()
    );
    assert!(PitTscCalibrator::estimate(None).is_none());
}
