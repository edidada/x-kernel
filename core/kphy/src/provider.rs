// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Global phy provider registry — the kreset pattern (provider list keyed
//! by DT phandle), with an acquire hook so providers can bind per-specifier
//! state (the combphy's mode-from-specifier, conflict-rejected) before any
//! consumer handle exists.

use alloc::{collections::BTreeMap, sync::Arc};

use kspin::SpinNoIrq;

use crate::{hw::PhyError, instance::PhyInstance};

/// Identifier of a registered provider — the controller node's DT phandle.
pub type ProviderId = u32;

/// The raw specifier cells a DT `phys = <&phy ...>` entry carries
/// (`#phy-cells` 0 or 1 on every current RK3588 phy; captured verbatim,
/// provider-interpreted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhySpecifier {
    pub cells: [u32; 2],
    pub ncells: u8,
}

impl PhySpecifier {
    /// The first specifier cell (the combphy's mode), or 0 when absent.
    pub const fn first(&self) -> u32 {
        self.cells[0]
    }
}

/// Resolves a consumer specifier to a registered (possibly shared)
/// instance. Called once per acquisition; providers bind specifier state
/// here and may reject conflicts.
pub trait PhyLookup: Send + Sync {
    /// The provider id used to register this provider (its DT phandle).
    fn provider_id(&self) -> ProviderId;

    /// Acquire the instance for this specifier.
    fn acquire(&self, spec: PhySpecifier) -> Result<Arc<PhyInstance>, PhyError>;
}

/// The provider list, keyed by [`ProviderId`].
struct Registry {
    providers: BTreeMap<ProviderId, &'static dyn PhyLookup>,
}

static REGISTRY: SpinNoIrq<Registry> = SpinNoIrq::new(Registry {
    providers: BTreeMap::new(),
});

/// Register a phy provider under its [`ProviderId`] (boot-static).
pub fn register_provider(provider: &'static dyn PhyLookup) {
    REGISTRY
        .lock()
        .providers
        .insert(provider.provider_id(), provider);
}

#[cfg(unittest)]
pub fn unregister_provider(id: ProviderId) {
    REGISTRY.lock().providers.remove(&id);
}

/// Whether any provider is registered (test hygiene helper).
#[cfg(unittest)]
pub fn provider_installed() -> bool {
    !REGISTRY.lock().providers.is_empty()
}

/// Resolve a DT `phys = <&phy ...>` specifier to a consumer handle.
pub fn phy_get(spec: PhySpecifier, provider: ProviderId) -> Result<crate::PhyHandle, PhyError> {
    let lookup = REGISTRY
        .lock()
        .providers
        .get(&provider)
        .copied()
        .ok_or(PhyError::NoProvider)?;
    let instance = lookup.acquire(spec)?;
    Ok(crate::PhyHandle::new(instance))
}
