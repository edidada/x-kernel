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
fn platform_window_is_consulted_only_for_missing_window() {
    let mut missing_coherency = facts();
    missing_coherency.coherent = None;
    let mut invalid_window = facts();
    invalid_window.range.as_mut().unwrap().size = 0;
    for (dma, mask) in [
        (Ok(facts()), u64::MAX),
        (Err(DmaResolveError::UnsupportedPath), u64::MAX),
        (Err(DmaResolveError::InvalidCoherency), u64::MAX),
        (Ok(missing_coherency), u64::MAX),
        (Ok(invalid_window), u64::MAX),
        (Ok(facts()), 0x17ff),
    ] {
        let device = device(dma);
        let expected = from_firmware(&device, mask);
        let mut called = false;
        let actual = with_platform_window(&device, mask, || {
            called = true;
            DmaWindow::new(0, 0, 0x10000).map_err(|_| DmaConfigurationError::InvalidWindow)
        });
        assert_eq!(actual, expected);
        assert_eq!(called, false);
    }
}

#[def_test]
fn platform_window_never_invents_coherency_or_expands_device_mask() {
    for coherent in [None, Some(false), Some(true)] {
        let device = device(Ok(DmaFacts {
            range: None,
            coherent,
        }));
        let result = with_platform_window(&device, 0x7fff, || {
            DmaWindow::new(0x4000, 0x4000, 0x8000).map_err(|_| DmaConfigurationError::InvalidWindow)
        });
        match coherent {
            None => assert_eq!(result, Err(DmaConfigurationError::MissingCoherency)),
            Some(coherent) => {
                let info = result.unwrap();
                assert_eq!(info.coherent, coherent);
                assert_eq!(info.mask, 0x7fff);
                assert_eq!(info.translate(0x4000), Some(0x4000));
                assert_eq!(info.translate(0x8000), None);
            }
        }
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

#[def_test]
#[cfg(target_arch = "aarch64")]
fn qemu_mmio_policy_bounds_identity_to_discovered_ram() {
    let mut count = 0;
    let mut is_valid = true;
    khal::firmware::devices::for_each_compatible_view(
        |compatible| (compatible == "virtio,mmio").then_some("deliberate,alias"),
        |view| {
            count += 1;
            let Some((start, size)) = view.single_root_ram_range() else {
                is_valid = false;
                return;
            };
            let Ok(info) = from_firmware_with_qemu_virt_mmio_policy(&view, u64::MAX) else {
                is_valid = false;
                return;
            };
            is_valid &= view.is_root_child_of("linux,dummy-virt")
                && info.coherent
                && info.mask == u64::MAX
                && info.translate_range(start as u64, size as u64) == Some(start as u64)
                && info.translate((start + size) as u64).is_none()
                && (start == 0 || info.translate((start - 1) as u64).is_none())
                && from_firmware_with_qemu_virt_mmio_policy(&view, 0x17ff)
                    == Err(DmaConfigurationError::InvalidMask);
        },
    );
    assert_eq!(count > 0, true);
    assert_eq!(is_valid, true);
}

#[def_test]
#[cfg(target_arch = "aarch64")]
fn qemu_mmio_policy_preserves_explicit_facts_and_rejects_alias_spoofing() {
    let mut count = 0;
    let mut is_valid = true;
    khal::firmware::devices::for_each_compatible_view(
        |_| Some("virtio,mmio"),
        |view| {
            count += 1;
            let strict = from_firmware(view.description(), u64::MAX);
            let policy = from_firmware_with_qemu_virt_mmio_policy(&view, u64::MAX);
            if strict != Err(DmaConfigurationError::MissingWindow) {
                is_valid &= strict == policy;
            } else if !view.is_compatible("virtio,mmio") {
                is_valid &= policy == Err(DmaConfigurationError::UnsupportedPlatformPolicy);
            }
        },
    );
    assert_eq!(count > 0, true);
    assert_eq!(is_valid, true);
}
