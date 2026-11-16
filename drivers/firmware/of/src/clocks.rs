// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device-tree clock binding parsing over the fwnode contract.
//!
//! Consumers resolve their `clocks` / `clock-names` pair to
//! `(provider, index)` pairs here; clock providers read their own
//! `#clock-cells`, `clock-output-names` and `clock-frequency` declarations.
//! Everything is expressed against the backend-neutral [`FwNode`], so a
//! synthetic or ACPI-derived backend can answer the same questions without
//! device-tree code.
//!
//! Known limitation: the referenced nodes' `#clock-cells` must be uniform
//! across one `clocks` list and at most one cell wide. This covers every
//! in-tree provider (fixed-clock declares zero cells; SoC controllers
//! declare one) and matches the clock framework's documented scope.

use fwnode::{FwError, FwNode};

/// One resolved clock connection: provider identity plus provider-local index.
///
/// `provider_id` is the referenced node's firmware identity
/// ([`FwNodeId::raw`]); the clock framework keys its provider registry on the
/// same value, so a consumer only needs this pair to reach the provider.
///
/// [`FwNodeId::raw`]: fwnode::FwNodeId::raw
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockSpec {
    /// Firmware identity of the provider node.
    pub provider_id: u32,
    /// Provider-local clock index (zero when the provider declares
    /// `#clock-cells = <0>`).
    pub index: u32,
}

/// Reads the node's own `#clock-cells` declaration.
///
/// `None` means the node does not declare itself a clock provider.
pub fn cells(node: &FwNode<'_>) -> Result<Option<u32>, FwError> {
    match node.property("#clock-cells")? {
        Some(property) => property.as_u32().map(Some),
        None => Ok(None),
    }
}

/// Reads the node's `clock-frequency` declaration, if present.
///
/// Both the 64-bit and the 32-bit cell encoding are accepted.
pub fn frequency(node: &FwNode<'_>) -> Option<u64> {
    let property = node.property("clock-frequency").ok()??;
    property.as_u64().ok()
}

/// Reads the node's `clock-output-names` entry `index`, if declared.
pub fn output_name<'a>(node: &FwNode<'a>, index: usize) -> Option<&'a str> {
    let property = node.property("clock-output-names").ok()??;
    property.as_str_list().ok()?.nth(index)
}

/// Resolves connection `index` of the consumer's `clocks` list.
///
/// `Ok(None)` means the consumer declares no `clocks` list or the index is
/// out of range. The referenced node's `#clock-cells` is read from the first
/// entry and assumed uniform across the list; a mixed-cell list is rejected.
pub fn resolve_by_index(consumer: &FwNode<'_>, index: usize) -> Result<Option<ClockSpec>, FwError> {
    // Entry 0 with zero argument cells is always the first phandle, so the
    // first target can be inspected before the entry width is known.
    let Some(first) = consumer.reference("clocks", 0, 0)? else {
        return Ok(None);
    };
    let width = cells(&first.node)?.unwrap_or(0);
    if width > 1 {
        return Err(FwError::InvalidValue);
    }
    let Some(reference) = consumer.reference("clocks", index, width as usize)? else {
        return Ok(None);
    };
    let target_cells = cells(&reference.node)?.unwrap_or(0);
    if target_cells != width {
        return Err(FwError::InvalidValue);
    }
    let index = if width == 1 {
        reference
            .args
            .as_slice()
            .first()
            .copied()
            .ok_or(FwError::InvalidValue)?
    } else {
        0
    };
    Ok(Some(ClockSpec {
        provider_id: reference.node.id().raw(),
        index,
    }))
}

/// Resolves the consumer's `clock-names` entry `name` to its `clocks`
/// position, then delegates to [`resolve_by_index`].
///
/// `clock-names` is consumer-local: the name selects the position in the
/// consumer's own `clocks` list, not a provider-side output name.
pub fn resolve_by_name(consumer: &FwNode<'_>, name: &str) -> Result<Option<ClockSpec>, FwError> {
    let Some(property) = consumer.property("clock-names")? else {
        return Ok(None);
    };
    for (index, candidate) in property.as_str_list()?.enumerate() {
        if candidate == name {
            return resolve_by_index(consumer, index);
        }
    }
    Ok(None)
}
