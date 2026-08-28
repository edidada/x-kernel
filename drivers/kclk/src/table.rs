// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Table-driven batch registration — the declarative layer over [`ClkBuilder`].
//!
//! A real SoC clock controller has hundreds of nodes; spelling each out as
//! `ClkBuilder::new(..).parent(..).build()` is repetitive and order-coupled
//! (a child needs its parent's `Arc` first). This module mirrors Linux's
//! `clk_branches[]` + `rockchip_clk_register_branches()`:
//!
//! 1. A static `&[ClkBranch]` table — one declarative entry per clock, parents
//!    referenced **by name** (not `Arc`), so the table may be filled in any
//!    order. The basic-type entries are `const`-constructible, so a controller
//!    with no custom PLLs can live in a `static`.
//! 2. [`register_table`] builds every node with empty parent slots, then
//!    resolves parents by name (order-independent), then refreshes cached
//!    rates top-down (parent before child), then publishes them in a
//!    [`ClkTable`].
//!
//! Custom PLLs go in the same table via [`ClkNodeDesc::Custom`]; a table
//! containing any `Custom` entry is not `const` (the `Arc` is runtime), so build
//! it as a local array in the controller's init function.
//!
//! ```ignore
//! static CCU: kclk::ClkTable = kclk::ClkTable::new(PHANDLE);
//! static BRANCHES: [kclk::ClkBranch; 3] = [
//!     kclk::ClkBranch::fixed_rate(0, "osc24m", 24_000_000),
//!     kclk::ClkBranch::fixed_factor(1, "pll1200", 50, 1, &["osc24m"]),
//!     kclk::ClkBranch::gate(2, "uart_clk", 0x30, 7, kclk::GateFlags::SET_TO_ENABLE, &["pll1200"]),
//! ];
//! kclk::register_table(&CCU, reg, &BRANCHES).unwrap();
//! kclk::register_provider(&CCU);
//! ```

use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    vec::Vec,
};

use crate::{
    ClkBuilder, ClkCore, ClkError, ClkFlags, ClkHwOps, ClkIndex, ClkRegAccess, ClkResult, ClkTable,
    Composite, Divider, DividerFlags, FixedFactor, FixedRate, Gate, GateFlags, Mux,
    core::with_clk_lock,
};

/// One declarative clock entry.
///
/// `parents` are referenced by name and resolved by [`register_table`] after
/// every node exists, so the table may list children before their parents.
pub struct ClkBranch {
    /// Provider-relative DT index to publish this clock at.
    pub index: ClkIndex,
    /// Globally-unique clock name.
    pub name: &'static str,
    /// Selectable parents, by name. Empty for a root.
    pub parents: &'static [&'static str],
    /// Policy flags.
    pub flags: ClkFlags,
    /// Hardware description (gate / mux / divider / ...).
    pub node: ClkNodeDesc,
}
impl ClkBranch {
    /// Build an entry from its raw parts.
    pub const fn new(
        index: ClkIndex,
        name: &'static str,
        parents: &'static [&'static str],
        flags: ClkFlags,
        node: ClkNodeDesc,
    ) -> Self {
        Self {
            index,
            name,
            parents,
            flags,
            node,
        }
    }

    /// Set policy flags. Const-chainable after a typed constructor.
    pub const fn with_flags(mut self, flags: ClkFlags) -> Self {
        self.flags = ClkFlags::from_bits_truncate(self.flags.bits() | flags.bits());
        self
    }

    /// Fixed-rate clock (a crystal); no parent.
    pub const fn fixed_rate(index: ClkIndex, name: &'static str, rate: u64) -> Self {
        Self::new(
            index,
            name,
            &[],
            ClkFlags::empty(),
            ClkNodeDesc::FixedRate { rate },
        )
    }

    /// Fixed-factor clock (fixed `mult`/`div` ratio).
    pub const fn fixed_factor(
        index: ClkIndex,
        name: &'static str,
        mult: u32,
        div: u32,
        parents: &'static [&'static str],
    ) -> Self {
        Self::new(
            index,
            name,
            parents,
            ClkFlags::empty(),
            ClkNodeDesc::FixedFactor { mult, div },
        )
    }

    /// Gate clock (a single enable bit).
    pub const fn gate(
        index: ClkIndex,
        name: &'static str,
        offset: usize,
        bit: u8,
        flags: GateFlags,
        parents: &'static [&'static str],
    ) -> Self {
        Self::new(
            index,
            name,
            parents,
            ClkFlags::empty(),
            ClkNodeDesc::Gate { offset, bit, flags },
        )
    }

    /// Mux clock (parent-selector field).
    pub const fn mux(
        index: ClkIndex,
        name: &'static str,
        offset: usize,
        shift: u8,
        mask: u32,
        table: Option<&'static [u8]>,
        parents: &'static [&'static str],
    ) -> Self {
        Self::new(
            index,
            name,
            parents,
            ClkFlags::empty(),
            ClkNodeDesc::Mux {
                offset,
                shift,
                mask,
                table,
            },
        )
    }

    /// Divider clock (programmable divider field).
    #[allow(clippy::too_many_arguments)]
    pub const fn divider(
        index: ClkIndex,
        name: &'static str,
        offset: usize,
        shift: u8,
        width: u8,
        table: Option<&'static [u32]>,
        flags: DividerFlags,
        parents: &'static [&'static str],
    ) -> Self {
        Self::new(
            index,
            name,
            parents,
            ClkFlags::empty(),
            ClkNodeDesc::Divider {
                offset,
                shift,
                width,
                table,
                flags,
            },
        )
    }

    /// Composite clock (independent mux + divider + gate sub-nodes).
    pub const fn composite(
        index: ClkIndex,
        name: &'static str,
        mux: Option<MuxDesc>,
        div: Option<DividerDesc>,
        gate: Option<GateDesc>,
        parents: &'static [&'static str],
        flags: ClkFlags,
    ) -> Self {
        Self::new(
            index,
            name,
            parents,
            flags,
            ClkNodeDesc::Composite { mux, div, gate },
        )
    }

    /// Custom clock (a hand-written `ClkHwOps`, e.g. a PLL). Not `const`.
    pub fn custom(
        index: ClkIndex,
        name: &'static str,
        ops: Arc<dyn ClkHwOps>,
        parents: &'static [&'static str],
        flags: ClkFlags,
    ) -> Self {
        Self::new(index, name, parents, flags, ClkNodeDesc::Custom { ops })
    }
}

