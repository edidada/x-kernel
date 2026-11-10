// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! MMC host controller drivers (Linux `drivers/mmc/host`): the
//! DesignWare Mobile Storage Host family.

#![no_std]

extern crate alloc;

pub mod dw_mmc;

#[cfg(feature = "dwmmc-rockchip")]
pub mod dw_mmc_rockchip;
