// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device-tree clock specifier extraction.
//!
//! A consumer node describes the clocks it consumes with two properties:
//!
//! ```dts
//! uart@9000000 {
//!     clocks = <&ccu 5>, <&ccu 12>;
//!     clock-names = "baudclk", "apb_pclk";
//! };
//! ```
//!
//! Each `clocks` entry is a *(phandle, specifier)* pair: the phandle selects
//! the clock controller, the specifier is controller-local (for most
//! controllers a single index cell). [`device_clock_specs`] returns the pairs
//! in declaration order, and [`device_clock_index_by_name`] resolves a
//! `clock-names` entry to its declaration index.
//!
//! The return type [`ClockSpec`] is deliberately *not* the device-res
//! [`ClkSpec`](device_res::ClkSpec) — this crate sits below `device-res` in
//! the dependency graph and must stay firmware-only. Callers (the DT-to-device
//! builder in kdevice/kdriver) convert `ClockSpec` to `ResourceDesc::Clk`.
//!
//! On boards without a clock controller DT node (QEMU `virt`), a plain
//! `clock-frequency = <N>` property on the consumer describes a fixed
//! reference; [`device_fixed_clock_hz`] extracts it so the platform can
//! publish it as a `FixedRate` clock.

use alloc::vec::Vec;

use crate::{FdtNode, fdt, property_u32};

/// One `assigned-clocks` entry: the target specifier plus the rate to apply.
/// Firmware-only view; the DT-to-device builder converts it to
/// `device_res::AssignedClock`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssignedClock {
    /// The controller's DT phandle.
    pub provider: u32,
    /// The controller-local clock index.
    pub index: u32,
    /// The rate in Hz to apply (0 = leave untouched per DT spec).
    pub rate_hz: u64,
}

/// One resolved `clocks` entry: the controller phandle plus the
/// controller-local specifier (an index for most controllers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockSpec {
    /// The controller's DT phandle (selects which provider).
    pub provider: u32,
    /// The controller-local clock index.
    pub index: u32,
    /// Consumer-local name at the same position in `clock-names`.
    pub connection_name: Option<&'static str>,
}

/// Extract the clock specifiers from a device node's `clocks` property.
///
/// Each `clocks` entry is `<&phandle specifier...>`; the phandle's
/// `#clock-cells` determines how many specifier cells follow (0 for a
/// fixed-clock referenced by phandle alone, 1 for the common index form).
/// Returns the [`ClockSpec`] list in declaration order.
pub fn device_clock_specs(node: FdtNode<'static, 'static>) -> Vec<ClockSpec> {
    device_clock_specs_for(node, "clocks")
}

/// [`device_clock_specs`] against an arbitrary phandle-specifier property
/// (same `<&phandle cells...>` layout, e.g. `assigned-clocks`).
pub fn device_clock_specs_for(node: FdtNode<'static, 'static>, prop_name: &str) -> Vec<ClockSpec> {
    let mut specs = Vec::new();

    let Some(prop) = node.property(prop_name) else {
        return specs;
    };
    let raw = prop.value;
    if raw.len() < 4 {
        return specs;
    }

    // Walk the raw cells: phandle, then #clock-cells specifier cells.
    let cells: Vec<u32> = raw
        .chunks_exact(4)
        .map(|c| u32::from_be_bytes(c.try_into().unwrap_or([0; 4])))
        .collect();

    let names = device_clock_names(node);
    let mut idx = 0usize;
    let mut spec_index = 0usize;
    while idx < cells.len() {
        let phandle = cells[idx];
        idx += 1;

        // The controller's #clock-cells tells us how many specifier cells
        // follow. Default to 1 (the overwhelmingly common form).
        let ncells = clock_cells_for_phandle(phandle).unwrap_or(1) as usize;

        if ncells == 0 {
            // A phandle-only reference (a fixed-clock node).
            specs.push(ClockSpec {
                provider: phandle,
                index: 0,
                connection_name: names.get(spec_index).copied(),
            });
            spec_index += 1;
        } else if idx + ncells <= cells.len() && ncells == 1 {
            specs.push(ClockSpec {
                provider: phandle,
                index: cells[idx],
                connection_name: names.get(spec_index).copied(),
            });
            idx += 1;
            spec_index += 1;
        } else {
            // Unsupported cell count or truncated entry: skip the rest
            // rather than emitting a wrong spec.
            break;
        }
    }

    specs
}

