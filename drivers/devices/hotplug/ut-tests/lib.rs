// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kdevice::system_device_snapshot;
use unittest::{assert, assert_eq, def_test};

use super::*;

struct DuplicateSource;

impl HotplugEventSource for DuplicateSource {
    fn start(&self, _sink: &'static dyn HotplugSink) -> KResult<()> {
        Ok(())
    }
}

static DUPLICATE_SOURCE: DuplicateSource = DuplicateSource;

#[def_test]
fn cpulist_collapses_ranges_and_deduplicates() {
    assert_eq!(format_cpulist([4, 0, 2, 1, 2]), String::from("0-2,4\n"));
    assert_eq!(format_cpulist([]), String::from("\n"));
}

#[def_test]
fn block_bitmap_uses_low_word_first_hex() {
    assert_eq!(format_block_bitmap([0, 3]), String::from("9\n"));
    assert_eq!(format_block_bitmap([]), String::from("0\n"));
    assert_eq!(format_block_bitmap([32]), String::from("100000000\n"));
}

#[def_test]
fn repeated_event_source_registration_is_rejected() {
    assert!(register_event_source(&DUPLICATE_SOURCE).is_ok());
    let error = register_event_source(&DUPLICATE_SOURCE).unwrap_err();
    assert_eq!(error, KError::AlreadyExists);
}

use alloc::sync::Arc;

use kdevice::{
    SystemDeviceDescriptor, install_system_device, present_system_devices, remove_system_device,
    set_system_device_online, system_device_state,
};
use kspin::{SpinNoPreempt, static_lock};

use crate::{
    cpu::{CpuControl, validate_cpu_request},
    memory::{MemoryBackend, MemoryControl},
};

static_lock! {
    static TEST_LOCK: SpinNoPreempt<()> = SpinNoPreempt::new(());
}

struct FakeMemoryState {
    mapping_succeeds: bool,
    add_succeeds: bool,
    remove_succeeds: bool,
    is_mapped: bool,
    map_calls: usize,
    unmap_calls: usize,
    add_calls: usize,
    remove_calls: usize,
}

struct FakeMemoryBackend {
    state: SpinNoPreempt<FakeMemoryState>,
}

impl FakeMemoryBackend {
    const fn new() -> Self {
        Self {
            state: SpinNoPreempt::new(FakeMemoryState {
                mapping_succeeds: true,
                add_succeeds: true,
                remove_succeeds: true,
                is_mapped: false,
                map_calls: 0,
                unmap_calls: 0,
                add_calls: 0,
                remove_calls: 0,
            }),
        }
    }

    fn reset(&self, mapping_succeeds: bool, add_succeeds: bool, remove_succeeds: bool) {
        let mut state = self.state.lock();
        *state = FakeMemoryState {
            mapping_succeeds,
            add_succeeds,
            remove_succeeds,
            is_mapped: false,
            map_calls: 0,
            unmap_calls: 0,
            add_calls: 0,
            remove_calls: 0,
        };
    }
}

impl MemoryBackend for FakeMemoryBackend {
    fn ensure_direct_mapped(&self, _start: u64, _size: u64) -> KResult<bool> {
        let mut state = self.state.lock();
        state.map_calls += 1;
        if !state.mapping_succeeds {
            return Err(KErrorKind::InvalidInput.into());
        }
        let was_mapped = state.is_mapped;
        state.is_mapped = true;
        Ok(!was_mapped)
    }

    fn unmap_direct(&self, _start: u64, _size: u64) -> KResult<()> {
        let mut state = self.state.lock();
        state.unmap_calls += 1;
        state.is_mapped = false;
        Ok(())
    }

    fn add_to_allocator(&self, _start: u64, _size: u64) -> KResult<()> {
        let mut state = self.state.lock();
        state.add_calls += 1;
        if !state.add_succeeds {
            return Err(KErrorKind::ResourceBusy.into());
        }
        Ok(())
    }

    fn remove_from_allocator(&self, _start: u64, _size: u64) -> KResult<()> {
        let mut state = self.state.lock();
        state.remove_calls += 1;
        if !state.remove_succeeds {
            return Err(KErrorKind::ResourceBusy.into());
        }
        Ok(())
    }
}

static FAKE_MEMORY_BACKEND: FakeMemoryBackend = FakeMemoryBackend::new();

fn install_cpu_for_test(index: u32, is_online: bool) -> SystemDeviceId {
    let id = SystemDeviceId::new(SystemClass::Cpu, index);
    if system_device_state(id).is_none() {
        let installation = install_system_device(SystemDeviceDescriptor::new(
            id,
            0,
            0,
            is_online,
            Arc::new(CpuControl::new(index)),
        ));
        debug_assert!(installation.is_ok(), "test CPU installation failed");
    }
    id
}

fn install_memory_for_test(
    index: u32,
    size_bytes: u64,
    is_online: bool,
    control: Arc<MemoryControl>,
) -> SystemDeviceId {
    let id = SystemDeviceId::new(SystemClass::Memory, index);
    if system_device_state(id).is_none() {
        let installation = install_system_device(SystemDeviceDescriptor::new(
            id,
            u64::from(index) * MEMORY_BLOCK_SIZE_BYTES,
            size_bytes,
            is_online,
            control,
        ));
        debug_assert!(installation.is_ok(), "test memory installation failed");
    }
    id
}

