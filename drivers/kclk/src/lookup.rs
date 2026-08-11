// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ready-made [`ClkLookup`] implementation for clock-controller providers.
//!
//! A clock controller publishes its outputs at fixed DT binding indices
//! (`clocks = <&ccu 5>` → index 5) and, for consumer convenience, by name
//! (a `clock-names` entry). [`ClkTable`] stores both views behind a single
//! spin lock and answers either query; a controller driver fills it during
//! probe and hands a `&'static` reference to [`crate::set_clk_provider`].

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

use kspin::SpinNoIrq;

use crate::{ClkCore, ClkIndex, ClkLookup};

/// Name- and index-keyed table of registered clock nodes.
///
/// `const`-constructible so it can live in a `static`:
///
/// ```
/// static CCU_TABLE: kclk::ClkTable = kclk::ClkTable::new();
/// ```
pub struct ClkTable {
    by_index: SpinNoIrq<Vec<Option<Arc<ClkCore>>>>,
    by_name: SpinNoIrq<BTreeMap<&'static str, Arc<ClkCore>>>,
}

impl ClkTable {
    /// Create an empty table. `const`, so usable in `static` initializers.
    pub const fn new() -> Self {
        Self {
            by_index: SpinNoIrq::new(Vec::new()),
            by_name: SpinNoIrq::new(BTreeMap::new()),
        }
    }

    /// Register `clk` under DT binding `index` and consumer `name`.
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

    /// Remove all entries. Only available under `--cfg unittest` so each test
    /// starts from a known-empty registry.
    #[cfg(unittest)]
    pub fn clear(&self) {
        self.by_index.lock().clear();
        self.by_name.lock().clear();
    }
}

impl Default for ClkTable {
    fn default() -> Self {
        Self::new()
    }
}

impl ClkLookup for ClkTable {
    fn get_by_name(&self, name: &str) -> Option<Arc<ClkCore>> {
        self.by_name.lock().get(name).cloned()
    }

    fn get_by_index(&self, index: ClkIndex) -> Option<Arc<ClkCore>> {
        self.by_index
            .lock()
            .get(index as usize)
            .and_then(|c| c.clone())
    }
}
