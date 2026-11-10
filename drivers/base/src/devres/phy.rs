// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The phy family: an acquired phy input with counted ops and the
//! probe-time powered guard.
//!
//! The counts live in the provider's shared instance (the phy core);
//! these handles forward onto it — two handles over one instance are two
//! consumers, and the hardware sequences run exactly on the refcount
//! edges.

use alloc::sync::Arc;
use core::fmt;

use super::ReleaseHook;
use crate::{devres::host_api::PhyOps, error::RuntimeError};

/// An acquired phy input (`struct phy *`).
pub struct PhyHandle {
    ops: Arc<dyn PhyOps>,
    release: Option<ReleaseHook>,
}

impl PhyHandle {
    /// Constructed by [`DeviceServices`] implementations.
    pub fn new(ops: Arc<dyn PhyOps>, release: Option<ReleaseHook>) -> Self {
        Self { ops, release }
    }

    /// `phy_init` (counted; hardware setup on the first acquire).
    pub fn init(&self) -> Result<(), RuntimeError> {
        self.ops.init()
    }

    /// `phy_exit` (counted; teardown on the last release).
    pub fn exit(&self) -> Result<(), RuntimeError> {
        self.ops.exit()
    }

    /// `phy_power_on` (counted).
    pub fn power_on(&self) -> Result<(), RuntimeError> {
        self.ops.power_on()
    }

    /// `phy_power_off` (counted).
    pub fn power_off(&self) {
        self.ops.power_off();
    }

    /// `phy_set_mode` (provider-local semantics).
    pub fn set_mode(&self, mode: u32) -> Result<(), RuntimeError> {
        self.ops.set_mode(mode)
    }

    /// The probe-time shape: init + power_on now; the guard's drop runs
    /// power_off + exit.
    pub fn init_powered(&self) -> Result<PoweredPhy, RuntimeError> {
        self.ops.init()?;
        if let Err(err) = self.ops.power_on() {
            let _ = self.ops.exit();
            return Err(err);
        }
        Ok(PoweredPhy {
            ops: self.ops.clone(),
        })
    }
}

impl fmt::Debug for PhyHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PhyHandle").finish_non_exhaustive()
    }
}

impl Drop for PhyHandle {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// Guard owning one counted init+power acquisition; drop = power_off +
/// exit (full teardown, per the slice's aligned semantics).
pub struct PoweredPhy {
    ops: Arc<dyn PhyOps>,
}

impl PoweredPhy {
    /// The wrapped phy (repeated explicit ops on the same counts).
    pub fn handle(&self) -> PhyHandle {
        PhyHandle {
            ops: self.ops.clone(),
            release: None,
        }
    }
}

impl Drop for PoweredPhy {
    fn drop(&mut self) {
        self.ops.power_off();
        if let Err(err) = self.ops.exit() {
            log::warn!("phy: PoweredPhy drop exit failed: {err}");
        }
    }
}
