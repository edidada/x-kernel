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

/// One resolved `clocks` entry: the controller phandle plus the
/// controller-local specifier (an index for most controllers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockSpec {
    /// The controller's DT phandle (selects which provider).
    pub provider: u32,
    /// The controller-local clock index.
    pub index: u32,
}

/// Extract the clock specifiers from a device node's `clocks` property.
///
/// Each `clocks` entry is `<&phandle specifier...>`; the phandle's
/// `#clock-cells` determines how many specifier cells follow (0 for a
/// fixed-clock referenced by phandle alone, 1 for the common index form).
/// Returns the [`ClockSpec`] list in declaration order.
pub fn device_clock_specs(node: FdtNode<'static, 'static>) -> Vec<ClockSpec> {
    let mut specs = Vec::new();

    let Some(prop) = node.property("clocks") else {
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

    let mut idx = 0usize;
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
            });
        } else if idx + ncells <= cells.len() && ncells == 1 {
            specs.push(ClockSpec {
                provider: phandle,
                index: cells[idx],
            });
            idx += 1;
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
    let prop = node.property("clock-names")?;
    let raw = prop.value;
    let names: Vec<&str> = core::str::from_utf8(raw)
        .ok()?
        .split('\0')
        .filter(|s| !s.is_empty())
        .collect();
    names.iter().position(|n| *n == name)
}

/// Resolve a `clock-names` entry to its [`ClockSpec`].
pub fn device_clock_spec_by_name(node: FdtNode<'static, 'static>, name: &str) -> Option<ClockSpec> {
    let index = device_clock_index_by_name(node, name)?;
    device_clock_specs(node).into_iter().nth(index)
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
