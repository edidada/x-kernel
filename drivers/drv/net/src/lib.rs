// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network interface drivers behind the driver core's `net` class.

#![no_std]
#![cfg_attr(doc, feature(doc_cfg))]

extern crate alloc;

#[cfg(feature = "rk3588-rtl8125")]
pub mod rtl8125;
