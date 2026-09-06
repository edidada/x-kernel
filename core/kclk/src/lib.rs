// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Common clock framework core for x-kernel.
//!
//! This crate is the in-kernel analogue of the Linux `drivers/clk/clk.c` core
//! plus the basic reusable clock types. It provides:
//!
//! - [`ClkHwOps`], the provider-side hardware operation trait (the `clk_ops`
//!   equivalent), including the two-phase `prepare`/`enable` gate.
//! - [`ClkCore`], a clock-tree node that caches rate/accuracy and serializes
//!   reference-counted gating and rate/parent changes that propagate to the
//!   selected parent.
//! - [`ClkLookup`] / [`register_provider`] / [`ClkTable`], the global provider
//!   registry that consumers query via [`clk_get_by_name`] /
//!   [`clk_get_by_index`] (by `(provider_id, index)`).
//! - [`ClkHandle`], the consumer-side reference returned to drivers, owning
//!   its per-consumer rate-range vote and notifier registration.
//! - Basic composable types in [`basic`]: [`FixedRate`], [`FixedFactor`],
//!   [`Gate`], [`Mux`], [`Divider`], [`Composite`].
//! - [`register_table`] with [`ClkBranch`]/[`ParentRef`]: declarative,
//!   order-independent batch registration for a whole controller, including
//!   cross-provider parents ([`ParentRef::External`]).
//! - [`clk_summary`] for bring-up diagnostics and [`clk_disable_unused`] for
//!   the late-init gate-down pass.
//!
//! Concrete clock-controller drivers (SoC PLL/mux/divider chips) register their
//! outputs here, mirroring how the [`device_res`] capability providers are
//! wired into the kernel.
//!
//! [`device_res`]: device_res
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

#[cfg(unittest)]
mod invariants;

#[cfg(unittest)]
mod tests;

pub use core::{ClkBuilder, ClkCore, ClkSummaryEntry, clk_disable_unused, clk_summary, clocks_len};

pub use basic::{
    Composite, DivTableEntry, Divider, DividerFlags, FixedFactor, FixedRate, Gate, GateFlags, Mux,
    MuxFlags,
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
