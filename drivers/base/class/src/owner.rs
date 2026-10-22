// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Live identity supplied by the device model or the legacy publisher bridge.

use alloc::{string::String, sync::Arc};
use core::sync::atomic::{AtomicBool, Ordering};

/// Device-model contract for a publication's shared live identity.
///
/// The model retains ownership of the state and must reuse the same `Arc`
/// allocation for all publications of one device. Class registries compare
/// allocation identity, not paths or numeric identifiers, for duplicate detection.
/// Methods run outside the class registry lock in task context; they must not
/// reenter publication withdrawal. The path must remain stable for this identity.
pub trait PublicationOwner: Send + Sync + 'static {
    /// Driver currently claiming the device; `None` prevents publication.
    fn driver_name(&self) -> Option<&'static str>;

    /// Stable physical path used by class projections.
    fn physical_path(&self) -> &str;

    /// Whether the device still backs its published capabilities.
    fn is_online(&self) -> bool;

    /// Permanently retire this identity after a publication is withdrawn.
    ///
    /// This idempotent model callback preserves the shared-device retirement
    /// protocol: all publications observe the same offline state. It must not
    /// free runtime payloads; class teardown waits for its own in-flight users.
    fn mark_offline(&self);
}

pub(crate) struct LegacyOwner {
    driver_name: &'static str,
    physical_path: String,
    offline: AtomicBool,
}

impl LegacyOwner {
    pub(crate) fn new(driver_name: &'static str, physical_path: String) -> Arc<Self> {
        Arc::new(Self {
            driver_name,
            physical_path,
            offline: AtomicBool::new(false),
        })
    }
}

impl PublicationOwner for LegacyOwner {
    fn driver_name(&self) -> Option<&'static str> {
        Some(self.driver_name)
    }

    fn physical_path(&self) -> &str {
        &self.physical_path
    }

    fn is_online(&self) -> bool {
        !self.offline.load(Ordering::Acquire)
    }

    fn mark_offline(&self) {
        self.offline.store(true, Ordering::Release);
    }
}
