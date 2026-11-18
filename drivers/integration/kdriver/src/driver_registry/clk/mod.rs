// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Clock provider drivers managed by the unified driver pipeline.

#[cfg(feature = "clk-fixed")]
mod fixed_clock;

#[cfg(feature = "clk-fixed")]
const DRIVER_FACTORIES: &[crate::driver_registry::DriverFactory] = &[
    #[cfg(feature = "clk-fixed")]
    fixed_clock::descriptor,
];

/// Register every enabled clock provider driver.
pub(crate) fn register_all(registrar: &mut crate::driver_registry::DriverRegistrar) {
    crate::driver_registry::register_factories(registrar, DRIVER_FACTORIES);
}

/// The production fixed-clock driver, for the unittest chain regressions.
#[cfg(all(feature = "clk-fixed", unittest))]
pub(crate) fn fixed_clock_driver() -> crate::driver_registry::BoxedDriver {
    fixed_clock::descriptor()
}
