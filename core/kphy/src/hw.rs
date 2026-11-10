// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! PHY hardware operations: what a provider driver implements for one
//! PHY instance.
//!
//! The framework ([`crate::PhyInstance`]) owns the refcount edges — these
//! ops run exactly on the 0→1 / 1→0 transitions, so a provider driver
//! never sees counted calls. Mode semantics are provider-local: the raw
//! mode value comes from the DT specifier or the consumer's `set_mode`.

/// Linux `enum phy_mode`-shaped raw mode value. The framework treats it
/// as opaque; providers interpret (e.g. the Rockchip combphy's DT
/// specifier cell uses the dt-bindings `PHY_TYPE_*` numbering).
pub type PhyMode = u32;

/// Provider hardware operations for one PHY instance.
pub trait PhyHwOps: Send + Sync {
    /// First-init hardware setup (`ops->init` on the 0→1 edge).
    fn hw_init(&self) -> Result<(), PhyError> {
        Ok(())
    }

    /// Last-exit teardown (`ops->exit` on the 1→0 edge).
    fn hw_exit(&self) -> Result<(), PhyError> {
        Ok(())
    }

    /// First-power-on (`ops->power_on` on the 0→1 edge).
    fn hw_power_on(&self) -> Result<(), PhyError> {
        Ok(())
    }

    /// Last-power-off (`ops->power_off` on the 1→0 edge). Providers
    /// whose power is folded into init/exit (the combphy/pcie3 reset +
    /// clock dance) leave this empty.
    fn hw_power_off(&self) {}

    /// Bind/consume a mode (`ops->set_mode`); provider-local semantics.
    fn hw_set_mode(&self, mode: PhyMode) -> Result<(), PhyError> {
        let _ = mode;
        Err(PhyError::NotSupported)
    }
}

/// Error vocabulary of the phy core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhyError {
    /// No provider registered under the requested phandle.
    NoProvider,
    /// The provider rejected the specifier (e.g. a combphy whose mode is
    /// already bound to a different type — Linux `xlate` conflict).
    ModeConflict,
    /// The provider has no PHY for that specifier.
    NotFound,
    /// The operation is not supported by this PHY.
    NotSupported,
    /// Unbalanced explicit call (exit/power_off with nothing acquired).
    Unbalanced,
    /// Provider hardware operation failed.
    Io,
}
