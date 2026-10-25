// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Publication provenance retains metadata, not the binding's implementation.

use alloc::sync::Arc;

use super::DeviceRef;
use crate::driver::DriverIdentity;

pub(super) struct BindingSource {
    pub(super) device: DeviceRef,
    pub(super) driver: Arc<DriverIdentity>,
}

impl driver_class::PublicationSource for BindingSource {
    fn driver_name(&self) -> Option<&'static str> {
        Some(self.driver.name)
    }

    fn physical_path(&self) -> &str {
        self.device.path()
    }

    fn is_online(&self) -> bool {
        self.device.is_online()
    }
}
