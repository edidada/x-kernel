// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The clock family: an acquired clock input and its prepare+enable guard.

use alloc::sync::Arc;
use core::fmt;

use super::ReleaseHook;
use crate::{devres::provider::ClkOps, error::RuntimeError};

/// An acquired clock input (`struct clk *`); drops the reference on drop.
///
/// `prepare`/`enable` are not undone automatically: quiesce belongs to
/// `remove()`, mirroring Linux ownership rules.
pub struct ClkHandle {
    ops: Arc<dyn ClkOps>,
    release: Option<ReleaseHook>,
}

impl ClkHandle {
    /// Constructed by [`DeviceServices`] implementations.
    pub fn new(ops: Arc<dyn ClkOps>, release: Option<ReleaseHook>) -> Self {
        Self { ops, release }
    }

    /// Prepare and enable, returning an ownership token for the ENABLED
    /// state. The token's Drop balances the prepare/enable pair — the
    /// review's finding was that `&self` + a bare `Ok(())` left probe-failure
    /// paths (request_irq/class publication errors AFTER a successful
    /// enable) with no rollback; remove-time cleanup relied on reading the
    /// provider's current state instead of local ownership.
    pub fn prepare_enable(&self) -> Result<EnabledClock, RuntimeError> {
        self.ops.prepare()?;
        if let Err(err) = self.ops.enable() {
            // Roll back the prepare: a failed enable must not leave the
            // clock half-prepared.
            let () = self.ops.disable();
            self.ops.unprepare();
            return Err(err);
        }
        Ok(EnabledClock {
            ops: Arc::clone(&self.ops),
        })
    }

    pub fn ops(&self) -> &dyn ClkOps {
        self.ops.as_ref()
    }
}

/// Ownership of one prepare+enable pair on a [`ClkHandle`].
///
/// Drop balances the pair (disable + unprepare). [`Self::keep_running`]
/// consumes the token WITHOUT balancing — for consumers that deliberately
/// hold the clock past device removal (the adopted stdout port's console
/// continuity policy). Every other path lets the token drop: probe failure
/// rolls back automatically, `remove` drops it from `Data`.
///
/// The token OWNS a clone of the handle's operations arc (not a borrow):
/// it is `'static` and can live in driver `Data`, so the balance is
/// structural — no `remove`-time "is it still enabled?" re-reading of
/// provider state.
pub struct EnabledClock {
    ops: Arc<dyn ClkOps>,
}

impl EnabledClock {
    /// Consume the token WITHOUT disabling: the caller takes explicit,
    /// permanent ownership of the enabled state. Use only when the clock
    /// must outlive the device binding (stdout console).
    pub fn keep_running(self) {
        core::mem::forget(self)
    }
}

impl Drop for EnabledClock {
    fn drop(&mut self) {
        self.ops.disable();
        self.ops.unprepare();
    }
}

impl core::ops::Deref for ClkHandle {
    type Target = dyn ClkOps;

    fn deref(&self) -> &Self::Target {
        self.ops.as_ref()
    }
}

impl Drop for ClkHandle {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

impl fmt::Debug for ClkHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClkHandle")
            .field("name", &self.ops.name())
            .field("rate", &self.ops.get_rate())
            .finish_non_exhaustive()
    }
}
