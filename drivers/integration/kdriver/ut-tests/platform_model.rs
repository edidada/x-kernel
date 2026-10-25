// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn selected_mmio_reservation_blocks_aliases_and_drop_allows_retry() {
    let owner = PlatformModel::new();
    let mut observed = None;
    khal::firmware::devices::for_each_compatible_view(
        |compatible| (compatible == "virtio,mmio").then_some("virtio,mmio"),
        |view| {
            if observed.is_some() || !view.is_root_child_of("linux,dummy-virt") {
                return;
            }
            let Some(mmio) = view.description().mmio else {
                return;
            };
            let staged = owner.stage(&view).unwrap().unwrap();
            let owns = owner.owns_range(mmio.base, mmio.size);
            let aliases = owner.owns_range(mmio.base + 4, 4);
            let duplicate = owner.stage(&view).unwrap().is_none();
            let irq = view.description().irq.unwrap();
            let trigger = match irq.trigger {
                InterruptTrigger::EdgeRising => IrqTrigger::EdgeRising,
                InterruptTrigger::EdgeFalling => IrqTrigger::EdgeFalling,
                InterruptTrigger::LevelHigh => IrqTrigger::LevelHigh,
                InterruptTrigger::LevelLow => IrqTrigger::LevelLow,
                InterruptTrigger::Unknown(raw) => IrqTrigger::Unknown(raw),
            };
            let preserves_irq = staged.resources.irq(0)
                == Some(IrqSpec::firmware(IrqController::Gic, irq.irq, trigger));
            let identity = staged.dma.window.is_identity() && staged.dma.coherent;
            drop(staged);
            let released = !owner.owns_range(mmio.base, mmio.size);
            let retry = owner.stage(&view).unwrap().unwrap();
            drop(retry);
            observed = Some((
                owns,
                aliases,
                duplicate,
                preserves_irq,
                identity,
                released,
                !owner.owns_range(mmio.base, mmio.size),
            ));
        },
    );
    let Some(observed) = observed else {
        return unittest::TestResult::Ignored;
    };
    assert_eq!(observed, (true, true, true, true, true, true, true));
}

#[def_test]
fn staged_registration_rejects_atomic_context_and_releases_reservation() {
    let owner = PlatformModel::new();
    let mut observed = None;
    khal::firmware::devices::for_each_compatible_view(
        |compatible| (compatible == "virtio,mmio").then_some("virtio,mmio"),
        |view| {
            if observed.is_some() || !view.is_root_child_of("linux,dummy-virt") {
                return;
            }
            let Some(mmio) = view.description().mmio else {
                return;
            };
            let staged = owner.stage(&view).unwrap().unwrap();
            let atomic = kspin::SpinNoPreempt::new(());
            let guard = atomic.lock();
            let result = staged.register();
            drop(guard);
            observed = Some((
                result,
                !owner.owns_range(mmio.base, mmio.size),
                owner.root.lock().is_none(),
            ));
        },
    );
    let Some(observed) = observed else {
        return unittest::TestResult::Ignored;
    };
    assert_eq!(observed, (Err(RuntimeError::BadState), true, true));
}

#[def_test(serial)]
fn modern_mmio_boot_rng_is_active_in_the_model() {
    if option_env!("XK_EXPECT_MODEL_MMIO_RNG") != Some("1") {
        return unittest::TestResult::Ignored;
    }
    assert!(cfg!(target_arch = "aarch64"));
    assert!(kbuild_config::KFEAT_VIRTIO_BUS_MMIO);
    let devices = global_domain().devices();
    let selected: alloc::vec::Vec<_> = devices
        .iter()
        .filter(|snapshot| snapshot.driver == Some("virtio-rng-mmio-model"))
        .collect();
    assert!(!selected.is_empty());
    for snapshot in selected {
        assert_eq!(snapshot.state, driver_model::DeviceState::Active);
        let region = snapshot.device.descriptor().resources.mmio(0).unwrap();
        assert!(!kdevice::device_descs_snapshot().iter().any(|legacy| {
            matches!(legacy.location(), kdevice::DeviceLocation::Mmio { base, .. } if base == region.base)
        }));
    }
}

#[def_test(serial)]
fn independent_enumeration_contexts_share_production_claims_and_discovery_lock() {
    let first = crate::enumeration::EnumerationContext::new();
    let mut second = crate::enumeration::EnumerationContext::default();
    let shared = Arc::ptr_eq(&first.model_owner, &second.model_owner);
    let owner = first.model_owner.clone();
    let discovery = owner.discovery.lock();
    let serialized = second.model_owner.discovery.try_lock().is_none();
    let node = u32::MAX;
    let region = MmioRegion::new(usize::MAX - 0x2000, 0x1000);
    let vacant = !owner.slots.lock().contains_key(&node);
    assert!(vacant);
    owner.slots.lock().insert(node, Slot::Reserved(region));
    let revisited = second.revisit_mmio(region.base, region.size);
    let overlapping = second.revisit_mmio(region.base + 4, 4);
    owner.slots.lock().remove(&node);
    drop(discovery);
    let released = !second.model_owner.owns_range(region.base, region.size);
    assert!(shared);
    assert!(serialized);
    assert!(revisited);
    assert!(overlapping);
    assert!(released);
}
