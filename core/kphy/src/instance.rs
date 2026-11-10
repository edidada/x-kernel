// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! One shared PHY instance: the hardware ops plus the per-consumer
//! reference counts (Linux `phy::init_count` / `power_count`).
//!
//! The RK3588 board genuinely shares: pcie3x4 and pcie3x2 both acquire
//! `phy@fee80000` — the hardware init sequence must run once, and the
//! PHY must power down only when the LAST consumer releases.

use alloc::sync::Arc;

use kspin::SpinNoIrq;

use crate::hw::{PhyError, PhyHwOps, PhyMode};

/// The counted instance behind every consumer handle.
pub struct PhyInstance {
    ops: Arc<dyn PhyHwOps>,
    state: SpinNoIrq<Counts>,
}

#[derive(Default)]
struct Counts {
    init: u32,
    power: u32,
}

impl PhyInstance {
    /// Bind hardware ops into a shared instance.
    pub fn new(ops: Arc<dyn PhyHwOps>) -> Arc<Self> {
        Arc::new(Self {
            ops,
            state: SpinNoIrq::new(Counts::default()),
        })
    }

    /// `phy_init`: run hardware init on the 0→1 edge; counted above it.
    pub fn init(&self) -> Result<(), PhyError> {
        let first = {
            let mut counts = self.state.lock();
            let first = counts.init == 0;
            counts.init += 1;
            first
        };
        if first && let Err(err) = self.ops.hw_init() {
            // Roll the count back: a failed init must not leave the
            // instance counted as initialized (Linux's enable-count lesson).
            self.state.lock().init -= 1;
            return Err(err);
        }
        Ok(())
    }

    /// `phy_exit`: hardware exit on the 1→0 edge.
    ///
    /// Unbalanced calls (nothing acquired) are rejected — a silent no-op
    /// would hide consumer bookkeeping bugs.
    pub fn exit(&self) -> Result<(), PhyError> {
        let last = {
            let mut counts = self.state.lock();
            if counts.init == 0 {
                return Err(PhyError::Unbalanced);
            }
            counts.init -= 1;
            counts.init == 0
        };
        if last {
            if self.state.lock().power != 0 {
                // Exiting while still powered is consumer bookkeeping
                // sloppiness that would strand the PHY powered: name it.
                log::warn!("kphy: exit while power_count > 0");
            }
            self.ops.hw_exit()?;
        }
        Ok(())
    }

    /// `phy_power_on`: run hardware power-on on the 0→1 edge.
    pub fn power_on(&self) -> Result<(), PhyError> {
        let first = {
            let mut counts = self.state.lock();
            let first = counts.power == 0;
            counts.power += 1;
            first
        };
        if first && let Err(err) = self.ops.hw_power_on() {
            self.state.lock().power -= 1;
            return Err(err);
        }
        Ok(())
    }

    /// `phy_power_off`: hardware power-off on the 1→0 edge.
    pub fn power_off(&self) -> Result<(), PhyError> {
        let last = {
            let mut counts = self.state.lock();
            if counts.power == 0 {
                return Err(PhyError::Unbalanced);
            }
            counts.power -= 1;
            counts.power == 0
        };
        if last {
            self.ops.hw_power_off();
        }
        Ok(())
    }

    /// `phy_set_mode`: forwarded to the provider (provider-local
    /// semantics; e.g. only legal before init on a combphy).
    pub fn set_mode(&self, mode: PhyMode) -> Result<(), PhyError> {
        self.ops.hw_set_mode(mode)
    }
}
