// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kdevice::PlatformIdentity;
use unittest::{assert_eq, def_test};

use super::*;

fn reset_model() -> alloc::sync::Arc<kdevice::BusInstance> {
    let (_bus_type, bus) = kdevice::reset_and_setup_platform_bus_for_tests("enumeration-test-bus");
    bus
}

#[def_test(serial)]
fn test_register_device_creates_descriptor_without_runtime_record() {
    let bus = reset_model();

    let mut context = EnumerationContext::new();
    let id = context
        .register_device(
            bus.id(),
            DeviceLocation::PlatformStatic { id: 1 },
            DiscoveryOrigin::PlatformStatic,
            DeviceIdentity::Platform(PlatformIdentity {
                alias: Some("test-device"),
                firmware_id: None,
            }),
            None,
            ResourceSet::new(),
        )
        .unwrap();

    let desc = kdevice::find_device_desc(id).unwrap();
    assert_eq!(desc.id(), id);
    assert_eq!(context.legacy_registered_count(), 1);
    assert_eq!(kdevice::device_records_snapshot().len(), 0);
}

#[def_test(serial)]
fn test_probe_pending_requeues_unmatched_descriptor() {
    let bus = reset_model();

    let mut context = EnumerationContext::new();
    context
        .register_device(
            bus.id(),
            DeviceLocation::PlatformStatic { id: 2 },
            DiscoveryOrigin::PlatformStatic,
            DeviceIdentity::Platform(PlatformIdentity {
                alias: Some("test-device"),
                firmware_id: None,
            }),
            None,
            ResourceSet::new(),
        )
        .unwrap();

    assert_eq!(context.probe_pending(), 0);
    assert_eq!(context.legacy_unclaimed_count(), 1);
    assert_eq!(
        context.legacy_unclaimed()[0].identity(),
        DeviceIdentity::Platform(PlatformIdentity {
            alias: Some("test-device"),
            firmware_id: None,
        })
    );
}

#[def_test(serial)]
fn mmio_rescan_reuses_descriptor_and_rejects_overlapping_aliases() {
    let bus = reset_model();
    let mut first = EnumerationContext::new();
    let id = first
        .register_device(
            bus.id(),
            DeviceLocation::Mmio {
                base: 0x1000,
                size: 0x200,
            },
            DiscoveryOrigin::DeviceTree,
            DeviceIdentity::Platform(PlatformIdentity {
                alias: None,
                firmware_id: Some("virtio,mmio"),
            }),
            None,
            ResourceSet::new(),
        )
        .unwrap();
    let mut rescan = EnumerationContext::new();
    assert_eq!(rescan.revisit_mmio(0x1000, 0x200), true);
    assert_eq!(rescan.revisit_mmio(0x1000, 0x200), true);
    assert_eq!(rescan.revisit_mmio(0x1100, 0x200), true);
    assert_eq!(rescan.revisit_mmio(0x0f00, 0x200), true);
    assert_eq!(rescan.revisit_mmio(0x1000, 0x100), true);
    assert_eq!(rescan.revisit_mmio(0x1200, 0x200), false);
    assert_eq!(rescan.revisit_mmio(0x1000, 0), false);
    assert_eq!(rescan.legacy_registered_count(), 0);
    assert_eq!(rescan.pending.len(), 1);
    assert_eq!(rescan.pending[0].id(), id);
    assert_eq!(kdevice::device_descs_snapshot().len(), 1);
    assert_eq!(rescan.probe_pending(), 0);
    assert_eq!(rescan.legacy_unclaimed_count(), 1);
    assert_eq!(rescan.legacy_unclaimed()[0].id(), id);
}
