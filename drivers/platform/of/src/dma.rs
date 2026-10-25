// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DMA property decoding and conservative structural-parent resolution.

use crate::FdtNode;

/// A firmware DMA window resolved all the way to CPU physical addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedDmaRange {
    /// Device-visible start address.
    pub bus_addr: u64,
    /// CPU physical start address.
    pub cpu_addr: u64,
    /// Window length in bytes.
    pub size: u64,
}

/// Firmware DMA facts, without device mask or platform fallback policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaFacts {
    /// `None` means no usable explicit range, not unlimited DMA permission.
    pub range: Option<ResolvedDmaRange>,
    /// Nearest coherency declaration; `None` requires platform policy.
    pub coherent: Option<bool>,
}

/// A DMA path cannot be represented by the supported direct, single-window model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmaResolveError {
    /// Invalid numeric property or tuple.
    Encoding(DmaRangeError),
    /// Multiple windows need a richer address map.
    MultipleWindows,
    /// A translated range encounters a missing ancestor mapping.
    MissingParentRange,
    /// The complete child window does not fit inside its parent window.
    OutsideParentRange,
    /// IOMMU, interconnect or nonnumeric bus semantics are unsupported here.
    UnsupportedPath,
    /// Conflicting or nonempty boolean coherency properties.
    InvalidCoherency,
}

/// One DMA window between a bus and its immediate parent address space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaRange {
    /// First address visible on the child bus.
    pub bus_addr: u64,
    /// First address in the parent bus, not necessarily a CPU physical address.
    pub parent_addr: u64,
    /// Window length in bytes.
    pub size: u64,
}

/// Invalid or unsupported local DMA property encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmaRangeError {
    /// Only one- and two-cell numeric addresses and sizes are supported.
    UnsupportedCells,
    /// Property length is not a whole number of range tuples.
    TruncatedTuple,
    /// A tuple describes no addresses.
    EmptyWindow,
    /// A bus or parent window extends beyond the 64-bit address space.
    AddressOverflow,
}

/// Local property state; none of these variants establishes a device DMA mask.
#[derive(Debug)]
pub enum DmaRanges<'a> {
    /// No local declaration. The caller must resolve ancestry or platform policy.
    Absent,
    /// An empty property declares identity translation at this bus boundary only.
    Identity,
    /// Explicit windows. Each iterator result must be checked before use.
    Windows(DmaRangeIter<'a>),
}

/// Allocation-free iteration over local DMA windows.
///
/// Address arithmetic is checked per tuple. Callers must reject an invalid tuple
/// rather than silently filtering it out to construct a partial DMA view.
#[derive(Debug)]
pub struct DmaRangeIter<'a> {
    tuples: core::slice::ChunksExact<'a, u8>,
    bus_bytes: usize,
    parent_bytes: usize,
}

fn number(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0, |value, byte| (value << 8) | u64::from(*byte))
}

impl Iterator for DmaRangeIter<'_> {
    type Item = Result<DmaRange, DmaRangeError>;

    fn next(&mut self) -> Option<Self::Item> {
        let tuple = self.tuples.next()?;
        let parent_end = self.bus_bytes + self.parent_bytes;
        let range = DmaRange {
            bus_addr: number(&tuple[..self.bus_bytes]),
            parent_addr: number(&tuple[self.bus_bytes..parent_end]),
            size: number(&tuple[parent_end..]),
        };
        let Some(last_offset) = range.size.checked_sub(1) else {
            return Some(Err(DmaRangeError::EmptyWindow));
        };
        if range.bus_addr.checked_add(last_offset).is_none()
            || range.parent_addr.checked_add(last_offset).is_none()
        {
            return Some(Err(DmaRangeError::AddressOverflow));
        }
        Some(Ok(range))
    }
}

/// Decode a bus node's local `dma-ranges` property.
///
/// Supply the bus node's `#address-cells` and `#size-cells`, and its parent's
/// `#address-cells`. Widths are needed only for a nonempty property. PCI's
/// three-cell address encoding is deliberately unsupported, not truncated.
/// This function does not walk DMA parents, resolve coherency, or translate
/// parent addresses into CPU physical addresses. Missing is distinct from empty.
///
/// # Errors
///
/// Unsupported cell widths and incomplete tuples fail immediately. Zero-size
/// and overflowing windows are reported by the returned iterator.
pub fn parse_dma_ranges(
    property: Option<&[u8]>,
    bus_address_cells: u32,
    parent_address_cells: u32,
    size_cells: u32,
) -> Result<DmaRanges<'_>, DmaRangeError> {
    let Some(bytes) = property else {
        return Ok(DmaRanges::Absent);
    };
    if bytes.is_empty() {
        return Ok(DmaRanges::Identity);
    }
    if ![bus_address_cells, parent_address_cells, size_cells]
        .iter()
        .all(|cells| matches!(cells, 1 | 2))
    {
        return Err(DmaRangeError::UnsupportedCells);
    }
    let bus_bytes = bus_address_cells as usize * 4;
    let parent_bytes = parent_address_cells as usize * 4;
    let tuple_bytes = bus_bytes + parent_bytes + size_cells as usize * 4;
    if !bytes.len().is_multiple_of(tuple_bytes) {
        return Err(DmaRangeError::TruncatedTuple);
    }
    Ok(DmaRanges::Windows(DmaRangeIter {
        tuples: bytes.chunks_exact(tuple_bytes),
        bus_bytes,
        parent_bytes,
    }))
}

