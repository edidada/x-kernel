// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Basic, reusable clock node types that implement [`ClkHwOps`](crate::ClkHwOps).
//!
//! These mirror the Linux `clk-fixed-rate`, `clk-fixed-factor`, `clk-gate`,
//! `clk-mux`, `clk-divider`, and `clk-composite` helpers. A clock-controller
//! driver composes a tree out of these building blocks and registers each node
//! via [`ClkBuilder`](crate::ClkBuilder); the framework owns rate caching and
//! reference-counted gating.

mod composite;
mod divider;
mod fixed_factor;
mod fixed_rate;
mod gate;
mod mux;

pub use composite::Composite;
pub use divider::{DivTableEntry, Divider, DividerFlags};
pub use fixed_factor::FixedFactor;
pub use fixed_rate::FixedRate;
pub use gate::{Gate, GateFlags};
pub use mux::Mux;
