// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Driver contracts: id tables plus probe/remove with typed private data.

use crate::{bus::Bus, device::DeviceAccess, error::ProbeError};

/// Immutable registration metadata, independent of driver ops and resources.
pub(crate) struct DriverIdentity {
    pub(crate) name: &'static str,
}

/// Driver operations with a typed private-data association.
///
/// `Data` is the Linux `struct foo_priv`: allocated in probe, owned by the
/// framework for the lifetime of the binding, passed back mutably in
/// `remove`, and dropped (releasing every guard it holds) after `remove`
/// returns.
///
/// Probe, remove, and destruction of successfully bound `Data` run in sleepable
/// task context under this device's management lock, without the domain state
/// lock. Same-device management reentry is rejected. Callbacks and destructors
/// must restore their execution context and must not wait for tasks that need
/// this device's management lock, including tasks entering `with_data`.
/// Cross-device lock ordering remains the bus/controller's responsibility.
pub trait DriverOps<B: Bus>: Send + Sync + 'static {
    /// Driver-private data; must be `Send + Sync` because interrupt handlers
    /// and work items share it through `Weak` handles.
    type Data: Send + Sync + 'static;

    /// Initialize the device. Store every acquired guard in the returned
    /// `Data`. Before returning `Err`, explicitly stop active operations and
    /// unregister published capabilities; only inactive guards may rely on RAII.
    /// The framework cannot call remove for Data that probe did not return.
    /// `dev` accesses this device; `env` supplies host-wide framework services.
    fn probe(
        &self,
        dev: &mut DeviceAccess<'_, B>,
        env: &crate::Environment<'_>,
    ) -> Result<Self::Data, ProbeError>;

    /// Quiesce the device. Guards inside `Data` are still alive here and are
    /// released after this returns; `dev` accesses the same device as probe.
    /// `env` supplies the same host service implementation used during probe.
    /// Unregister marks the device source offline before this callback;
    /// unbind preserves registration. The driver must close its published
    /// operations explicitly rather than treating device identity as a gate.
    /// Stop and join ongoing operations before releasing their independent
    /// resources. A publication credential's Drop is not a substitute for
    /// explicit synchronous unregistration.
    ///
    /// Linux shape: `remove(pdev)` then `dev_get_drvdata(pdev)`. Here the
    /// drvdata is passed typed and mutably as the third argument instead of
    /// recovered through an untyped accessor, so no downcast (and no panic
    /// path) exists in the driver-facing surface.
    fn remove(
        &self,
        dev: &mut DeviceAccess<'_, B>,
        _env: &crate::Environment<'_>,
        data: &mut Self::Data,
    ) {
        let _ = (dev, data);
    }
}

/// A driver registration record: everything is data except the ops.
///
/// The direct translation target of Linux's `struct platform_driver`:
/// `driver.name`, the `*_device_id` tables, and the function pointers.
pub struct DriverDesc<B: Bus, P: DriverOps<B>> {
    pub name: &'static str,
    /// Const id table (`of_device_id` / `pci_device_id` rows).
    pub ids: &'static [B::IdEntry],
    pub ops: P,
}

impl<B: Bus, P: DriverOps<B>> DriverDesc<B, P> {
    pub const fn new(name: &'static str, ids: &'static [B::IdEntry], ops: P) -> Self {
        Self { name, ids, ops }
    }
}

/// Declare a driver and its registration entry point (Linux
/// `module_platform_driver()` / `module_pci_driver()` / `module_init()`).
///
/// The three facts a transcribed driver supplies are exactly Linux's: the
/// bus it binds to, its id table, and its ops type. Everything else — which
/// domain receives the registration, whether that domain is global — stays
/// on the framework side, so `drv/**` holds no framework call and a domain
/// change (host root, test domain) rewrites this macro once instead of every
/// driver.
///
/// ```ignore
/// static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("arm,pl011"))];
/// driver_model::declare_driver!(PlatformBus, Pl011Driver, "pl011-serial", IDS);
///
/// // A family with two entry points names the generated function:
/// driver_model::declare_driver!(PlatformBus, Ns16550MmioDriver, "ns16550-mmio", MMIO_IDS => register_mmio);
/// ```
#[macro_export]
macro_rules! declare_driver {
    ($bus:ty, $driver:ident, $name:literal, $ids:expr) => {
        $crate::declare_driver!($bus, $driver, $name, $ids => register);
    };
    ($bus:ty, $driver:ident, $name:literal, $ids:expr => $register:ident) => {
        /// Registration entry point called by the composition root.
        /// Returns whether the registration was accepted (`false`: a
        /// driver of the same name is already registered on this bus).
        pub fn $register() -> bool {
            let desc: $crate::DriverDesc<$bus, $driver> =
                $crate::DriverDesc::new($name, $ids, $driver);
            $crate::global_domain().register_driver(desc)
        }
    };
}
