// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! External proof harnesses over the production crate's public API.
#![no_std]

pub use ktime_types::*;

#[cfg(kani)]
#[path = "harnesses/mod.rs"]
mod verification;
