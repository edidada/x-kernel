// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Framework services supplied separately from device access.

use crate::{DeviceServices, ProbeError, extension::ExtensionService};

/// Callback-scoped access to host provider registration and extensions.
///
/// This environment has no device identity or bus type. Provider registration
/// is not tied to device removal and returns no withdrawal guard.
pub struct Environment<'a> {
    services: &'a dyn DeviceServices,
}

impl<'a> Environment<'a> {
    pub(crate) fn new(services: &'a dyn DeviceServices) -> Self {
        Self { services }
    }

    /// Look up a family-specific service extension on the installed host
    /// (e.g. the virtio transport service). The core names no family: the
    /// family registers its extension value at host install time, and the
    /// driver asks by type.
    pub fn extension<E: ExtensionService>(&self) -> Option<&'static E> {
        self.services.extensions().get::<E>()
    }

    /// Register a host provider (`of_clk_add_hw_provider`,
    /// `gpiochip_add_data`, `regulator_register`): the resource OTHER drivers
    /// acquire by firmware handle, and the thing their probe defers on until
    /// it appears.
    ///
    /// `C` is the provider family's [`ProviderClass`](crate::ProviderClass)
    /// descriptor; `key` is the class-scoped handle consumers name in their
    /// firmware reference; `lookup` is the class's lookup object. This is the
    /// single provider-registration door.
    ///
    /// Call it as the LAST fallible step of a provider driver's probe: a
    /// registration that wakes parked consumers cannot be rolled back by
    /// returning `Err` afterwards.
    pub fn provide<C: crate::ProviderClass>(
        &self,
        key: C::Key,
        lookup: &'static C::Lookup,
    ) -> Result<(), ProbeError> {
        self.services
            .register_provider(crate::ProviderRegistration::new::<C>(key, lookup))
            .map_err(ProbeError::Fatal)
    }
}
