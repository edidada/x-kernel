// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Consumer handles over a (possibly shared) phy instance.
//!
//! Explicit ops mirror Linux `phy_init/exit/power_on/power_off/set_mode`
//! one-to-one (counted on the instance). [`PhyHandle::init_powered`]
//! returns the probe-time guard: init + power_on now, power_off + exit on
//! drop.

use alloc::sync::Arc;

use crate::{
    hw::{PhyError, PhyMode},
    instance::PhyInstance,
};

/// One consumer's handle over a shared instance. Cheap to clone; the
/// counts live on the instance.
#[derive(Clone)]
pub struct PhyHandle {
    instance: Arc<PhyInstance>,
}

impl PhyHandle {
    pub(crate) fn new(instance: Arc<PhyInstance>) -> Self {
        Self { instance }
    }

    /// `phy_init` (counted; hardware setup on the first acquire).
    pub fn init(&self) -> Result<(), PhyError> {
        self.instance.init()
    }

    /// `phy_exit` (counted; hardware teardown on the last release).
    pub fn exit(&self) -> Result<(), PhyError> {
        self.instance.exit()
    }

    /// `phy_power_on` (counted).
    pub fn power_on(&self) -> Result<(), PhyError> {
        self.instance.power_on()
    }

    /// `phy_power_off` (counted).
    pub fn power_off(&self) -> Result<(), PhyError> {
        self.instance.power_off()
    }

    /// `phy_set_mode` (provider-local semantics).
    pub fn set_mode(&self, mode: PhyMode) -> Result<(), PhyError> {
        self.instance.set_mode(mode)
    }

    /// The probe-time shape: `init` + `power_on` now; the guard's drop
    /// runs `power_off` + `exit` (the consumer chose full teardown).
    pub fn init_powered(&self) -> Result<PoweredPhy, PhyError> {
        self.instance.init()?;
        if let Err(err) = self.instance.power_on() {
            // Roll the init back so the instance is left unacquired.
            let _ = self.instance.exit();
            return Err(err);
        }
        Ok(PoweredPhy {
            instance: self.instance.clone(),
        })
    }
}

/// Guard owning one counted init+power acquisition; drop = power_off +
/// exit (full teardown, per the slice's aligned semantics).
pub struct PoweredPhy {
    instance: Arc<PhyInstance>,
}

impl PoweredPhy {
    /// The wrapped instance (repeated explicit ops on the same counts).
    pub fn handle(&self) -> PhyHandle {
        PhyHandle {
            instance: self.instance.clone(),
        }
    }
}

impl Drop for PoweredPhy {
    fn drop(&mut self) {
        let _ = self.instance.power_off();
        if let Err(err) = self.instance.exit() {
            log::warn!("kphy: PoweredPhy drop exit failed: {err:?}");
        }
    }
}
