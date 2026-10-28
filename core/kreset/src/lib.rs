// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `kreset` — the reset-line core (the `kclk` pattern, reset semantics).
//!
//! Layering mirrors the clock story:
//! - this crate is the neutral core: lines, provider lookup, registry;
//! - a controller driver (e.g. the RK3588 CRU) publishes a
//!   [`ResetLookup`] during probe under its DT phandle;
//! - the device-model bridge (driver-host) resolves consumer specifiers
//!   through [`reset_get`] and hands out [`ResetHandle`]s with deferral
//!   when the provider has not registered yet.
//!
//! Deliberately minimal: no rate topology, no dependency graph, no shared
//! refcount — Linux `reset_control_assert/deassert/status` map one-to-one
//! onto [`ResetLine`] methods.

#![no_std]

extern crate alloc;

mod line;
mod provider;

pub use line::{DeassertedGuard, ResetHandle, ResetLine, ResetRegAccess};
pub use provider::{ProviderId, ResetError, ResetIndex, ResetLookup, register_provider, reset_get};
#[cfg(unittest)]
pub use provider::{provider_installed, unregister_provider};

#[cfg(unittest)]
mod tests;
