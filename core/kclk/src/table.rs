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
//!     kclk::ClkBranch::gate(2, "uart_clk", 0x30, 7, kclk::GateFlags::empty(), &["pll1200"]),
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
    Composite, Divider, DividerFlags, FixedFactor, FixedRate, Gate, GateFlags, Mux, MuxFlags,
    core::with_clk_lock,
};

/// A reference to one selectable parent of a [`ClkBranch`].
///
/// [`Local`] names a clock in the same `register_table` call — resolved after
/// every node exists, so the table may list children before their parents.
/// [`External`] names a clock published by another (already-registered)
/// provider, by `(provider id, DT index)` — the same pair a DT
/// `clocks = <&phandle N>` specifier carries. This lets a controller describe
/// cross-controller dependencies (e.g. a PMU clock derived from a main-CCU
/// output) declaratively instead of pre-building `Arc<ClkCore>` handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentRef {
    /// A clock registered by this same table, by name.
    Local(&'static str),
    /// A clock of another registered provider, by `(provider id, DT index)`.
    External(crate::ProviderId, ClkIndex),
}

impl From<&'static str> for ParentRef {
    fn from(name: &'static str) -> Self {
        ParentRef::Local(name)
    }
}

impl ParentRef {
    /// The local name, if this is a [`ParentRef::Local`].
    pub const fn local_name(&self) -> Option<&'static str> {
        match self {
            ParentRef::Local(n) => Some(n),
            ParentRef::External(..) => None,
        }
    }
}

/// One declarative clock entry.
///
/// `parents` are referenced by name and resolved by [`register_table`] after
/// every node exists, so the table may list children before their parents.
///
/// `Clone` lets a controller combine a `const` static table with runtime-built
/// entries (custom PLL/frac ops) into one `register_table` batch.
#[derive(Clone)]
pub struct ClkBranch {
    /// Provider-relative DT index to publish this clock at.
    pub(crate) index: ClkIndex,
    /// Globally-unique clock name.
    pub(crate) name: &'static str,
    /// Selectable parents, in exactly one of the two spellings — a struct
    /// literal cannot set both with one silently ignored.
    pub(crate) parents: ParentList,
    /// Policy flags.
    pub(crate) flags: ClkFlags,
    /// Hardware description (gate / mux / divider / ...).
    pub node: ClkNodeDesc,
}

/// The parent list of a [`ClkBranch`], in one of its two spellings.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ParentList {
    /// Local names within this table (the common single-controller form).
    Names(&'static [&'static str]),
    /// Structured references, including cross-provider externals.
    Refs(&'static [ParentRef]),
}
impl ClkBranch {
    /// The provider-relative DT index (loop diagnostics / critical walk).
    pub const fn index(&self) -> ClkIndex {
        self.index
    }

