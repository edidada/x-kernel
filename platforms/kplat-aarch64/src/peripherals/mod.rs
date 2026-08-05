// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 peripheral drivers and firmware interface helpers.

pub mod memory;
#[cfg(any(feature = "nmi-pmu", feature = "nmi-sdei"))]
pub mod nmi;
#[cfg(feature = "pmu")]
pub mod pmu;
pub mod psci;
pub mod smccc;
