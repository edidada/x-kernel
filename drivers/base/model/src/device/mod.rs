// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Registered device ownership, enumeration descriptions and scoped access.
//!
//! The descriptor is built before registration and then owned by DeviceCore.
//! DeviceRef retains that object; DeviceAccess and its views borrow it for callbacks.

mod access;
mod descriptor;
mod source;

use alloc::sync::Arc;

pub use access::DeviceAccess;
pub use descriptor::DeviceDescriptor;
pub use source::PublicationSource;

use crate::domain::DeviceId;

/// The device's live identity card — the shared, domain-owned side of a
/// registered device (Linux's refcounted `struct device` the class layer
/// points at). Created by the domain at `add_device`; class publications
/// and consumers hold `Arc`s and read facts LIVE instead of carrying
/// value snapshots that drift.
///
/// Owns the immutable enumeration descriptor and shared publication-source
/// state. The driver name records the current probe/binding and is cleared
/// on failure or teardown. Runtime resources remain owned by the domain.
/// References retain metadata, not hardware availability or operation permits.
pub(crate) struct DeviceCore {
    pub(crate) id: DeviceId,
    pub(crate) desc: crate::DeviceDescriptor,
    /// Per-device lifecycle serialization, retained even by stale references.
    pub(crate) management: Arc<dyn crate::DeviceManagementLock>,
    /// Membership token for rejecting foreign domains after registry removal.
    pub(crate) domain: Arc<()>,
    /// Set by the domain when a driver's probe starts (the claim is known
    /// then, not at bind-commit).
    driver: kspin::SpinNoPreempt<Option<Arc<crate::driver::DriverIdentity>>>,
    /// The bus-vocabulary path component, derived once at `add_device`.
    physical_path: alloc::string::String,
    /// Flipped by the removal orchestration (before quiesce and before the
    /// class publications withdraw). Every publication of this device —
    /// and any consumer holding a clone — observes it.
    offline: core::sync::atomic::AtomicBool,
}

impl DeviceCore {
    /// The driver that claimed this device, once probing started.
    pub(crate) fn driver_name(&self) -> Option<&'static str> {
        self.driver.lock().as_ref().map(|driver| driver.name)
    }

    /// Record the claiming driver. Domain-internal.
    pub(crate) fn bind_driver(&self, identity: Arc<crate::driver::DriverIdentity>) {
        *self.driver.lock() = Some(identity);
    }

    pub(crate) fn clear_driver(&self) {
        *self.driver.lock() = None;
    }

    /// The physical path (DEVPATH source), as derived from the bus data.
    pub(crate) fn physical_path(&self) -> &str {
        self.physical_path.as_str()
    }

    /// Whether the device is still standing behind its publications.
    pub(crate) fn is_online(&self) -> bool {
        !self.offline.load(core::sync::atomic::Ordering::Acquire)
    }

    /// Mark the device gone. Removal-orchestration internal.
    pub(crate) fn mark_offline(&self) {
        self.offline
            .store(true, core::sync::atomic::Ordering::Release);
    }

    pub(crate) fn new(
        id: DeviceId,
        desc: crate::DeviceDescriptor,
        management: Arc<dyn crate::DeviceManagementLock>,
        domain: Arc<()>,
    ) -> alloc::sync::Arc<Self> {
        let physical_path = desc.bus_data.path_component();
        alloc::sync::Arc::new(Self {
            id,
            desc,
            management,
            domain,
            driver: kspin::SpinNoPreempt::new(None),
            physical_path,
            offline: core::sync::atomic::AtomicBool::new(false),
        })
    }
}

/// An owning reference to a registered device's identity and immutable facts.
///
/// Cloning this reference does not retain the binding or its resources. After
/// removal, metadata remains readable, but it grants no hardware access.
/// Dropping a reference never unregisters the device. Final metadata destruction
/// runs in the last owner's context; bus data must not require sleepable Drop.
#[derive(Clone)]
pub struct DeviceRef {
    pub(crate) core: Arc<DeviceCore>,
}

impl DeviceRef {
    /// Immutable enumeration facts, retained even after device removal.
    pub fn descriptor(&self) -> &crate::DeviceDescriptor {
        &self.core.desc
    }

    /// Physical location derived from the registered bus data.
    pub fn path(&self) -> &str {
        self.core.physical_path()
    }

    /// The current probe/binding driver, not proof of hardware availability.
    pub fn driver_name(&self) -> Option<&'static str> {
        self.core.driver_name()
    }

    /// Whether removal has not yet closed this device's publication source.
    /// This snapshot is neither a binding check nor an in-flight operation permit.
    pub fn is_online(&self) -> bool {
        self.core.is_online()
    }

    /// Whether both references designate the same registered object.
    pub fn same_device(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.core, &other.core)
    }
}

impl core::fmt::Debug for DeviceRef {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("DeviceRef")
            .field("registration_key", &self.core.id)
            .finish_non_exhaustive()
    }
}
