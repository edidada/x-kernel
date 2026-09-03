// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Common clock framework core for x-kernel.
//!
//! This crate is the in-kernel analogue of the Linux `drivers/clk/clk.c` core
//! plus the basic reusable clock types. This first slice provides the
//! provider-side hardware operation trait ([`ClkHwOps`]) and the composable
//! basic types in [`basic`] ([`FixedRate`], [`FixedFactor`], [`Gate`],
//! [`Mux`], [`Divider`], [`Composite`]).
//!
//! The node model, provider registry, consumer handles, and declarative batch
//! registration land in subsequent slices of this crate.
#![cfg_attr(not(test), no_std)]
#![warn(missing_docs)]

extern crate alloc;

pub mod basic;

mod core;
mod error;
mod event;
mod flags;
mod handle;
mod lookup;
mod ops;
mod provider;
mod rate;
mod reg;
mod table;

pub use core::{ClkBuilder, ClkCore, ClkSummaryEntry, clk_disable_unused, clk_summary, clocks_len};

pub use basic::{
    Composite, DivTableEntry, Divider, DividerFlags, FixedFactor, FixedRate, Gate, GateFlags, Mux,
};
pub use error::{ClkError, ClkResult};
pub use event::{ClkEvent, ClkNotifier, ClkNotifierId, NotifierResult};
pub use flags::ClkFlags;
pub use handle::ClkHandle;
pub use lookup::ClkTable;
pub use ops::ClkHwOps;
#[cfg(unittest)]
pub use provider::reset_providers;
#[cfg(unittest)]
pub use provider::unregister_provider;
pub use provider::{
    ClkLookup, ProviderId, clk_get_by_index, clk_get_by_name, provider_installed, register_provider,
};
pub use rate::{ClkRateRequest, Rate, RoundResult};
pub use reg::{ClkRegAccess, MmioClkReg, NullClkReg};

pub use table::{
    ClkBranch, ClkNodeDesc, DividerDesc, GateDesc, MuxDesc, ParentRef, register_table,
};

/// Index into a provider's clock table.
///
/// Corresponds to the cell value in a DT `clocks = <&phandle N>` specifier.
pub type ClkIndex = u32;

#[cfg(unittest)]
mod tests;
