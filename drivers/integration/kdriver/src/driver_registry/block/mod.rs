// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Block-device descriptors managed by the unified driver pipeline.

#[cfg(feature = "ramdisk")]
mod ramdisk;

const DRIVER_FACTORIES: &[crate::driver_registry::DriverFactory] = &[
    #[cfg(feature = "ramdisk")]
    ramdisk::descriptor,
];

pub(crate) fn register_all(registrar: &mut crate::driver_registry::DriverRegistrar) {
    crate::driver_registry::register_factories(registrar, DRIVER_FACTORIES);
}