/// Resolve a `clock-names` entry to its declaration index.
///
/// Returns `None` when the node has no `clock-names`, or `name` is not in it.
pub fn device_clock_index_by_name(node: FdtNode<'static, 'static>, name: &str) -> Option<usize> {
    let names = device_clock_names(node);
    names.iter().position(|n| *n == name)
}

/// Parse the consumer-local `clock-names` list in declaration order.
pub fn device_clock_names(node: FdtNode<'static, 'static>) -> Vec<&'static str> {
    let Some(prop) = node.property("clock-names") else {
        return Vec::new();
    };
    let Ok(raw) = core::str::from_utf8(prop.value) else {
        return Vec::new();
    };
    raw.split('\0').filter(|name| !name.is_empty()).collect()
}

/// Read the first provider output name from `clock-output-names`.
pub fn clock_output_name(node: FdtNode<'static, 'static>) -> Option<&'static str> {
    let prop = node.property("clock-output-names")?;
    core::str::from_utf8(prop.value)
        .ok()?
        .split('\0')
        .find(|name| !name.is_empty())
}

/// Resolve a `clock-names` entry to its [`ClockSpec`].
pub fn device_clock_spec_by_name(node: FdtNode<'static, 'static>, name: &str) -> Option<ClockSpec> {
    let index = device_clock_index_by_name(node, name)?;
    device_clock_specs(node).into_iter().nth(index)
}

/// Parse `assigned-clocks` + `assigned-clock-rates` in declaration order.
///
/// The specifier cells parse exactly like `clocks`; each entry pairs with its
/// rate from `assigned-clock-rates`. Rates may be one cell (zero-extended
/// u32 — the common vendor form) or two cells (u64) per entry; the layout is
/// detected from the property length against the spec count. Entries without
/// a rate (or a truncated property) are dropped rather than guessed.
pub fn device_assigned_clocks(node: FdtNode<'static, 'static>) -> Vec<AssignedClock> {
    let specs = device_clock_specs_for(node, "assigned-clocks");
    if specs.is_empty() {
        return Vec::new();
    }
    let Some(prop) = node.property("assigned-clock-rates") else {
        return Vec::new();
    };
    let cells: Vec<u32> = prop
        .value
        .chunks_exact(4)
        .map(|c| u32::from_be_bytes(c.try_into().unwrap_or([0; 4])))
        .collect();

    // Per-entry cell width: 2 cells/entry (u64) when the property length
    // matches, else 1 cell/entry (u32 zero-extended).
    let wide = cells.len() == specs.len() * 2;
    if !wide && cells.len() < specs.len() {
        return Vec::new();
    }
    specs
        .into_iter()
        .enumerate()
        .map(|(i, spec)| {
            let rate_hz = if wide {
                (u64::from(cells[i * 2]) << 32) | u64::from(cells[i * 2 + 1])
            } else {
                u64::from(cells[i])
            };
            AssignedClock {
                provider: spec.provider,
                index: spec.index,
                rate_hz,
            }
        })
        .collect()
}

/// The fixed reference frequency from a `clock-frequency` property, if the
/// node carries one (QEMU `virt` describes its PL011 this way).
pub fn device_fixed_clock_hz(node: FdtNode<'static, 'static>) -> Option<u64> {
    property_u32(node, "clock-frequency").map(u64::from)
}

/// Read the controller node referenced by `phandle`'s `#clock-cells`.
fn clock_cells_for_phandle(phandle: u32) -> Option<u32> {
    let fdt = fdt()?;
    let controller = fdt
        .all_nodes()
        .find(|n| n.property_u32("phandle") == Some(phandle))?;
    controller.property_u32("#clock-cells")
}
