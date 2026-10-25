// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::String, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use driver_model::{Bus, DevIrq, DmaWindow, MmioRegion, Virq};
use subsys_hwrng::{HwrngDevice, HwrngRegistry};
use unittest::{assert_eq, def_test};
use virtio::{
    StaticHal,
    mock_virtio::{MockHal, MockReset, MockTransport},
};

use super::*;

#[def_test]
fn rng_model_rejects_other_platform_instances() {
    let domain = DeviceDomain::new();
    let trusted = domain
        .add_bus_instance(PlatformBus::ID, None, "trusted")
        .unwrap();
    let other = domain
        .add_bus_instance(PlatformBus::ID, None, "other")
        .unwrap();
    let accepted = validate_instance(trusted, trusted);
    let rejected = validate_instance(trusted, other);
    drop(domain);
    assert_eq!(accepted, Ok(()));
    assert_eq!(rejected, Err(RuntimeError::Unsupported));
}

#[def_test]
fn rng_model_requires_coherent_identity_dma() {
    assert_eq!(validate_dma(None), Err(RuntimeError::Unsupported));
    for (window, coherent, mask, expected) in [
        (DmaWindow::IDENTITY, true, u64::MAX, Ok(())),
        (
            DmaWindow::new(0x4000, 0x4000, 0x1000).unwrap(),
            true,
            0xffff,
            Ok(()),
        ),
        (
            DmaWindow::new(0x4000, 0x2000, 0x1000).unwrap(),
            true,
            u64::MAX,
            Err(RuntimeError::Unsupported),
        ),
        (
            DmaWindow::IDENTITY,
            false,
            u64::MAX,
            Err(RuntimeError::Unsupported),
        ),
        (
            DmaWindow::IDENTITY,
            true,
            0x17ff,
            Err(RuntimeError::Invalid),
        ),
    ] {
        assert_eq!(
            validate_dma(Some(&DmaInfo {
                window,
                coherent,
                mask
            })),
            expected
        );
    }
}

#[def_test]
fn rng_model_identification_rejects_legacy_and_other_devices_without_reset() {
    for (version, device_id, expected) in [
        (2, 4, Ok(())),
        (1, 4, Err(RuntimeError::Unsupported)),
        (2, 2, Err(RuntimeError::Unsupported)),
        (2, 0, Err(RuntimeError::Unsupported)),
    ] {
        let mut registers = [0xa5a5_5a5au32; 64];
        registers[0] = 0x7472_6976;
        registers[1] = version;
        registers[2] = device_id;
        let before = registers;
        // SAFETY: this aligned initialized fixture remains live and exclusive
        // through validation and guard Drop; no real MMIO or hardware is used.
        let mapping = unsafe {
            IoMap::new(
                registers.as_mut_ptr() as usize,
                MmioRegion::new(0, 256),
                None,
            )
        };
        // SAFETY: the fixture has no concurrent mutation or transport owner.
        let result = unsafe { validate_mmio(&mapping) };
        drop(mapping);
        assert_eq!(result, expected);
        assert_eq!(registers, before);
    }
}

static REGISTRY: HwrngRegistry = HwrngRegistry::new();

#[def_test(serial)]
fn rng_model_binding_drop_stops_before_unmap_and_isolates_old_handles() {
    let mut observations = Vec::new();
    let mut old_handles = Vec::new();
    for publish in [true, true, false] {
        let mut registers = [0u32; 64];
        let reset = Arc::new(MockReset::default());
        let phase = Arc::new(AtomicUsize::new(0));
        let irq_saw_reset = Arc::new(AtomicBool::new(false));
        let unmap_saw_irq = Arc::new(AtomicBool::new(false));
        let drain = new_rng_drain().unwrap();
        let mut transport = MockTransport::new_rng();
        transport.reset = Some(reset.clone());
        let irq_phase = phase.clone();
        let irq_reset = reset.clone();
        let irq_observed = irq_saw_reset.clone();
        let runtime = Runtime::<MockTransport, StaticHal<MockHal>>::try_new_with_irq_factory(
            transport,
            StaticHal::default(),
            new_rng_completion(),
            move |_| {
                Ok(DevIrq::new(
                    Virq::new(905),
                    Some(Arc::new(move || {
                        irq_observed.store(
                            irq_reset.confirmations.load(Ordering::Relaxed) > 0,
                            Ordering::Relaxed,
                        );
                        irq_phase.store(1, Ordering::Relaxed);
                    })),
                ))
            },
            ktask::yield_now,
        )
        .unwrap();
        let map_phase = phase.clone();
        let map_observed = unmap_saw_irq.clone();
        // SAFETY: the initialized fixture outlives the binding's mapping; the
        // mock transport does not access it and the hook only records cleanup.
        let mapping = unsafe {
            IoMap::new(
                registers.as_mut_ptr() as usize,
                MmioRegion::new(0, 256),
                Some(Arc::new(move || {
                    map_observed.store(map_phase.load(Ordering::Relaxed) == 1, Ordering::Relaxed);
                    map_phase.store(2, Ordering::Relaxed);
                })),
            )
        };
        let mut binding = RngBinding {
            runtime,
            registration: None,
            mmio: ManuallyDrop::new(mapping),
            is_stopped: false,
        };
        if publish {
            let registration = REGISTRY
                .attach_legacy(
                    "virtio-rng-model-cleanup-test",
                    String::from("fixture/rng"),
                    binding.runtime.clone(),
                    drain,
                )
                .unwrap();
            old_handles.push(registration.publication());
            binding.registration = Some(registration);
        }
        let retained = binding.runtime.clone();
        binding.stop_or_fail_closed();
        binding.stop_or_fail_closed();
        let before_drop = phase.load(Ordering::Relaxed);
        drop(binding);
        let mut output = [0xa5u8; 8];
        let stale_read = retained.read(&mut output);
        observations.push((
            before_drop,
            phase.load(Ordering::Relaxed),
            irq_saw_reset.load(Ordering::Relaxed),
            unmap_saw_irq.load(Ordering::Relaxed),
            stale_read,
        ));
        drop(retained);
    }
    let stale_reads: Vec<_> = old_handles
        .iter()
        .map(|handle| {
            let mut output = [0xa5u8; 8];
            (handle.is_available(), handle.read(&mut output))
        })
        .collect();
    drop(old_handles);
    for (before_drop, phase, reset_seen, irq_seen, stale_read) in observations {
        assert_eq!(before_drop, 1);
        assert_eq!(phase, 2);
        assert_eq!(reset_seen, true);
        assert_eq!(irq_seen, true);
        assert_eq!(stale_read, Err(kerrno::KError::BadState));
    }
    for (available, read) in stale_reads {
        assert_eq!(available, false);
        assert_eq!(read, Err(kerrno::KError::BadState));
    }
}
