// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Publication provenance retains metadata, not the binding's implementation.

use alloc::sync::Arc;

use super::DeviceRef;
use crate::driver::DriverIdentity;

/// Read-only provenance for a subsystem capability provider.
///
/// Retains metadata only: it grants no operation permit or hardware access.
/// The driver identity belongs to the publishing binding and must not change
/// after rebind. Device registration liveness is independent of subsystem
/// admission. Methods run in task context and must not block or reenter a
/// subsystem registry. Final destruction must not require sleeping.
pub trait PublicationSource: Send + Sync + 'static {
    /// The publishing driver; absent identity prevents subsystem registration.
    fn driver_name(&self) -> Option<&'static str>;

    /// Stable physical location, not a publication uniqueness key.
    fn physical_path(&self) -> &str;

    /// Device registration liveness, not permission to use a runtime.
    fn is_online(&self) -> bool;
}

pub(super) struct BindingSource {
    pub(super) device: DeviceRef,
    pub(super) driver: Arc<DriverIdentity>,
}

impl PublicationSource for BindingSource {
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
