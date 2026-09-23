// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Clock provider registry with exact deferred-probe wakeups.

use alloc::{boxed::Box, collections::BTreeMap, sync::Arc};

use driver_core::{ClkHandle, ClkOps, DepSource, ServiceError};
use kspin::SpinNoPreempt;

/// Registry of clock providers (`struct clk *` producers), keyed by the DT
/// phandle that firmware uses to reference them.
///
/// A missing provider is reported as [`ServiceError::Deferred`] so the probe
/// parks instead of failing; when [`ClkRegistry::register_provider`] runs it
/// fires the wake hook, which re-probes exactly the parked consumers.
pub struct ClkRegistry {
    providers: SpinNoPreempt<BTreeMap<u64, Arc<dyn ClkOps>>>,
    wake: Box<dyn Fn(DepSource) + Send + Sync>,
}

impl ClkRegistry {
    pub fn new(wake: impl Fn(DepSource) + Send + Sync + 'static) -> Self {
        Self {
            providers: SpinNoPreempt::new(BTreeMap::new()),
            wake: Box::new(wake),
        }
    }

    /// Publish a provider and wake everything waiting on it.
    pub fn register_provider(&self, phandle: u64, ops: Arc<dyn ClkOps>) {
        self.providers.lock().insert(phandle, ops);
        log::debug!("driver-runtime: clk provider {phandle:#x} registered");
        (self.wake)(DepSource::clk(phandle));
    }

    /// Acquire a clock input, or report the dependency we are waiting for.
    pub fn get(&self, phandle: u64) -> Result<ClkHandle, ServiceError> {
        match self.providers.lock().get(&phandle).cloned() {
            Some(ops) => Ok(ClkHandle::new(ops, None)),
            None => Err(ServiceError::Deferred(DepSource::clk(phandle))),
        }
    }

    pub fn has_provider(&self, phandle: u64) -> bool {
        self.providers.lock().contains_key(&phandle)
    }
}
