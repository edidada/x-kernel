// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Global clock provider registry.
//!
//! A platform installs a single [`ClkLookup`] during early init via
//! [`set_clk_provider`]; consumers then resolve clocks by name or index through
//! [`clk_get_by_name`] / [`clk_get_by_index`].
//!
//! This mirrors the [`device_res`] capability-provider pattern: production uses
//! [`klazy::Once`] for lock-free reads (a single acquire load after install),
//! while `--cfg unittest` swaps in a replaceable slot so tests can stand up a
//! mock provider even after the host kernel has installed the real one.

use alloc::sync::Arc;

#[cfg(not(unittest))]
use klazy::Once;

use crate::{ClkCore, ClkHandle};

/// Resolves a consumer clock specifier to a registered node.
///
/// A clock-controller driver implements this to expose its outputs — typically
/// backed by an onecell/simple lookup table indexed by the DT `clocks` cell.
pub trait ClkLookup: Send + Sync {
    /// Resolve a clock by firmware name (e.g. a DT `clock-names` entry).
    fn get_by_name(&self, name: &str) -> Option<Arc<ClkCore>>;

    /// Resolve a clock by the provider-relative index from a DT phandle spec.
    fn get_by_index(&self, index: crate::ClkIndex) -> Option<Arc<ClkCore>>;
}

// Production registry: a single install, lock-free reads thereafter.
#[cfg(not(unittest))]
static CLK_PROVIDER: Once<&'static dyn ClkLookup> = Once::new();

// Test registry: replaceable so each test can swap in a mock.
#[cfg(unittest)]
static CLK_PROVIDER: kspin::SpinNoIrq<Option<&'static dyn ClkLookup>> = kspin::SpinNoIrq::new(None);

/// Install the global clock provider. Must be called exactly once during early
/// kernel init, before any consumer resolves a clock.
#[cfg(not(unittest))]
pub fn set_clk_provider(provider: &'static dyn ClkLookup) {
    CLK_PROVIDER.call_once(|| provider);
}

/// Install the global clock provider (test variant, replaceable).
#[cfg(unittest)]
pub fn set_clk_provider(provider: &'static dyn ClkLookup) {
    *CLK_PROVIDER.lock() = Some(provider);
}

/// Uninstall the global clock provider.
///
/// Only available under `--cfg unittest`, where the registry is replaceable;
/// lets each test start from a known-empty registry.
#[cfg(unittest)]
pub fn reset_clk_provider() {
    *CLK_PROVIDER.lock() = None;
}

fn provider() -> Option<&'static dyn ClkLookup> {
    #[cfg(not(unittest))]
    {
        CLK_PROVIDER.get().copied()
    }
    #[cfg(unittest)]
    {
        *CLK_PROVIDER.lock()
    }
}

/// Peek at the installed provider without installing one.
///
/// Returns [`None`] before [`set_clk_provider`] has run.
pub fn try_clk_provider() -> Option<&'static dyn ClkLookup> {
    provider()
}

/// Resolve a clock by firmware name into a consumer handle.
pub fn clk_get_by_name(name: &str) -> Option<ClkHandle> {
    provider()?.get_by_name(name).map(ClkHandle::new)
}

/// Resolve a clock by provider-relative index into a consumer handle.
pub fn clk_get_by_index(index: crate::ClkIndex) -> Option<ClkHandle> {
    provider()?.get_by_index(index).map(ClkHandle::new)
}
