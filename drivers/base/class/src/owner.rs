// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Live identity supplied by the device model or the legacy publisher bridge.

use alloc::{string::String, sync::Arc};
use core::sync::atomic::{AtomicBool, Ordering};

/// Read-only provider identity behind a publication.
///
/// This describes the device supplying the capability, not the Registration
/// that controls publication. Only the model may change device liveness.
///
/// The model retains ownership of the state and must reuse the same `Arc`
/// allocation for all publications of one device. Class registries compare
/// allocation identity, not paths or numeric identifiers, for duplicate detection.
/// Methods run in task context. Online queries may run under the class registry
/// lock: they must be non-blocking and must not reenter the class. The path must
/// remain stable for this identity.
pub trait PublicationSource: Send + Sync + 'static {
    /// Driver currently claiming the device; `None` prevents publication.
    fn driver_name(&self) -> Option<&'static str>;

    /// Stable physical path used by class projections.
    fn physical_path(&self) -> &str;

    /// Whether the device still backs its published capabilities.
    fn is_online(&self) -> bool;
}

pub(crate) struct LegacyOwner {
    driver_name: &'static str,
    physical_path: String,
    offline: AtomicBool,
}

impl LegacyOwner {
    #[cfg(unittest)]
    pub(crate) fn mark_offline(&self) {
        self.offline.store(true, Ordering::Release);
    }

    pub(crate) fn new(driver_name: &'static str, physical_path: String) -> Arc<Self> {
        Arc::new(Self {
            driver_name,
            physical_path,
            offline: AtomicBool::new(false),
        })
    }
}

impl PublicationSource for LegacyOwner {
    fn driver_name(&self) -> Option<&'static str> {
        Some(self.driver_name)
    }

    fn physical_path(&self) -> &str {
        &self.physical_path
    }

    fn is_online(&self) -> bool {
        !self.offline.load(Ordering::Acquire)
    }
}
