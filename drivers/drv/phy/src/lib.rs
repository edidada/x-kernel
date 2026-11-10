// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! driver-core phy provider drivers (rk3588).

#![no_std]

extern crate alloc;

pub mod grf;

#[cfg(feature = "rk3588-usb2phy")]
pub mod usb2phy;

#[cfg(feature = "rk3588-usbdp")]
pub mod usbdp;

#[cfg(feature = "rk3588-combphy")]
pub mod combphy;

#[cfg(feature = "rk3588-pcie3phy")]
pub mod pcie3phy;
