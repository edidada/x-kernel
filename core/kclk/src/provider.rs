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

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

use kspin::{SpinNoPreempt, static_lock};

use crate::{ClkCore, ClkError, ClkHandle, ClkIndex, ClkResult};

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

    /// Resolve a clock by its **SoC name** — the provider output name registered
    /// via [`ClkTable::register`](crate::ClkTable::register) (e.g. `"uart0_clk"`).
    ///
    /// This is not a DT `clock-names` connection name (those are consumer-local
    /// and resolved by the platform to `get_by_index`); SoC clock names are
    /// globally unique across the SoC. The registry scans providers in order;
    /// the first that claims the name wins. Returning `None` lets it try the
    /// next provider.
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

    /// Only called from the test-only `unregister_provider`.
    #[cfg(unittest)]
    fn unregister(&mut self, id: ProviderId) {
        self.providers.remove(&id);
    }

    /// The provider registered under `id`, if any. Returns the `&'static`
    /// reference (which outlives the lock guard) so the caller can drop the
    /// registry lock before calling the open `ClkLookup` adapter.
    fn get_provider(&self, id: ProviderId) -> Option<&'static dyn ClkLookup> {
        self.providers.get(&id).copied()
    }

    /// Snapshot all providers as `&'static` references, for the same
    /// lock-then-release pattern.
    fn provider_snapshot(&self) -> Vec<&'static dyn ClkLookup> {
        self.providers.values().copied().collect()
    }
}

static_lock! {
    static REGISTRY: SpinNoPreempt<Registry> = SpinNoPreempt::new(Registry::new());
}

/// Register a clock provider under its [`ProviderId`].
///
/// Called during platform/controller init. A re-registration of an existing id
/// replaces the previous provider, which is the expected behavior when a
/// controller is re-probed. Clocks themselves are never freed (boot-static,
/// matching Linux). Providers cannot be unregistered at runtime (boot-static);
/// only `--cfg unittest` builds may remove them.
pub fn register_provider(provider: &'static dyn ClkLookup) {
    REGISTRY.lock().register(provider);
}

/// Remove a previously-registered provider from the lookup table.
///
/// **Only available under `--cfg unittest`.** Clock controllers are boot-static:
/// `CLOCKS` permanently holds every registered clock, consumers may already hold
/// handles into the tree, and an unregistered provider's MMIO mapping may still
/// be live in those nodes — there is no coherent runtime-removal semantics
/// (revocation, dependency tracking, mapping lifetime). Rather than keep a
/// seemingly-usable but incomplete API, dynamic remove is not offered in
/// production builds, matching the "clocks live forever" boot-static model.
#[cfg(unittest)]
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
///
/// Returns [`ClkError::NoProvider`] when `provider_id` is not yet registered
/// (a consumer probing before its controller should defer/retry) and
/// [`ClkError::NotFound`] when the provider exists but the index is invalid
/// (a permanent misconfiguration).
pub fn clk_get_by_index(provider_id: ProviderId, index: ClkIndex) -> ClkResult<ClkHandle> {
    // Snapshot the provider ref under the lock, then release before calling
    // the open ClkLookup adapter — the adapter may be slow or (one day) sleep,
    // and holding the IRQ-disabled registry lock across it is wrong.
    let provider = REGISTRY.lock().get_provider(provider_id);
    match provider {
        Some(p) => p
            .get_by_index(index)
            .map(ClkHandle::new)
            .ok_or(ClkError::NotFound),
        None => Err(ClkError::NoProvider),
    }
}

/// Resolve a clock's core node by its globally-unique SoC name.
///
/// Internal variant of [`clk_get_by_name`] for cross-provider parent
/// resolution during table registration (Linux resolves branch parent names
/// through the global clock lookup at registration time). Returns
/// [`ClkError::NotFound`] when no registered provider carries the name.
pub(crate) fn clk_get_core_by_name(name: &str) -> ClkResult<Arc<ClkCore>> {
    let providers = REGISTRY.lock().provider_snapshot();
    providers
        .iter()
        .find_map(|p| p.get_by_name(name))
        .ok_or(ClkError::NotFound)
}

/// Resolve a clock by its globally-unique SoC name into a consumer handle.
///
/// `name` is the clock's provider output name (e.g. `"uart0_clk"`), **not** a
/// DT `clock-names` connection name; the platform resolves connection names to
/// `(provider, index)` and uses [`clk_get_by_index`] instead. Returns
/// [`ClkError::NoProvider`] when no provider is registered at all (defer) and
/// [`ClkError::NotFound`] when providers exist but the name is unknown.
pub fn clk_get_by_name(name: &str) -> ClkResult<ClkHandle> {
    // Snapshot providers under the lock, release, then scan — same rationale
    // as clk_get_by_index.
    let providers = REGISTRY.lock().provider_snapshot();
    if providers.is_empty() {
        return Err(ClkError::NoProvider);
    }
    providers
        .iter()
        .find_map(|p| p.get_by_name(name))
        .map(ClkHandle::new)
        .ok_or(ClkError::NotFound)
}
