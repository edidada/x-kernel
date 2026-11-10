// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Global reset provider registry — the [`kclk`] provider-list pattern,
//! stripped to reset semantics.
//!
//! A reset controller registers under a [`ProviderId`] (its DT phandle); a
//! consumer resolves `resets = <&cru 528>` to `(provider_id, 528)` — the
//! specifier index is provider-local, so controllers cannot collide.

use alloc::{collections::BTreeMap, sync::Arc};

use kspin::SpinNoIrq;

use crate::{ResetHandle, ResetLine};

/// Identifier of a registered provider — the controller node's DT phandle.
pub type ProviderId = u32;

/// The provider-local index a DT `resets = <&cru N>` specifier carries.
pub type ResetIndex = u32;

/// Resolves a consumer reset specifier to a registered line.
pub trait ResetLookup: Send + Sync {
    /// The provider id used to register this provider (its DT phandle).
    fn provider_id(&self) -> ProviderId;

    /// Resolve a line by the provider-relative index from a DT specifier.
    fn get_by_index(&self, index: ResetIndex) -> Option<Arc<ResetLine>>;
}

/// The provider list, keyed by [`ProviderId`].
struct Registry {
    providers: BTreeMap<ProviderId, &'static dyn ResetLookup>,
}

impl Registry {
    const fn new() -> Self {
        Self {
            providers: BTreeMap::new(),
        }
    }
}

static REGISTRY: SpinNoIrq<Registry> = SpinNoIrq::new(Registry::new());

/// Register a reset provider under its [`ProviderId`].
///
/// Boot-static: providers are never unregistered at runtime (matching
/// `of_reset_controller_register`); only `--cfg unittest` builds may
/// remove them.
pub fn register_provider(provider: &'static dyn ResetLookup) {
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

/// Resolve a DT `resets = <&cru N>` specifier to a consumer handle.
///
/// `Err(NoProvider)` when no controller is registered under `provider` —
/// the caller (the device-services bridge) turns that into a probe
/// deferral.
pub fn reset_get(provider: ProviderId, index: ResetIndex) -> Result<ResetHandle, ResetError> {
    let lookup = REGISTRY
        .lock()
        .providers
        .get(&provider)
        .copied()
        .ok_or(ResetError::NoProvider)?;
    let line = lookup.get_by_index(index).ok_or(ResetError::NotFound)?;
    Ok(ResetHandle::new(line))
}

/// Error vocabulary of the reset registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetError {
    /// No provider registered under the requested id.
    NoProvider,
    /// The provider is registered but has no line at that index.
    NotFound,
}