#[def_test]
fn cpu_boot_and_last_online_rejections_are_policy_errors() {
    let _guard = TEST_LOCK.lock();

    assert_eq!(
        validate_cpu_request(0, true, true, false, 2).unwrap_err(),
        KError::OperationNotPermitted
    );
    assert_eq!(
        validate_cpu_request(7, false, true, false, 1).unwrap_err(),
        KError::OperationNotPermitted
    );
    assert_eq!(
        validate_cpu_request(7, false, false, true, 2).unwrap_err(),
        KError::OperationNotSupported
    );
    assert!(validate_cpu_request(7, false, true, true, 2).is_ok());
}

#[def_test]
fn cpu_controller_rejects_absent_boot_and_unavailable_transitions() {
    let _guard = TEST_LOCK.lock();
    let controller = CpuHotplugController::new();

    let absent = SystemDeviceId::new(SystemClass::Cpu, 50_000);
    assert_eq!(
        controller.set_online(absent, true).unwrap_err(),
        KError::NoSuchDevice
    );

    let boot_cpu = install_cpu_for_test(0, true);
    assert_eq!(
        controller.set_online(boot_cpu, false).unwrap_err(),
        KError::OperationNotPermitted
    );

    let online_cpu = install_cpu_for_test(50_001, true);
    assert!(controller.set_online(online_cpu, true).is_ok());

    let offline_cpu = install_cpu_for_test(50_002, false);
    assert_eq!(
        controller.set_online(offline_cpu, true).unwrap_err(),
        KError::OperationNotSupported
    );
}

#[def_test]
fn memory_online_failure_rolls_back_mapping_and_published_state() {
    let _guard = TEST_LOCK.lock();
    FAKE_MEMORY_BACKEND.reset(true, false, true);
    let id = SystemDeviceId::new(SystemClass::Memory, 1001);
    let control = Arc::new(MemoryControl::with_backend(id, &FAKE_MEMORY_BACKEND));
    let id = install_memory_for_test(1001, MEMORY_BLOCK_SIZE_BYTES, false, control);

    assert_eq!(
        set_system_device_online(id, true).unwrap_err(),
        KError::ResourceBusy
    );
    assert!(!system_device_state(id).unwrap().is_online);

    let state = FAKE_MEMORY_BACKEND.state.lock();
    assert_eq!(state.map_calls, 1);
    assert_eq!(state.unmap_calls, 1);
    assert_eq!(state.add_calls, 1);
    assert_eq!(state.remove_calls, 0);
    assert!(!state.is_mapped);
    drop(state);

    remove_system_device(id).unwrap();
}

#[def_test]
fn memory_offline_busy_leaves_the_device_online() {
    let _guard = TEST_LOCK.lock();
    FAKE_MEMORY_BACKEND.reset(true, true, false);
    let id = SystemDeviceId::new(SystemClass::Memory, 1002);
    let control = Arc::new(MemoryControl::with_backend(id, &FAKE_MEMORY_BACKEND));
    let id = install_memory_for_test(1002, MEMORY_BLOCK_SIZE_BYTES, true, control);

    assert_eq!(
        set_system_device_online(id, false).unwrap_err(),
        KError::ResourceBusy
    );
    assert!(system_device_state(id).unwrap().is_online);

    let state = FAKE_MEMORY_BACKEND.state.lock();
    assert_eq!(state.remove_calls, 1);
    assert_eq!(state.unmap_calls, 0);
    drop(state);

    remove_system_device(id).unwrap();
}

#[def_test]
fn memory_controller_requires_one_aligned_hotplug_block() {
    let _guard = TEST_LOCK.lock();
    let id = SystemDeviceId::new(SystemClass::Memory, 1003);
    let control = Arc::new(MemoryControl::with_backend(id, &FAKE_MEMORY_BACKEND));
    let id = install_memory_for_test(1003, 4096, false, control);

    assert_eq!(
        MEMORY_CONTROLLER.set_online(id, true).unwrap_err(),
        KError::InvalidInput
    );
    assert!(!system_device_state(id).unwrap().is_online);
    remove_system_device(id).unwrap();
}

#[def_test]
fn boot_topology_is_present_without_a_started_event_source() {
    let _guard = TEST_LOCK.lock();
    crate::init();
    let published = system_device_snapshot();

    let cpus = present_system_devices(SystemClass::Cpu);
    let memory = present_system_devices(SystemClass::Memory);
    assert!(!cpus.is_empty());
    assert!(!memory.is_empty());
    assert!(controller_for(SystemClass::Cpu).is_some());
    assert!(controller_for(SystemClass::Memory).is_some());
    for id in cpus.into_iter().chain(memory) {
        assert!(system_device_state(id).unwrap().is_present);
    }

    // Both the init-array slot and sysfs may request initialization. A repeat
    // must keep the existing publication rather than panic or duplicate it.
    crate::init();
    assert_eq!(system_device_snapshot(), published);
}
