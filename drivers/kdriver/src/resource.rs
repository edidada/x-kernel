// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Compatibility re-exports for X-Kernel's [`device_res`] provider adapter.
//!
//! The implementation lives in `device-res-xkernel`; `kdriver` keeps this
//! module only so existing internal paths such as `resource::devm_iomap`
//! remain local to the driver orchestration crate.

pub use device_res_xkernel::{
    devm_alloc_coherent, devm_iomap, devm_request_irq, devm_request_threaded_irq,
    devm_request_threaded_irq_default, install_resource_provider,
};
