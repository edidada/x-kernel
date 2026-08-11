// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Global clock provider registry.
//!
//! A real SoC has several clock controllers (CCU, PMUCRU, video CCU, ...), each
//! publishing its own outputs. The registry therefore holds a *list* of
//! providers, each tagged with a [`ProviderId`] (normally the DT phandle of the
//! controller node). A consumer resolves a clock by `(provider_id, index)` —
//! the two cells of a DT `clocks = <&ccu 5>` specifier — or, for convenience, by
//! a globally-unique name.
//!
//! This mirrors Linux's `of_clk_providers` list: the phandle first selects the
//! provider, then the index is interpreted within that provider's local
//! namespace.

use alloc::{collections::BTreeMap, sync::Arc};

use kspin::SpinNoIrq;

use crate::{ClkCore, ClkHandle, ClkIndex};

/// Identifier of a registered provider.
///
/// In practice this is the DT phandle of the controller node, so a consumer
/// `clocks = <&ccu 5>` specifier maps to `(phandle_of_ccu, 5)`. We keep it an
/// opaque `u32` rather than coupling to a DT type so the registry stays usable
/// without firmware (static platform configurations, tests).
pub type ProviderId = u32;

/// Resolves a consumer clock specifier to a registered node.
///
/// A clock-controller driver implements this to expose its outputs. `index` is
/// provider-local: two controllers may both publish index 0 without colliding
/// because the registry keys on the provider id first.
pub trait ClkLookup: Send + Sync {
    /// The provider id used to register this provider (usually its DT phandle).
    fn provider_id(&self) -> ProviderId;

    /// Resolve a clock by the provider-relative index from a DT phandle spec.
    fn get_by_index(&self, index: ClkIndex) -> Option<Arc<ClkCore>>;

    /// Resolve a clock by firmware name (a DT `clock-names` entry).
    ///
    /// Name lookup is global across the SoC; names must therefore be unique
    /// across all providers that publish them. Returning `None` lets the
    /// registry try the next provider.
    fn get_by_name(&self, name: &str) -> Option<Arc<ClkCore>> {
        let _ = name;
        None
    }
}

/// The provider list, keyed by [`ProviderId`].
struct Registry {
    providers: BTreeMap<ProviderId, &'static dyn ClkLookup>,
}

impl Registry {
    const fn new() -> Self {
        Self {
            providers: BTreeMap::new(),
        }
    }

    fn register(&mut self, provider: &'static dyn ClkLookup) {
        self.providers.insert(provider.provider_id(), provider);
    }

    fn unregister(&mut self, id: ProviderId) {
        self.providers.remove(&id);
    }

    fn by_index(&self, id: ProviderId, index: ClkIndex) -> Option<Arc<ClkCore>> {
        self.providers.get(&id)?.get_by_index(index)
    }

    fn by_name(&self, name: &str) -> Option<Arc<ClkCore>> {
        // Names are globally unique; the first provider that claims it wins.
        self.providers.values().find_map(|p| p.get_by_name(name))
    }
}

static REGISTRY: SpinNoIrq<Registry> = SpinNoIrq::new(Registry::new());

/// Register a clock provider under its [`ProviderId`].
///
/// Called during platform/controller init. A re-registration of an existing id
/// replaces the previous provider, which is the expected behavior when a
/// controller is re-probed.
pub fn register_provider(provider: &'static dyn ClkLookup) {
    REGISTRY.lock().register(provider);
}

/// Remove a previously-registered provider. Mainly for `--cfg unittest` and for
/// controller removal.
pub fn unregister_provider(id: ProviderId) {
    REGISTRY.lock().unregister(id);
}

/// Whether any provider has been registered yet.
pub fn provider_installed() -> bool {
    !REGISTRY.lock().providers.is_empty()
}

/// Clear the whole registry. Only available under `--cfg unittest`.
#[cfg(unittest)]
pub fn reset_providers() {
    REGISTRY.lock().providers.clear();
}

/// Resolve a clock by `(provider_id, index)` into a consumer handle.
pub fn clk_get_by_index(provider_id: ProviderId, index: ClkIndex) -> Option<ClkHandle> {
    REGISTRY
        .lock()
        .by_index(provider_id, index)
        .map(ClkHandle::new)
}

/// Resolve a clock by globally-unique name into a consumer handle.
pub fn clk_get_by_name(name: &str) -> Option<ClkHandle> {
    REGISTRY.lock().by_name(name).map(ClkHandle::new)
}
