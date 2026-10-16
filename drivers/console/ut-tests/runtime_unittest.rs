// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::boxed::Box;


use subsys_char::CharDevice;
use unittest::def_test;

use super::*;

struct DummyConsole;

impl CharDevice for DummyConsole {
    fn read(&self, _: &mut [u8]) -> KResult<usize> {
        panic!("identity test must not perform device I/O")
    }

    fn write(&self, _: &[u8]) -> KResult<usize> {
        panic!("identity test must not perform device I/O")
    }
}

#[def_test]
fn stdout_identity_requires_local_handle_and_clears_on_removal() {
    // Detached class parent: never touches a registry — identity test only.
    // Offline mirrors the main-side bound-but-inactive device object: the
    // handle exists, availability is false.
    let parent = driver_base::ClassParent::detached(
        1,
        "console-test",
        alloc::string::String::from("platform/static1"),
    );
    parent.mark_offline();
    let runtime: CharDeviceImpl = Box::new(DummyConsole);
    let stdout = ClassDevice::try_new_with_info(parent, runtime, "console-test".into()).unwrap();
    assert!(!stdout.is_available());
    let id = stdout.id();
    let mut subsystem = ConsoleSubsystem::new();
    subsystem.stdout = Some(id);
    // The ID alone cannot resolve a handle absent from this local list.
    // Adding it below does not activate the independent Bound device.
    assert!(subsystem.stdout_handle().is_none());
    subsystem.add(stdout);
    assert_eq!(subsystem.stdout_handle().unwrap().id(), id);
    assert!(!subsystem.stdout_handle().unwrap().is_available());
    let other = driver_base::ClassDeviceId::new_detached();
    assert!(subsystem.remove(other).is_none());
    assert_eq!(subsystem.stdout_handle().unwrap().id(), id);
    assert!(subsystem.remove(id).is_some());
    assert!(subsystem.stdout_handle().is_none());
    assert!(subsystem.stdout.is_none());
}
