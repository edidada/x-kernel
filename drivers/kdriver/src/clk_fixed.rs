// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DT-walking registration of `compatible = "fixed-clock"` providers.
//!
//! This lives in the driver aggregator (not in the OS-agnostic `clk` crate)
//! because iterating the device tree to find provider nodes is a firmware-
//! description concern: the `clk` registry itself is keyed by OS-neutral name
//! and knows nothing about the DT, so a future ACPI/PCI backend registers its
//! fixed-rate providers its own way without changing `clk`.
//!
//! Each `fixed-clock` node (a crystal/OSC the bootloader left running) becomes
//! a [`clk::FixedClockProvider`] registered under its DT node name with the
//! `@unit` suffix stripped — the same key the bus layer writes into `ClkDesc`
//! when a consumer's `clocks` property references it.

use alloc::sync::Arc;

use clk::{FixedClockProvider, register_provider};

/// Register every `compatible = "fixed-clock"` DT node as a clock provider,
/// keyed by its (unit-stripped) node name.
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

        // A node nobody references contributes nothing queryable, but unlike
        // the old phandle-keyed scheme a name always exists — skip only when
        // the node is malformed (no rate).
        let Some(rate) = node.property_u32("clock-frequency") else {
            log::warn!(
                "clk: fixed-clock {:?} has no clock-frequency; skipped",
                node.name
            );
            continue;
        };

        let name = of::node_name_key(node.name);
        register_provider(name, Arc::new(FixedClockProvider::new(rate as u64)));
        log::debug!("clk: registered fixed-clock {:?} = {} Hz", name, rate);
    }
}
