// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! One-shot real-device unregistration acceptance in a dedicated guest.
//!
//! The exact module filter is mandatory: removal is permanent for this boot,
//! and no other test may depend on the removed device. The entropy worker and
//! other lifecycle opt-ins must be disabled. Coverage and guest shutdown follow
//! this case; no registration is resurrected to manufacture test cleanup.
//! The fixed boot-DT owner retains its claim to prevent legacy rediscovery.
//! This is not a hotplug/re-enumeration, pending or IRQ-tail safety test.
//! Reads and removal use the ordinary APIs; an outer watchdog only fails hangs.

use alloc::{string::String, vec::Vec};

use driver_model::{DeviceState, RuntimeError, global_domain};
use subsys_hwrng::{HWRNG, HwrngHandle};
use unittest::{assert, assert_eq, def_test};

const DRIVER_NAME: &str = "virtio-rng-mmio-model";
const MODULE_FILTER: &str = "kdriver::platform_model::unregister_tests";

fn publications() -> Vec<HwrngHandle> {
    HWRNG
        .devices()
        .into_iter()
        .filter(|handle| handle.driver_name() == Some(DRIVER_NAME))
        .collect()
}

fn has_legacy_device(base: usize) -> bool {
    kdevice::device_descs_snapshot().iter().any(|legacy| {
        matches!(legacy.location(), kdevice::DeviceLocation::Mmio { base: start, .. } if start == base)
    })
}

/// Consume more than the concrete RNG's current 256-byte cache before removal.
fn sample_is_valid(handle: &HwrngHandle) -> bool {
    let mut remaining = 257;
    while remaining > 0 {
        let mut buffer = [0xa5u8; 265];
        match handle.read(&mut buffer[4..4 + remaining]) {
            Ok(count) if count > 0 && count <= remaining => {
                if buffer[..4] != [0xa5; 4] || !buffer[4 + count..].iter().all(|byte| *byte == 0xa5)
                {
                    return false;
                }
                remaining -= count;
            }
            _ => return false,
        }
    }
    true
}

#[def_test(serial)]
fn real_model_mmio_rng_unregister_closes_retained_references() {
    if option_env!("XK_TEST_MODEL_MMIO_UNREGISTER") != Some("1") {
        return unittest::TestResult::Ignored;
    }
    assert_eq!(option_env!("UNITTEST_CRATE"), Some(MODULE_FILTER));
    assert!(cfg!(target_arch = "aarch64"));
    assert!(kbuild_config::KFEAT_VIRTIO_BUS_MMIO);
    assert!(!kbuild_config::KFEAT_ENTROPY_VIRTIO_RNG);
    assert!(option_env!("XK_TEST_MODEL_MMIO_LIFECYCLE") != Some("1"));
    assert!(option_env!("XK_TEST_MODEL_MMIO_PENDING") != Some("1"));
    assert!(option_env!("XK_TEST_MODEL_MMIO_ISOLATION") != Some("1"));
    assert!(ktask::is_sleepable_context());
    let domain = global_domain();
    let mut devices: Vec<_> = domain
        .devices()
        .into_iter()
        .filter(|snapshot| snapshot.driver == Some(DRIVER_NAME))
        .collect();
    let mut handles = publications();
    assert_eq!(devices.len(), 1);
    assert_eq!(handles.len(), 1);
    let snapshot = devices.pop().unwrap();
    assert_eq!(snapshot.state, DeviceState::Active);
    let device = snapshot.device;
    let retained = device.clone();
    let handle = handles.pop().unwrap();
    let path = String::from(device.path());
    let region = device.descriptor().resources.mmio(0).unwrap();
    let owner = super::production_owner();
    assert!(device.is_online());
    assert!(owner.owns_range(region.base, region.size));
    assert!(!has_legacy_device(region.base));
    assert!(sample_is_valid(&handle));

    assert_eq!(domain.remove_device(&device), Ok(()));
    assert!(!retained.is_online());
    assert!(retained.same_device(&device));
    assert_eq!(retained.path(), path.as_str());
    assert_eq!(retained.descriptor().resources.mmio(0), Some(region));
    assert_eq!(retained.driver_name(), None);
    assert!(domain.snapshot(&retained).is_none());
    assert!(
        !domain
            .devices()
            .iter()
            .any(|entry| entry.device.same_device(&retained))
    );
    assert!(publications().is_empty());
    assert!(!handle.is_available());
    let mut buffer = [0xa5u8; 16];
    assert_eq!(handle.read(&mut buffer), Err(kerrno::KError::BadState));
    assert_eq!(buffer, [0xa5; 16]);
    assert!(matches!(
        domain.bind_device(&retained),
        Err(RuntimeError::NotFound)
    ));
    assert_eq!(domain.unbind_device(&retained), Err(RuntimeError::NotFound));
    assert_eq!(domain.remove_device(&retained), Ok(()));
    assert!(domain.snapshot(&retained).is_none());
    assert!(publications().is_empty());
    assert!(owner.owns_range(region.base, region.size));
    let mut enumeration = crate::enumeration::EnumerationContext::new();
    assert!(enumeration.revisit_mmio(region.base, region.size));
    assert!(!has_legacy_device(region.base));
}
