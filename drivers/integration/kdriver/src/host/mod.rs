// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel service composition for the portable device framework.
//!
//! This module connects resource providers, management locks and extension
//! services to a domain. Device identity and cleanup remain model-owned;
//! native resource acquisition belongs to the host capability adapter.

mod management;
mod services;

pub use services::{new_device_services, new_native_device_services};
