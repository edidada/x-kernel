// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cross-platform, standard clock providers.
//!
//! This module holds provider implementations that are pure (no firmware
//! description dependency) and so belong in the neutral crate: `fixed-clock`
//! is a single immutable rate. The DT-walking registration helper that finds
//! `fixed-clock` nodes and registers each under its node name lives outside
//! this crate (in the driver aggregator) because iterating the device tree is
//! a firmware-description concern and would pull a DT dependency into what is
//! otherwise an OS-agnostic crate.

use crate::ClkProvider;

/// Provider for a `compatible = "fixed-clock"` node: a single immutable rate
/// read once from the node's `clock-frequency` property. Such nodes carry
/// `#clock-cells = <0>`, so the specifier is always empty.
pub struct FixedClockProvider {
    rate: u64,
}

impl FixedClockProvider {
    /// Wrap a fixed rate, in Hz.
    pub const fn new(rate: u64) -> Self {
        Self { rate }
    }
}

impl ClkProvider for FixedClockProvider {
    fn get_rate(&self, _specifier: &[u32]) -> Option<u64> {
        Some(self.rate)
    }
}
