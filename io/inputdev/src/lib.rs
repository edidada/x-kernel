// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Input device initialization and access helpers.
#![no_std]

#[macro_use]
extern crate log;
extern crate alloc;

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use kclass::{
    ClassDevice, InputDeviceImpl, input_devices as class_input_devices, subscribe_input_available,
};
use kdevice::subscribe_device_removed;
use kpoll::{PollContext, PollRegisterError, PollSet};
use ksync::Mutex;
use ktime_types::TimeSpan;
use lazyinit::LazyInit;

mod keyboard;

pub use keyboard::read_keyboard_bytes;

static KEYBOARD_WAITERS: LazyInit<PollSet> = LazyInit::new();
static KEYBOARD_WAKE_STARTED: AtomicBool = AtomicBool::new(false);

static INPUT_DEVICES: LazyInit<Mutex<Vec<ClassDevice<InputDeviceImpl>>>> = LazyInit::new();

/// Initialize the input subsystem with detected devices.
pub fn init_input() {
    info!("Initialize input subsystem...");

    INPUT_DEVICES.init_once(Mutex::new(Vec::new()));
    ensure_keyboard_wake();

    register_input_devices(class_input_devices());
    subscribe_input_available(Arc::new(register_input_device));

    subscribe_input_unregister();
}

fn ensure_keyboard_wake() {
    let _ = KEYBOARD_WAITERS.call_once(PollSet::new);
    if KEYBOARD_WAKE_STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    ktask::spawn_with_name(
        || loop {
            ktask::sleep(TimeSpan::from_millis(16));
            KEYBOARD_WAITERS.wake();
        },
        "kbd-wake".into(),
    );
}

/// Wake the console TTY reader when virtio-keyboard may have events.
///
/// The UART IRQ does not fire for virtio-input, so the TTY reader also waits
/// on this poll set. A 16ms wake is enough for typing and avoids a busy loop.
pub fn register_keyboard_wait(context: &mut PollContext<'_>) -> Result<(), PollRegisterError> {
    ensure_keyboard_wake();
    context.register(&KEYBOARD_WAITERS)
}

fn register_input_devices(devices: Vec<ClassDevice<InputDeviceImpl>>) {
    for handle in devices {
        register_input_device(handle);
    }
}

fn register_input_device(handle: ClassDevice<InputDeviceImpl>) {
    if !INPUT_DEVICES.is_inited() {
        return;
    }

    let mut devices = INPUT_DEVICES.lock();
    if devices.iter().any(|device| device.id() == handle.id()) {
        return;
    }

    info!(
        "  input device activated: {:?} (driver={}, {:?})",
        handle.name(),
        handle.driver_name(),
        handle.location(),
    );
    devices.push(handle);
}

fn subscribe_input_unregister() {
    subscribe_device_removed(Arc::new(|id| {
        if !INPUT_DEVICES.is_inited() {
            return;
        }
        let mut devs = INPUT_DEVICES.lock();
        if let Some(pos) = devs.iter().position(|h| h.id() == id) {
            devs.swap_remove(pos);
            info!("input: unregistered removed device {:?}", id);
        }
    }));
}

/// Snapshot registered input device handles without removing them.
///
/// Evdev, mousedev, and the TTY keyboard feeder all clone these handles. The
/// virtio device serializes `read_event`, so consumers must not all drain the
/// same device: mouse devices go to mousedev, keyboards go to the TTY path.
pub fn input_snapshot_devices() -> Vec<ClassDevice<InputDeviceImpl>> {
    if !INPUT_DEVICES.is_inited() {
        return Vec::new();
    }
    INPUT_DEVICES.lock().clone()
}

/// Drain all registered input device handles out of the input subsystem.
pub fn input_drain_devices() -> Vec<ClassDevice<InputDeviceImpl>> {
    if !INPUT_DEVICES.is_inited() {
        return Vec::new();
    }
    INPUT_DEVICES.lock().drain(..).collect()
}
