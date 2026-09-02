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

mod error;
mod flags;
mod ops;
mod reg;

pub use basic::{Composite, DivTableEntry, Divider, DividerFlags, FixedFactor, FixedRate, Gate, GateFlags, Mux};
pub use error::{ClkError, ClkResult};
pub use flags::ClkFlags;
pub use ops::ClkHwOps;
pub use reg::{ClkRegAccess, MmioClkReg, NullClkReg};
