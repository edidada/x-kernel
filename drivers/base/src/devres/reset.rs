// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The reset family: an acquired reset line and its explicit ops.
//!
//! No refcount, no RAII state machine — Linux `reset_control_assert/
//! deassert/status` map one-to-one onto the ops; the one convenience RAII
//! shape (deassert-with-assert-on-drop) is the probe-time norm.

use alloc::sync::Arc;
use core::fmt;

use super::ReleaseHook;
use crate::devres::host_api::ResetOps;

/// An acquired reset line (`struct reset_control *`).
///
/// The line is boot-static provider state; the optional release hook
/// covers provider-side bookkeeping (test doubles, future refcounted
/// providers) and mirrors every other devres guard.
pub struct ResetHandle {
    ops: Arc<dyn ResetOps>,
    release: Option<ReleaseHook>,
}

impl ResetHandle {
    /// Constructed by [`DeviceServices`] implementations.
    pub fn new(ops: Arc<dyn ResetOps>, release: Option<ReleaseHook>) -> Self {
        Self { ops, release }
    }

    /// Assert the line (`reset_control_assert`): hold the IP in reset.
    pub fn assert(&self) {
        self.ops.assert();
    }

    /// Deassert the line (`reset_control_deassert`): release the IP out
    /// of reset.
    pub fn deassert(&self) {
        self.ops.deassert();
    }

    /// Sample the line (`reset_control_status`): `true` when asserted.
    pub fn status(&self) -> bool {
        self.ops.status()
    }

    /// Deassert and return a guard that re-asserts on drop — the
    /// probe-time shape for "release the IP for the binding's lifetime".
    pub fn deasserted(&self) -> DeassertedGuard {
        self.ops.deassert();
        DeassertedGuard {
            ops: self.ops.clone(),
        }
    }
}

impl fmt::Debug for ResetHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResetHandle")
            .field("asserted", &self.ops.status())
            .finish_non_exhaustive()
    }
}

impl Drop for ResetHandle {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// Guard that re-asserts its line on drop (the reverse of
/// `devm_reset_control_deassert`'s cleanup).
pub struct DeassertedGuard {
    ops: Arc<dyn ResetOps>,
}

impl DeassertedGuard {
    /// The wrapped line, for callers that need repeated toggles.
    pub fn handle(&self) -> ResetHandle {
        ResetHandle {
            ops: self.ops.clone(),
            release: None,
        }
    }
}

impl Drop for DeassertedGuard {
    fn drop(&mut self) {
        self.ops.assert();
    }
}
