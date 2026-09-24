// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! driver-core block drivers: one module per device.

#![no_std]

extern crate alloc;

mod common;

pub mod ramdisk;