    /// The globally-unique clock name.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The policy flags (the critical walk filters on IS_CRITICAL).
    pub const fn flags(&self) -> ClkFlags {
        self.flags
    }

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
            parents: ParentList::Names(parents),
            flags,
            node,
        }
    }

    /// Build an entry from structured [`ParentRef`]s (locals by name plus
    /// cross-provider externals). `refs` takes precedence over the plain
    /// string form at registration time.
    pub const fn new_refs(
        index: ClkIndex,
        name: &'static str,
        refs: &'static [ParentRef],
        flags: ClkFlags,
        node: ClkNodeDesc,
    ) -> Self {
        Self {
            index,
            name,
            parents: ParentList::Refs(refs),
            flags,
            node,
        }
    }

    /// Resolve this entry's parents as [`ParentRef`]s: the structured form if
    /// present, else each plain name upgraded to [`ParentRef::Local`].
    fn parent_refs(&self) -> Vec<ParentRef> {
        match self.parents {
            ParentList::Names(names) => names.iter().map(|n| ParentRef::Local(n)).collect(),
            ParentList::Refs(refs) => refs.to_vec(),
        }
    }

    /// Number of selectable parents.
    fn parent_count(&self) -> usize {
        match self.parents {
            ParentList::Names(names) => names.len(),
            ParentList::Refs(refs) => refs.len(),
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
    #[expect(
        clippy::too_many_arguments,
        reason = "CG-024-lint-exceptions: the constructor carries the clock table plus its                   parent context, matching the firmware description"
    )]
    pub const fn mux(
        index: ClkIndex,
        name: &'static str,
        offset: usize,
        shift: u8,
        mask: u32,
        table: Option<&'static [u8]>,
        flags: MuxFlags,
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
                flags,
            },
        )
    }

    /// Divider clock (programmable divider field).
    #[expect(
        clippy::too_many_arguments,
        reason = "CG-024-lint-exceptions: the constructor carries the clock table plus its                   parent context, matching the firmware description"
    )]
    pub const fn divider(
        index: ClkIndex,
        name: &'static str,
        offset: usize,
        shift: u8,
        width: u8,
        table: Option<&'static [crate::DivTableEntry]>,
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
#[derive(Clone)]
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
        /// Mux policy flags.
        flags: MuxFlags,
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
        table: Option<&'static [crate::DivTableEntry]>,
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
#[derive(Clone, Copy)]
pub struct MuxDesc {
    /// Byte offset of the register word.
    pub offset: usize,
    /// LSB position of the field.
    pub shift: u8,
    /// Unshifted field mask (e.g. `0x7` for 3 bits).
    pub mask: u32,
    /// Optional parent-index translation table.
    pub table: Option<&'static [u8]>,
    /// Mux policy flags.
    pub flags: MuxFlags,
}

/// Divider sub-node description for [`ClkNodeDesc::Composite`].
#[derive(Clone, Copy)]
pub struct DividerDesc {
    /// Byte offset of the register word.
    pub offset: usize,
    /// LSB position of the field.
    pub shift: u8,
    /// Field width in bits (`1..32`).
    pub width: u8,
    /// Optional `(register value, divider)` table (Linux `clk_div_table`).
    pub table: Option<&'static [crate::DivTableEntry]>,
    /// Divider policy flags.
    pub flags: DividerFlags,
}

/// Gate sub-node description for [`ClkNodeDesc::Composite`].
#[derive(Clone, Copy)]
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
    let mut nodes: BTreeMap<&'static str, Arc<ClkCore>> = BTreeMap::new();
    for (b, ops) in branches.iter().zip(ops_list) {
        let n = b.parent_count();
        let parents: Vec<Option<Arc<ClkCore>>> = (0..n).map(|_| None).collect();
        let core = match ClkBuilder::new(b.name, ops)
            .flags(b.flags)
            .parents(parents)
            .build()
        {
            Ok(core) => core,
            Err(e) => {
                rollback_nodes(&nodes);
                return Err(e);
            }
        };
        nodes.insert(b.name, core);
    }

    // Pass 2+3 — under CLK_LOCK: resolve parent references into slots, then
    // refresh cached rates top-down so deep subtrees seeded against a zero
    // parent rate pick up the right value now that parents are linked. A
    // failure here (cycle, refresh error, unresolved external) rolls the
    // batch back out of the global clock list.
    //
    // `Local` resolves within this batch (order-independent); `External`
    // resolves through the provider registry (the referenced controller must
    // already be registered) — the same `(phandle, index)` pair a DT clocks
    // specifier carries. An unresolved reference of either kind is an error:
    // a half-linked tree reads rate 0 at the gap and misleads consumers.
    let dep_map: BTreeMap<&'static str, Vec<&'static str>> = branches
        .iter()
        .map(|b| {
            (
                b.name,
                b.parent_refs()
                    .iter()
                    .filter_map(|r| r.local_name())
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    let resolve_result = with_clk_lock(|| -> ClkResult<()> {
        for b in branches {
            let node = &nodes[b.name];
            for (i, pref) in b.parent_refs().iter().enumerate() {
                let parent = match pref {
                    ParentRef::Local(pname) => match nodes.get(pname) {
                        Some(n) => n.clone(),
                        // Not in this batch: fall back to the global provider
                        // registry (Linux resolves branch parent names through
                        // the global clock lookup at registration time). This
                        // is what lets a second controller (e.g. the RK3588
                        // PMU CRU) hang its outputs off main-CRU parents.
                        None => crate::provider::clk_get_core_by_name(pname).map_err(|_| {
                            // Bring-up diagnostic: name the culprit — an
                            // InvalidParent with no name is undebuggable on
                            // hardware.
                            log::warn!(
                                "kclk: parent `{pname}` not in this batch and not registered \
                                 globally"
                            );
                            ClkError::InvalidParent
                        })?,
                    },
                    ParentRef::External(pid, idx) => crate::provider::clk_get_by_index(*pid, *idx)
                        .map_err(|_| ClkError::InvalidParent)?
                        .into_core(),
                };
                node.resolve_parent_slot_locked(i as u8, parent)?;
            }
        }
        // Detect parent cycles before the recursive rate refresh: a cyclic
        // table would otherwise recurse forever at prepare/enable time.
        // (Externals cannot participate in a cycle — they are already-built
        // nodes in other providers.)
        let mut visiting: BTreeSet<&'static str> = BTreeSet::new();
        let mut visited: BTreeSet<&'static str> = BTreeSet::new();
        for b in branches {
            refresh_subtree(b.name, &nodes, &dep_map, &mut visiting, &mut visited)?;
        }
        Ok(())
    });
    if let Err(e) = resolve_result {
        rollback_nodes(&nodes);
        return Err(e);
    }

    // Pass 4 — publish each output in the provider table.
    for b in branches {
        table.register(b.index, b.name, nodes[b.name].clone());
    }
    Ok(())
}

fn rollback_nodes(nodes: &BTreeMap<&'static str, Arc<ClkCore>>) {
    let nodes: Vec<Arc<ClkCore>> = nodes.values().cloned().collect();
    crate::core::rollback_nodes(&nodes);
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
            flags,
        } => Mux::into_ops(reg.clone(), *offset, *shift, *mask, *table, *flags),
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
                .map(|d| Mux::into_ops(reg.clone(), d.offset, d.shift, d.mask, d.table, d.flags));
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
    dep_map: &BTreeMap<&'static str, Vec<&'static str>>,
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
    if let Some(parents) = dep_map.get(name) {
        for &pname in parents.iter() {
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
