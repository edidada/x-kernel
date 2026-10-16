// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `hwrng` class: its publication API (publish/devices/find/
//! subscribe), removal entries, and the class hooks — instantiated on
//! the shared machinery in `driver-base::class` (the Linux
//! drivers/char/hw_random shape: the subsystem owns its whole class story).

use alloc::{string::String, sync::Arc, vec::Vec};

use driver_base::{
    ClassAvailabilityCallback, ClassDevice, ClassDeviceId, ClassParent, ClassRegistry,
    ClassRemovalCallback, ClassWithdraw,
    class::class_api::{ClassHooks, class_notify},
};
use kerrno::KResult;
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;

use super::HwrngDevice;

pub type HwrngDeviceImpl = Arc<dyn HwrngDevice + Send + Sync>;

static HOOKS: ClassHooks<HwrngDeviceImpl> = ClassHooks::passthrough();

driver_base::class_api!(
    "hwrng",
    HwrngDeviceImpl,
    HWRNG_DEVICES,
    hwrng_class_registry,
    publish_hwrng,
    hwrng_devices,
    find_hwrng_device,
    subscribe_hwrng_available,
    subscribe_hwrng_removed,
    HOOKS
);

/// Unpublish one `hwrng` class device by publication id.
pub fn unpublish_hwrng(id: ClassDeviceId) {
    driver_base::class::class_api::class_remove(hwrng_class_registry, &HOOKS, id);
}

/// Guard form: withdraws this publication when the activation drops.
pub fn hwrng_withdraw(device: &ClassDevice<HwrngDeviceImpl>) -> ClassWithdraw {
    ClassWithdraw::new(unpublish_hwrng, device.id())
}
