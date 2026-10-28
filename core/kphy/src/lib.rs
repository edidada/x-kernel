// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `kphy` — the PHY core: shared instances with framework-owned
//! refcounts, provider registry with acquire-time specifier binding.
//!
//! Layering mirrors kclk/kreset:
//! - this crate is the neutral core (instance counts, lookup, registry);
//! - a phy driver publishes a [`PhyLookup`] under its DT phandle, binding
//!   provider-local state (mode) in `acquire`;
//! - the device-model bridge (driver-host) resolves `phys = <&phy ...>`
//!   specifiers with deferral while the provider has not registered.

#![no_std]

extern crate alloc;

mod handle;
mod hw;
mod instance;
mod provider;

pub use handle::{PhyHandle, PoweredPhy};
pub use hw::{PhyError, PhyHwOps, PhyMode};
pub use instance::PhyInstance;
pub use provider::{PhyLookup, PhySpecifier, ProviderId, phy_get, register_provider};
#[cfg(unittest)]
pub use provider::{provider_installed, unregister_provider};

#[cfg(unittest)]
mod tests;
