// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cross-platform, standard clock providers.
//!
//! These are not SoC-specific: `fixed-clock` is a DT binding every platform
//! uses for crystals/oscillators, so it lives in the neutral crate rather than
//! under per-SoC glue. Future additions (a `fixed-factor-clock`, an SCMI clock
//! client) follow the same rule — anything specified by a standard binding or
//! an industry protocol belongs here and serves every platform at once.

use alloc::sync::Arc;

use crate::{ClkProvider, registry::register_provider};

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

/// Register every `compatible = "fixed-clock"` DT node as a provider keyed by
/// its phandle.
///
/// Safe to call once during driver init regardless of platform: with no DT
/// present it is a no-op. Each crystal/OSC (e.g. `xin24m`, `xin32k`) that the
/// bootloader already has running becomes queryable, which later lets a SoC
/// CRU provider compute derived rates by walking the parent chain.
pub fn register_fixed_clocks() {
    let Some(fdt) = of::fdt() else {
        return;
    };

    for node in fdt.all_nodes() {
        if !node.is_compatible("fixed-clock") {
            continue;
        }

        let Some(phandle) = node
            .property_u32("phandle")
            .or_else(|| node.property_u32("linux,phandle"))
        else {
            // A fixed-clock nobody references has no phandle; nothing can look
            // it up, so there is no point registering it.
            continue;
        };

        let Some(rate) = node.property_u32("clock-frequency") else {
            log::warn!(
                "clk: fixed-clock {:?} has a phandle but no clock-frequency; skipped",
                node.name
            );
            continue;
        };

        register_provider(phandle, Arc::new(FixedClockProvider::new(rate as u64)));
        log::debug!(
            "clk: registered fixed-clock {:#x} ({:?}) = {} Hz",
            phandle,
            node.name,
            rate
        );
    }
}
