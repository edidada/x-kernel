// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bridge between the `kphy` phy core and the device model — the mirror
//! of [`crate::reset`](crate): [`kphy::register_provider`] keeps the
//! boot-static registry; this module projects it —
//!
//! * [`PhyRegistry::get`] turns a firmware [`PhySpec`] into a consumer
//!   handle, mapping `NoProvider` to [`ServiceError::Deferred`];
//! * [`PhyRegistry::register_provider`] publishes a phy and fires the
//!   wake hook (`of_phy_provider_register` / `-EPROBE_DEFER`).
//!
//! A phy driver ends its probe with
//! `dev.env().provide::<PhyProvider>(phandle, lookup)?`, and a consumer
//! calls `dev.res().phy("pcie-phy")`.

use alloc::{boxed::Box, collections::BTreeSet, sync::Arc};

use driver_base::{
    DepKind, DepSource, PhyHandle, PhySpec, ProviderClass, RuntimeError, ServiceError,
    devres::host_api::PhyOps,
};
use kphy::{PhyError, PhyLookup};
use kspin::SpinNoPreempt;

/// The phy provider family: what makes `kphy` providers reachable through
/// [`Device::provide`](driver_base::Device::provide).
pub struct PhyProvider;

impl ProviderClass for PhyProvider {
    /// DT phandle (the key consumers name in `phys = <&phandle ...>`).
    type Key = u32;
    type Lookup = dyn PhyLookup;

    const KIND: DepKind = DepKind::Phy;
}

/// Newtype adapting a [`kphy::PhyHandle`] to the device-model phy
/// operations (the orphan-rule bridge). The counts live on the shared
/// kphy instance the handle wraps.
pub(crate) struct KphyHandle(kphy::PhyHandle);

impl PhyOps for KphyHandle {
    fn init(&self) -> Result<(), RuntimeError> {
        self.0.init().map_err(map_phy_error)
    }

    fn exit(&self) -> Result<(), RuntimeError> {
        self.0.exit().map_err(map_phy_error)
    }

    fn power_on(&self) -> Result<(), RuntimeError> {
        self.0.power_on().map_err(map_phy_error)
    }

    fn power_off(&self) {
        if let Err(err) = self.0.power_off() {
            log::warn!("phy: unbalanced power_off: {err:?}");
        }
    }

    fn set_mode(&self, mode: u32) -> Result<(), RuntimeError> {
        self.0.set_mode(mode).map_err(map_phy_error)
    }
}

fn map_phy_error(err: PhyError) -> RuntimeError {
    match err {
        PhyError::NoProvider | PhyError::NotFound => RuntimeError::NotFound,
        PhyError::ModeConflict | PhyError::Unbalanced => RuntimeError::Invalid,
        PhyError::NotSupported => RuntimeError::Unsupported,
        PhyError::Io => RuntimeError::Io,
    }
}

/// Device-model view of the kphy provider registry.
pub struct PhyRegistry {
    wake: Box<dyn Fn(DepSource) + Send + Sync>,
    /// Phandles already served; a duplicate publication is refused.
    registered: SpinNoPreempt<BTreeSet<u64>>,
}

impl PhyRegistry {
    pub fn new(wake: impl Fn(DepSource) + Send + Sync + 'static) -> Self {
        Self {
            wake: Box::new(wake),
            registered: SpinNoPreempt::new(BTreeSet::new()),
        }
    }

    /// Publish a phy (`of_phy_provider_register` analogue) and wake every
    /// probe parked on it.
    pub fn register_provider(&self, provider: &'static dyn PhyLookup) -> Result<(), RuntimeError> {
        let provider_id = provider.provider_id();
        if !self.registered.lock().insert(u64::from(provider_id)) {
            log::warn!("driver-host: phy provider {provider_id:#x} registered twice");
            return Err(RuntimeError::AlreadyExists);
        }
        kphy::register_provider(provider);
        log::debug!("driver-host: phy provider {provider_id:#x} registered");
        (self.wake)(DepSource::phy(u64::from(provider_id)));
        Ok(())
    }

    /// Acquire a phy input, or report the dependency we are waiting for.
    pub fn get(&self, spec: PhySpec) -> Result<PhyHandle, ServiceError> {
        let provider_id = u32::try_from(spec.provider).map_err(|_| RuntimeError::Invalid)?;
        let kspec = kphy::PhySpecifier {
            cells: spec.cells,
            ncells: spec.ncells,
        };
        match kphy::phy_get(kspec, provider_id) {
            Ok(handle) => Ok(PhyHandle::new(Arc::new(KphyHandle(handle)), None)),
            Err(PhyError::NoProvider) => Err(ServiceError::Deferred(DepSource::phy(spec.provider))),
            Err(err) => Err(ServiceError::Fatal(map_phy_error(err))),
        }
    }
}