/// Declarative hardware description for a [`ClkBranch`].
///
/// Every variant except [`Custom`](Self::Custom) is `const`-constructible; a
/// table using only those variants can be a `static`.
pub enum ClkNodeDesc {
    /// Fixed-rate clock (a crystal), no parent.
    FixedRate {
        /// Frequency in Hz.
        rate: u64,
    },
    /// Fixed `mult`/`div` ratio of the parent rate.
    FixedFactor {
        /// Numerator.
        mult: u32,
        /// Denominator.
        div: u32,
    },
    /// A single enable bit.
    Gate {
        /// Byte offset of the register word.
        offset: usize,
        /// Enable-bit position.
        bit: u8,
        /// Gate polarity flags.
        flags: GateFlags,
    },
    /// A parent-selector field.
    Mux {
        /// Byte offset of the register word.
        offset: usize,
        /// LSB position of the field.
        shift: u8,
        /// Unshifted field mask (e.g. `0x7` for 3 bits).
        mask: u32,
        /// Optional parent-index translation table.
        table: Option<&'static [u8]>,
    },
    /// A programmable divider field.
    Divider {
        /// Byte offset of the register word.
        offset: usize,
        /// LSB position of the field.
        shift: u8,
        /// Field width in bits (`1..32`).
        width: u8,
        /// Optional explicit divider-value table.
        table: Option<&'static [u32]>,
        /// Divider policy flags.
        flags: DividerFlags,
    },
    /// Independent mux + divider + gate sub-nodes.
    Composite {
        /// Optional mux sub-node.
        mux: Option<MuxDesc>,
        /// Optional divider sub-node.
        div: Option<DividerDesc>,
        /// Optional gate sub-node.
        gate: Option<GateDesc>,
    },
    /// A hand-written `ClkHwOps` (a PLL). Makes the enclosing table non-`const`.
    Custom {
        /// The provider ops.
        ops: Arc<dyn ClkHwOps>,
    },
}

/// Mux sub-node description for [`ClkNodeDesc::Composite`].
pub struct MuxDesc {
    /// Byte offset of the register word.
    pub offset: usize,
    /// LSB position of the field.
    pub shift: u8,
    /// Unshifted field mask (e.g. `0x7` for 3 bits).
    pub mask: u32,
    /// Optional parent-index translation table.
    pub table: Option<&'static [u8]>,
}

