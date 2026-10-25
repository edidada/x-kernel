// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use khal::firmware::devices::{DmaFacts, FirmwareSource, ResolvedDmaRange};
use unittest::{assert_eq, def_test};

use super::*;

fn device(dma: Result<DmaFacts, DmaResolveError>) -> FirmwareDevice {
    FirmwareDevice {
        firmware_id: "test,dma",
        source: FirmwareSource::DeviceTree,
        mmio: None,
        irq: None,
        dma,
    }
}

fn facts() -> DmaFacts {
    DmaFacts {
        range: Some(ResolvedDmaRange {
            cpu_addr: 0x8000,
            bus_addr: 0x1000,
            size: 0x1000,
        }),
        coherent: Some(false),
    }
}

#[def_test]
fn firmware_dma_keeps_window_mask_and_coherency_independent() {
    let first = from_firmware(&device(Ok(facts())), 0x17ff).unwrap_err();
    assert_eq!(first, DmaConfigurationError::InvalidMask);
    let first = from_firmware(&device(Ok(facts())), 0x1fff).unwrap();
    assert_eq!(first.coherent, false);
    assert_eq!(first.translate_range(0x8000, 0x1000), Some(0x1000));
    assert_eq!(first.translate_range(0x8000, 0x1001), None);
    let mut second_facts = facts();
    second_facts.range.as_mut().unwrap().bus_addr = 0x3000;
    let second = from_firmware(&device(Ok(second_facts)), 0x3fff).unwrap();
    assert_eq!(second.translate_range(0x8000, 0x1000), Some(0x3000));
    assert_eq!(first.translate(0x8000), Some(0x1000));
    let masked = from_firmware(&device(Ok(second_facts)), 0x1fff).unwrap();
    assert_eq!(masked.translate(0x8000), None);
}

#[def_test]
fn firmware_dma_missing_and_rejected_facts_never_become_defaults() {
    let mut missing = facts();
    missing.range = None;
    assert_eq!(
        from_firmware(&device(Ok(missing)), u64::MAX),
        Err(DmaConfigurationError::MissingWindow)
    );
    missing = facts();
    missing.coherent = None;
    assert_eq!(
        from_firmware(&device(Ok(missing)), u64::MAX),
        Err(DmaConfigurationError::MissingCoherency)
    );
    for error in [
        DmaResolveError::UnsupportedPath,
        DmaResolveError::MultipleWindows,
        DmaResolveError::InvalidCoherency,
    ] {
        assert_eq!(
            from_firmware(&device(Err(error)), u64::MAX),
            Err(DmaConfigurationError::Firmware(error))
        );
    }
}

#[def_test]
fn firmware_dma_revalidates_public_range_values() {
    for range in [
        ResolvedDmaRange {
            cpu_addr: 0,
            bus_addr: 0,
            size: 0,
        },
        ResolvedDmaRange {
            cpu_addr: u64::MAX,
            bus_addr: 0,
            size: 2,
        },
        ResolvedDmaRange {
            cpu_addr: 0,
            bus_addr: u64::MAX,
            size: 2,
        },
    ] {
        let mut invalid = facts();
        invalid.range = Some(range);
        assert_eq!(
            from_firmware(&device(Ok(invalid)), u64::MAX),
            Err(DmaConfigurationError::InvalidWindow)
        );
    }
}

#[def_test]
#[cfg(target_arch = "aarch64")]
fn firmware_discovery_delivers_dma_facts() {
    let mut count = 0;
    let mut is_consistent = true;
    khal::firmware::devices::for_each_compatible(
        |_| Some("test,matched"),
        |device| {
            count += 1;
            is_consistent &= match device.dma {
                Err(error) => {
                    from_firmware(&device, u64::MAX) == Err(DmaConfigurationError::Firmware(error))
                }
                Ok(facts) if facts.range.is_none() => {
                    from_firmware(&device, u64::MAX) == Err(DmaConfigurationError::MissingWindow)
                }
                _ => true,
            };
        },
    );
    assert_eq!(count > 0, true);
    assert_eq!(is_consistent, true);
}
