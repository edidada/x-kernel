// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! driver-core clock provider drivers.

#![no_std]

extern crate alloc;

#[cfg(feature = "fixed-clock")]
pub mod fixed_clock;

#[cfg(feature = "fixed-factor")]
pub mod fixed_factor;

#[cfg(feature = "scmi-clock")]
pub mod scmi_clock;