/// Divider sub-node description for [`ClkNodeDesc::Composite`].
pub struct DividerDesc {
    /// Byte offset of the register word.
    pub offset: usize,
    /// LSB position of the field.
    pub shift: u8,
    /// Field width in bits (`1..32`).
    pub width: u8,
    /// Optional explicit divider-value table.
    pub table: Option<&'static [u32]>,
    /// Divider policy flags.
    pub flags: DividerFlags,
}

/// Gate sub-node description for [`ClkNodeDesc::Composite`].
pub struct GateDesc {
    /// Byte offset of the register word.
    pub offset: usize,
    /// Enable-bit position.
    pub bit: u8,
    /// Gate polarity flags.
    pub flags: GateFlags,
}

/// Build a whole clock tree from a declarative table.
///
/// Order-independent: parents are referenced by name and resolved after every
/// node exists, so the table may list children before their parents. Cached
/// rates are refreshed top-down (parent before child) once parents are
/// resolved. Each node is published in `table` at its declared `(index, name)`.
///
/// A controller with no custom PLLs can pass a `&'static [ClkBranch]`; a
/// controller with PLLs builds the array at runtime (the `Custom` `Arc` is not
/// `const`).
///
/// # Failure atomicity
///
/// Duplicate names/indices and bad descriptors (invalid fixed-factor, divider
/// width) are rejected before any global state is touched. If a node's
/// `build()` (init/recalc) or the resolve/refresh pass fails partway, the
/// batch is rolled back out of the global clock list before returning the
/// error, so no unreachable-but-listed nodes are left for
/// `clk_disable_unused` or a later same-name registration to trip over. The
/// provider table (`ClkTable`) is only written after everything succeeded, so
/// consumers never observe a partial tree. Treat a failed `register_table`
/// as a fatal controller-probe error regardless.
pub fn register_table(
    table: &ClkTable,
    reg: Arc<dyn ClkRegAccess>,
    branches: &[ClkBranch],
) -> ClkResult {
    // Pre-flight: reject duplicate names and duplicate DT indices up front, so
    // a bad table fails before any node is built and published.
    let mut seen_names: BTreeSet<&'static str> = BTreeSet::new();
    let mut seen_indices: BTreeSet<ClkIndex> = BTreeSet::new();
    for b in branches {
        if !seen_names.insert(b.name) {
            return Err(ClkError::BadState); // duplicate name in the table
        }
        if !seen_indices.insert(b.index) {
            return Err(ClkError::BadState); // duplicate DT index
        }
    }
    // Pre-build every descriptor's ops before touching any global state, so a
    // bad descriptor (e.g. a zero fixed-factor) fails before nodes enter the
    // global clock list. (build() and the refresh below can still fail late —
    // registration is not fully transactional; see the doc on this function.)
    let mut ops_list = Vec::with_capacity(branches.len());
    for b in branches {
        ops_list.push(build_ops(&b.node, &reg)?);
    }

    // Pass 1 — build every node with empty parent slots. `build()` seeds the
    // initial parent index and (zero) rate from the hardware; rates are fixed
    // in pass 3. Not under CLK_LOCK: build() internally takes it via
    // adopt_orphans, and re-entrancy would panic.
    //
    // Rollback: record how many entries this batch pushed into the global
    // CLOCKS list; on any later failure, truncate back to that mark so a
    // failed registration does not leave unreachable-but-listed nodes for
    // clk_disable_unused / future same-name adopters to trip over.
    let clocks_mark = with_clk_lock(crate::core::clocks_len);
    let mut nodes: BTreeMap<&'static str, Arc<ClkCore>> = BTreeMap::new();
    for (b, ops) in branches.iter().zip(ops_list) {
        let parents: Vec<Option<Arc<ClkCore>>> = (0..b.parents.len()).map(|_| None).collect();
        let core = match ClkBuilder::new(b.name, ops)
            .flags(b.flags)
            .parents(parents)
            .build()
        {
            Ok(core) => core,
            Err(e) => {
                rollback_clocks(clocks_mark);
                return Err(e);
            }
        };
        nodes.insert(b.name, core);
    }

    // Pass 2+3 — under CLK_LOCK: resolve name-referenced parents into slots,
    // then refresh cached rates top-down so deep subtrees seeded against a
    // zero parent rate pick up the right value now that parents are linked.
    // A failure here (cycle, refresh error) rolls the batch back out of the
    // global clock list.
    let dep_map: BTreeMap<&'static str, &'static [&'static str]> =
        branches.iter().map(|b| (b.name, b.parents)).collect();
    let resolve_result = with_clk_lock(|| -> ClkResult<()> {
        for b in branches {
            let node = &nodes[b.name];
            for (i, pname) in b.parents.iter().enumerate() {
                if let Some(parent) = nodes.get(*pname) {
                    node.set_parent_slot(i as u8, parent.clone());
                }
                // A parent not in this table stays an empty slot. Note: this
                // does NOT register an orphan for later adoption — only
                // `ClkBuilder::parent_name` does that. A cross-controller
                // parent must be resolved by the caller (e.g. pre-built and
                // seeded) or the slot remains None and the rate reads 0.
            }
        }
        // Detect parent cycles before the recursive rate refresh: a cyclic
        // table would otherwise recurse forever at prepare/enable time.
        let mut visiting: BTreeSet<&'static str> = BTreeSet::new();
        let mut visited: BTreeSet<&'static str> = BTreeSet::new();
        for b in branches {
            refresh_subtree(b.name, &nodes, &dep_map, &mut visiting, &mut visited)?;
        }
        Ok(())
    });
    if let Err(e) = resolve_result {
        rollback_clocks(clocks_mark);
        return Err(e);
    }

    // Pass 4 — publish each output in the provider table.
    for b in branches {
        table.register(b.index, b.name, nodes[b.name].clone());
    }
    Ok(())
}

/// Truncate the global clock list back to `mark` entries — the rollback half
/// of [`register_table`]'s batch registration. Callers that did not record a
/// mark must not call this.
fn rollback_clocks(mark: usize) {
    with_clk_lock(|| crate::core::truncate_clocks(mark));
}

/// Construct the hardware ops for a descriptor, sharing the controller's
/// register accessor.
fn build_ops(node: &ClkNodeDesc, reg: &Arc<dyn ClkRegAccess>) -> ClkResult<Arc<dyn ClkHwOps>> {
    Ok(match node {
        ClkNodeDesc::FixedRate { rate } => FixedRate::new(*rate).into_ops(),
        ClkNodeDesc::FixedFactor { mult, div } => FixedFactor::new(*mult, *div)?.into_ops(),
        ClkNodeDesc::Gate { offset, bit, flags } => {
            Gate::into_ops(reg.clone(), *offset, *bit, *flags)
        }
        ClkNodeDesc::Mux {
            offset,
            shift,
            mask,
            table,
        } => Mux::into_ops(reg.clone(), *offset, *shift, *mask, *table),
        ClkNodeDesc::Divider {
            offset,
            shift,
            width,
            table,
            flags,
        } => Divider::into_ops(reg.clone(), *offset, *shift, *width, *table, *flags),
        ClkNodeDesc::Composite { mux, div, gate } => {
            let m = mux
                .as_ref()
                .map(|d| Mux::into_ops(reg.clone(), d.offset, d.shift, d.mask, d.table));
            let dv = div.as_ref().map(|d| {
                Divider::into_ops(reg.clone(), d.offset, d.shift, d.width, d.table, d.flags)
            });
            let g = gate
                .as_ref()
                .map(|d| Gate::into_ops(reg.clone(), d.offset, d.bit, d.flags));
            Composite::new(m, dv, g)
        }
        ClkNodeDesc::Custom { ops } => ops.clone(),
    })
}

/// Depth-first rate refresh: ensure each parent is refreshed before this node,
/// then recompute this node's cached rate from its (now-current) parent rate.
///
/// `visiting` tracks the current DFS stack to reject parent cycles (a cyclic
/// table is a configuration error); `visited` skips already-refreshed nodes.
fn refresh_subtree(
    name: &'static str,
    nodes: &BTreeMap<&'static str, Arc<ClkCore>>,
    dep_map: &BTreeMap<&'static str, &'static [&'static str]>,
    visiting: &mut BTreeSet<&'static str>,
    visited: &mut BTreeSet<&'static str>,
) -> ClkResult {
    // Black (fully refreshed) → short-circuit BEFORE the gray check, otherwise
    // a diamond A→B→C, A→C would misfire; gray (on the DFS stack) → cycle.
    if visited.contains(name) {
        return Ok(());
    }
    if !visiting.insert(name) {
        return Err(ClkError::BadState); // cycle: `name` is on its own DFS stack
    }
    if let Some(&parents) = dep_map.get(name) {
        for &pname in parents {
            // Only recurse into parents that live in this table; an external
            // (unresolved) parent keeps its own rate and is read as-is.
            if nodes.contains_key(pname) {
                refresh_subtree(pname, nodes, dep_map, visiting, visited)?;
            }
        }
    }
    visiting.remove(name);
    visited.insert(name);
    nodes[name].refresh_rate();
    Ok(())
}
