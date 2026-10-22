// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Family-specific host extensions, without a per-extension downcast.
//!
//! A device family that needs machinery beyond the generic capabilities
//! (the virtio PCI transport is the one instance) provides a value whose type
//! is the contract. The host registers that value once at install time; a
//! driver asks for it by type:
//!
//! ```ignore
//! let Some(transport) = dev.env().extension::<driver_host::virtio_transport::PciTransportService>()
//! else { return Err(...) };
//! ```
//!
//! The type name in the driver IS the compile-time check (a wrong type simply
//! is not registered), and there is exactly one downcast for every extension —
//! here, in the framework — instead of one per extension inside the host.

use alloc::collections::BTreeMap;
use core::any::{Any, TypeId};

use kspin::SpinNoPreempt;

/// A type that device families may fetch through `Device::env().extension()`.
pub trait ExtensionService: Send + Sync + 'static {}

/// The host's extension table: one boot-static value per extension type.
pub struct ExtensionRegistry {
    entries: SpinNoPreempt<BTreeMap<TypeId, &'static (dyn Any + Send + Sync)>>,
}

impl ExtensionRegistry {
    /// An empty registry (hosts that provide no extensions).
    pub const fn new() -> Self {
        Self {
            entries: SpinNoPreempt::new(BTreeMap::new()),
        }
    }

    /// Register one extension value. Extensions are boot-static host state
    /// (registered once at install), so the value is a `&'static`.
    pub fn register<E: ExtensionService>(&self, extension: &'static E) {
        self.entries.lock().insert(
            TypeId::of::<E>(),
            extension as &'static (dyn Any + Send + Sync),
        );
    }

    /// Fetch a registered extension by type.
    pub fn get<E: ExtensionService>(&self) -> Option<&'static E> {
        self.entries
            .lock()
            .get(&TypeId::of::<E>())
            .and_then(|entry| entry.downcast_ref::<E>())
    }
}

impl Default for ExtensionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

static EMPTY_REGISTRY: ExtensionRegistry = ExtensionRegistry::new();

/// The shared empty registry (hosts without extensions, test doubles).
pub(crate) fn empty_registry() -> &'static ExtensionRegistry {
    &EMPTY_REGISTRY
}

/// The host capability behind `Device::env().extension()`.
pub trait ExtensionServices: Send + Sync {
    /// The host's extension table.
    fn extensions(&self) -> &ExtensionRegistry;
}
