// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bridge between the `kreset` reset-line core and the device model.
//!
//! The mirror of [`crate::clk`](crate): [`kreset::register_provider`]
//! keeps the boot-static registry keyed by DT phandle; this module is its
//! device-model projection —
//!
//! * [`ResetRegistry::get`] turns a firmware [`ResetSpec`] into a consumer
//!   handle, mapping `NoProvider` to [`ServiceError::Deferred`];
//! * [`ResetRegistry::register_provider`] publishes a controller and fires
//!   the wake hook (`of_reset_controller_register` / `-EPROBE_DEFER`).
//!
//! A reset-controller driver ends its probe with
//! `dev.env().provide::<ResetProvider>(phandle, lookup)?`, and a consumer
//! calls `dev.res().reset("pcie")`.

use alloc::{boxed::Box, collections::BTreeSet, sync::Arc};

use driver_base::{
    DepKind, DepSource, ProviderClass, ResetHandle, ResetSpec, RuntimeError, ServiceError,
    devres::host_api::ResetOps,
};
use kreset::{ResetError, ResetLookup};
use kspin::SpinNoPreempt;

/// The reset provider family: what makes `kreset` providers reachable
/// through [`Device::provide`](driver_base::Device::provide).
pub struct ResetProvider;

impl ProviderClass for ResetProvider {
    /// DT phandle (the key consumers name in `resets = <&phandle n>`).
    type Key = u32;
    type Lookup = dyn ResetLookup;

    const KIND: DepKind = DepKind::Reset;
}

/// Newtype adapting a [`kreset::ResetHandle`] to the device-model reset
/// operations (the orphan-rule bridge).
pub(crate) struct KresetHandle(kreset::ResetHandle);

impl ResetOps for KresetHandle {
    fn assert(&self) {
        self.0.assert();
    }

    fn deassert(&self) {
        self.0.deassert();
    }

    fn status(&self) -> bool {
        self.0.status()
    }
}

/// Device-model view of the kreset provider registry.
pub struct ResetRegistry {
    wake: Box<dyn Fn(DepSource) + Send + Sync>,
    /// Phandles already served; a duplicate publication is refused (same
    /// reasoning as the clock registry).
    registered: SpinNoPreempt<BTreeSet<u64>>,
}

impl ResetRegistry {
    pub fn new(wake: impl Fn(DepSource) + Send + Sync + 'static) -> Self {
        Self {
            wake: Box::new(wake),
            registered: SpinNoPreempt::new(BTreeSet::new()),
        }
    }

    /// Publish a reset controller (`of_reset_controller_register`
    /// analogue) and wake every probe parked on it.
    pub fn register_provider(
        &self,
        provider: &'static dyn ResetLookup,
    ) -> Result<(), RuntimeError> {
        let provider_id = provider.provider_id();
        if !self.registered.lock().insert(u64::from(provider_id)) {
            log::warn!("driver-host: reset provider {provider_id:#x} registered twice");
            return Err(RuntimeError::AlreadyExists);
        }
        kreset::register_provider(provider);
        log::debug!("driver-host: reset provider {provider_id:#x} registered");
        (self.wake)(DepSource::reset(u64::from(provider_id)));
        Ok(())
    }

    /// Acquire a reset line, or report the dependency we are waiting for.
    pub fn get(&self, spec: ResetSpec) -> Result<ResetHandle, ServiceError> {
        let provider_id = u32::try_from(spec.provider).map_err(|_| RuntimeError::Invalid)?;
        match kreset::reset_get(provider_id, spec.index) {
            Ok(handle) => Ok(ResetHandle::new(Arc::new(KresetHandle(handle)), None)),
            Err(ResetError::NoProvider) => {
                Err(ServiceError::Deferred(DepSource::reset(spec.provider)))
            }
            Err(err) => Err(ServiceError::Fatal(match err {
                ResetError::NoProvider | ResetError::NotFound => RuntimeError::NotFound,
            })),
        }
    }
}
