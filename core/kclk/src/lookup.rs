// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ready-made [`ClkLookup`] implementation for one clock controller.
//!
//! A clock controller publishes its outputs at fixed DT binding indices
//! (`clocks = <&ccu 5>` → index 5 within *this* controller) and, for consumer
//! convenience, by the clock's SoC name (its provider output name, e.g.
//! `"uart0_clk"` — not a DT `clock-names` connection name). [`ClkTable`]
//! stores both views behind a single spin lock and answers either query; a
//! controller driver fills it during probe, assigns it a [`ProviderId`] (its
//! DT phandle), and registers it via [`crate::register_provider`].

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

use kspin::SpinNoPreempt;

use crate::{ClkCore, ClkIndex, ClkLookup, ProviderId};

/// Name- and index-keyed table of the outputs of one clock controller.
///
/// `const`-constructible so it can live in a `static`:
///
/// ```
/// static CCU_TABLE: kclk::ClkTable = kclk::ClkTable::new(0x1);
/// ```
pub struct ClkTable {
    provider_id: ProviderId,
    by_index: SpinNoPreempt<Vec<Option<Arc<ClkCore>>>>,
    by_name: SpinNoPreempt<BTreeMap<&'static str, Arc<ClkCore>>>,
}

impl ClkTable {
    /// Create an empty table bound to controller `provider_id`.
    ///
    /// `provider_id` is normally the DT phandle of the controller node, so a
    /// consumer `clocks = <&ccu 5>` specifier resolves here.
    pub const fn new(provider_id: ProviderId) -> Self {
        Self {
            provider_id,
            by_index: SpinNoPreempt::new(Vec::new()),
            by_name: SpinNoPreempt::new(BTreeMap::new()),
        }
    }

    /// Register `clk` under DT binding `index` and SoC `name` (the clock's
    /// provider output name, e.g. `"uart0_clk"`; not a DT `clock-names`
    /// connection name).
    ///
    /// Re-registering an existing index/name replaces the previous node, which
    /// is the expected behavior when a controller is re-probed.
    pub fn register(&self, index: ClkIndex, name: &'static str, clk: Arc<ClkCore>) {
        let mut idx = self.by_index.lock();
        let need = index as usize + 1;
        if idx.len() < need {
            idx.resize(need, None);
        }
        idx[index as usize] = Some(clk.clone());
        drop(idx);
        self.by_name.lock().insert(name, clk);
    }

    /// Remove a previously registered clock from both views.
    ///
    /// Removing a clock only drops the table's reference; a consumer still
    /// holding its handle keeps the underlying node alive (reference counting).
    pub fn unregister(&self, index: ClkIndex, name: &'static str) {
        if let Some(slot) = self.by_index.lock().get_mut(index as usize) {
            *slot = None;
        }
        self.by_name.lock().remove(name);
    }

    /// Remove all entries. Only available under `--cfg unittest` so each test
    /// starts from a known-empty registry.
    #[cfg(unittest)]
    pub fn clear(&self) {
        self.by_index.lock().clear();
        self.by_name.lock().clear();
    }

    /// Returns the clock registered under DT binding `index`, if any.
    pub fn get_by_index(&self, index: ClkIndex) -> Option<Arc<ClkCore>> {
        self.by_index
            .lock()
            .get(index as usize)
            .and_then(|slot| slot.clone())
    }

    /// Returns the clock registered under SoC output `name`, if any.
    pub fn get_by_name(&self, name: &str) -> Option<Arc<ClkCore>> {
        self.by_name.lock().get(name).cloned()
    }
}

impl ClkLookup for ClkTable {
    fn provider_id(&self) -> ProviderId {
        self.provider_id
    }

    fn get_by_name(&self, name: &str) -> Option<Arc<ClkCore>> {
        self.by_name.lock().get(name).cloned()
    }

    fn get_by_index(&self, index: ClkIndex) -> Option<Arc<ClkCore>> {
        self.by_index
            .lock()
            .get(index as usize)
            .and_then(Option::clone)
    }
}