fn local_ranges<'a>(node: FdtNode<'_, 'a>) -> Result<DmaRanges<'a>, DmaResolveError> {
    let cells = |name| match node.property(name) {
        Some(property) if property.value.len() == 4 => Ok(number(property.value) as u32),
        _ => Err(DmaResolveError::Encoding(DmaRangeError::UnsupportedCells)),
    };
    let value = node.property("dma-ranges").map(|property| property.value);
    let (bus_cells, parent_cells, size_cells) = if value.is_some_and(|bytes| !bytes.is_empty()) {
        let parent = node.parent().ok_or(DmaResolveError::UnsupportedPath)?;
        let parent_cells = parent
            .property("#address-cells")
            .filter(|property| property.value.len() == 4)
            .ok_or(DmaResolveError::Encoding(DmaRangeError::UnsupportedCells))?;
        (
            cells("#address-cells")?,
            number(parent_cells.value) as u32,
            cells("#size-cells")?,
        )
    } else {
        (0, 0, 0)
    };
    parse_dma_ranges(value, bus_cells, parent_cells, size_cells).map_err(DmaResolveError::Encoding)
}

fn single_range(mut windows: DmaRangeIter<'_>) -> Result<DmaRange, DmaResolveError> {
    let first = windows
        .next()
        .ok_or(DmaResolveError::Encoding(DmaRangeError::EmptyWindow))?
        .map_err(DmaResolveError::Encoding)?;
    if let Some(next) = windows.next() {
        next.map_err(DmaResolveError::Encoding)?;
        return Err(DmaResolveError::MultipleWindows);
    }
    Ok(first)
}

fn coherency(node: FdtNode<'_, '_>) -> Result<Option<bool>, DmaResolveError> {
    let coherent = node.property("dma-coherent");
    let noncoherent = node.property("dma-noncoherent");
    if (coherent.is_some() && noncoherent.is_some())
        || coherent.is_some_and(|property| !property.value.is_empty())
        || noncoherent.is_some_and(|property| !property.value.is_empty())
    {
        return Err(DmaResolveError::InvalidCoherency);
    }
    Ok(coherent.map(|_| true).or(noncoherent.map(|_| false)))
}

/// Resolve a numeric direct-DMA path through structural parents at discovery.
///
/// Follows Linux's separation of DMA ranges and inherited coherency, but only
/// supports a single range at each bus. Missing range information returns
/// `range: None` before any window is found; it never implies identity. Once
/// translation starts, every intermediate bus must declare a range or empty
/// identity property. The structural root terminates physical translation.
/// No platform fallback or device address mask is inferred.
///
/// # Errors
/// Rejects unsupported paths, malformed properties, disjoint windows and child
/// windows not wholly covered by their parents. Requires explicit cell widths
/// for numeric tuples. Discovery-only O(depth * node count), without allocation.
pub fn resolve_dma(node: FdtNode<'_, '_>) -> Result<DmaFacts, DmaResolveError> {
    let mut current = Some(node);
    let mut range: Option<ResolvedDmaRange> = None;
    let mut coherent = None;
    let mut can_inherit_range = true;
    let mut is_device = true;
    while let Some(node) = current {
        if ["iommus", "iommu-map", "interconnects", "interconnect-names"]
            .iter()
            .any(|name| node.property(name).is_some())
            || node
                .property_str("device_type")
                .is_some_and(|kind| matches!(kind, "pci" | "pciex" | "vci" | "ht" | "isa"))
            || node
                .name
                .split('@')
                .next()
                .is_some_and(|name| matches!(name, "isa" | "pcie"))
        {
            return Err(DmaResolveError::UnsupportedPath);
        }
        let local_coherent = coherency(node)?;
        coherent = coherent.or(local_coherent);
        let parent = node.parent();
        if parent.is_none() {
            if node
                .property("dma-ranges")
                .is_some_and(|property| !property.value.is_empty())
            {
                return Err(DmaResolveError::UnsupportedPath);
            }
            break;
        }
        match local_ranges(node)? {
            DmaRanges::Absent if range.is_some() => {
                return Err(DmaResolveError::MissingParentRange);
            }
            DmaRanges::Absent if !is_device => can_inherit_range = false,
            DmaRanges::Windows(windows) => {
                let local = single_range(windows)?;
                if let Some(resolved) = &mut range {
                    let offset = resolved
                        .cpu_addr
                        .checked_sub(local.bus_addr)
                        .ok_or(DmaResolveError::OutsideParentRange)?;
                    if offset >= local.size || resolved.size > local.size - offset {
                        return Err(DmaResolveError::OutsideParentRange);
                    }
                    resolved.cpu_addr = local.parent_addr + offset;
                } else if can_inherit_range {
                    range = Some(ResolvedDmaRange {
                        bus_addr: local.bus_addr,
                        cpu_addr: local.parent_addr,
                        size: local.size,
                    });
                }
            }
            _ => {}
        }
        current = parent;
        is_device = false;
    }
    Ok(DmaFacts { range, coherent })
}

unittest::path_mod_test!(tests, "../ut-tests/dma_unittest.rs");
