// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Sleepable host management-lock capability for the portable device model.
//!
//! Lifecycle unit tests install this capability through `DeviceServices`.
//! Production domain service installation remains a composition-root concern.

use alloc::sync::Arc;

use driver_model::{DeviceManagementError, DeviceManagementLock};
use ksync::Mutex;

/// Creates an independent management lock in a sleepable current task.
///
/// Successful `run_locked` calls execute the callback synchronously exactly
/// once and release the lock before returning. The callback must restore its
/// execution context and must not wait for work that needs this same lock.
/// Allocation follows the kernel's `Arc` allocation-failure policy.
///
/// # Errors
///
/// Returns [`DeviceManagementError::InvalidContext`] before allocation when
/// the current context cannot sleep. The returned capability also rejects
/// invalid context before checking ownership or taking its lock, and rejects
/// same-task recursion with [`DeviceManagementError::Reentrant`]. Neither
/// rejection invokes the callback or acquires the lock.
pub fn new_device_management_lock() -> Result<Arc<dyn DeviceManagementLock>, DeviceManagementError>
{
    if !ktask::is_sleepable_context() {
        return Err(DeviceManagementError::InvalidContext);
    }
    Ok(Arc::new(HostDeviceManagementLock {
        lock: Mutex::new(()),
    }))
}

struct HostDeviceManagementLock {
    lock: Mutex<()>,
}

impl DeviceManagementLock for HostDeviceManagementLock {
    fn run_locked(&self, operation: &mut dyn FnMut()) -> Result<(), DeviceManagementError> {
        if !ktask::is_sleepable_context() {
            return Err(DeviceManagementError::InvalidContext);
        }
        if self.lock.is_owned_by_current() {
            return Err(DeviceManagementError::Reentrant);
        }
        let _guard = self.lock.lock();
        operation();
        Ok(())
    }
}

unittest::path_mod_test!(
    device_management_unittest,
    "../ut-tests/device_management_unittest.rs"
);

#[cfg(feature = "hwrng")]
unittest::path_mod_test!(
    device_management_lifecycle_unittest,
    "../ut-tests/device_management_lifecycle_unittest.rs"
);

#[cfg(feature = "hwrng")]
unittest::path_mod_test!(
    device_management_downstream_unittest,
    "../ut-tests/device_management_downstream_unittest.rs"
);

#[cfg(feature = "hwrng")]
unittest::path_mod_test!(
    hwrng_binding_unittest,
    "../ut-tests/hwrng_binding_unittest.rs"
);

#[cfg(feature = "virtio-rng")]
unittest::path_mod_test!(
    virtio_rng_lifecycle_unittest,
    "../ut-tests/virtio_rng_lifecycle_unittest.rs"
);
