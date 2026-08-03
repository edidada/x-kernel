// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Global clock-provider registry, keyed by DT phandle.
//!
//! Unlike `device-res`'s single MMIO/DMA/IRQ provider, a clock tree has many
//! producers, so this is a small lock-guarded table rather than a `Once`. A
//! linear scan is fine: there are only a handful of clock-provider nodes per
//! board and lookups happen at probe time, never on a data hot path.

use alloc::{sync::Arc, vec::Vec};

use kspin::SpinNoIrq;

use crate::ClkProvider;

static REGISTRY: SpinNoIrq<Vec<(u32, Arc<dyn ClkProvider>)>> = SpinNoIrq::new(Vec::new());

/// Register a clock provider under its DT phandle.
///
/// Called by a provider's probe (SoC CRU driver, SCMI client, ...) once it can
/// answer rate queries. If `phandle` is already registered, the new provider
/// shadows the old one (lookups scan front-to-back), mirroring Linux's
/// last-writer behaviour for `of_clk_add_hw_provider`.
pub fn register_provider(phandle: u32, provider: Arc<dyn ClkProvider>) {
    let mut registry = REGISTRY.lock();
    if let Some(pos) = registry.iter().position(|(p, _)| *p == phandle) {
        registry.remove(pos);
    }
    registry.insert(0, (phandle, provider));
}

/// Look up the provider registered under `phandle`, if any.
pub(crate) fn try_provider(phandle: u32) -> Option<Arc<dyn ClkProvider>> {
    REGISTRY
        .lock()
        .iter()
        .find(|(p, _)| *p == phandle)
        .map(|(_, provider)| provider.clone())
}

/// Drop every registered provider.
///
/// Test-only: the kernel never unregisters clock providers — they describe
/// hardware that exists for the whole boot.
#[cfg(unittest)]
pub(crate) fn reset_registry() {
    REGISTRY.lock().clear();
}
