// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Numeric DT address translation, without kernel mappings or device ownership.

use alloc::vec::Vec;

use crate::{FdtNode, PropertyError};

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/address.rs");

/// One complete reg interval translated into CPU physical address space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MmioResource<'a> {
    /// CPU physical start address in bytes, independent of host pointer width.
    pub base: u64,
    /// Interval length in bytes.
    pub size: u64,
    /// Optional borrowed reg-names entry; duplicates remain intact.
    pub name: Option<&'a str>,
}

/// Why a numeric address declaration could not be interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressErrorReason {
    /// Missing, malformed or duplicated property.
    Property(PropertyError),
    /// The bus requires a non-numeric address translator.
    UnsupportedBus,
    /// Only one- and two-cell numeric addresses/sizes are supported.
    UnsupportedCells(u32),
    /// A reg declaration has no structural parent.
    MissingParent,
    /// A declared interval has zero size.
    ZeroSize,
    /// Address arithmetic or declared cell width overflowed.
    Overflow,
    /// No ranges window covers the complete resource.
    OutsideRanges,
    /// There are more names than reg tuples.
    ExcessNames,
}

/// An address interpretation error referring only to the input tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressError {
    /// Structure-block offset of the offending device or ancestor node.
    pub node_offset: u32,
    /// Property being interpreted.
    pub property: &'static str,
    /// Tuple/name index, or None for scalar metadata.
    pub index: Option<usize>,
    /// Rejection reason; this is never an IRQ/provider readiness dependency.
    pub reason: AddressErrorReason,
}

/// Collect CPU MMIO intervals for ordinary numeric buses, in reg order.
///
/// Allocates result metadata, not hardware resources. Any error discards
/// the entire provisional result. Cell widths default to 2/1 at each bus;
/// PCI/ISA encodings are unsupported. Missing ranges rejects translation,
/// empty ranges is identity, and each window must contain the whole interval.
/// This stricter interval policy differs from translating only its start.
/// Missing/empty reg yields no resources unless reg-names is present.
/// Structural parent queries scan the tree; this is a discovery operation.
pub fn mmio_resources<'b, 'a: 'b>(
    node: FdtNode<'b, 'a>,
) -> Result<Vec<MmioResource<'a>>, AddressError> {
    let reg = match node.property_strict("reg") {
        Ok(property) => property.value,
        Err(PropertyError::Missing) => &[],
        Err(error) => return Err(address_error(node, "reg", None, error.into())),
    };
    let names = mmio_names(node)?;
    let mut resources = Vec::new();
    if reg.is_empty() {
        if !names.is_empty() {
            return Err(address_error(
                node,
                "reg-names",
                Some(0),
                AddressErrorReason::ExcessNames,
            ));
        }
        return Ok(resources);
    }
    let parent = node
        .parent()
        .ok_or_else(|| address_error(node, "reg", None, AddressErrorReason::MissingParent))?;
    numeric_bus(parent)?;
    let address_bytes = address_cell_bytes(parent, "#address-cells", 2)?;
    let size_bytes = address_cell_bytes(parent, "#size-cells", 1)?;
    let tuple_bytes = address_bytes + size_bytes;
    address_complete_tuples(node, "reg", reg, tuple_bytes)?;
    let count = reg.len() / tuple_bytes;
    if names.len() > count {
        return Err(address_error(
            node,
            "reg-names",
            Some(count),
            AddressErrorReason::ExcessNames,
        ));
    }
    for (index, tuple) in reg.chunks_exact(tuple_bytes).enumerate() {
        let address = number(&tuple[..address_bytes]);
        let size = number(&tuple[address_bytes..]);
        address_interval_end(node, "reg", index, (address, size), address_bytes)?;
        let base = translate_mmio(parent, address, size, index)?;
        resources.push(MmioResource {
            base,
            size,
            name: names.get(index).copied(),
        });
    }
    Ok(resources)
}

fn mmio_names<'b, 'a: 'b>(node: FdtNode<'b, 'a>) -> Result<Vec<&'a str>, AddressError> {
    resource_names(node, "reg-names")
        .map(Iterator::collect)
        .map_err(|(index, reason)| address_error(node, "reg-names", index, reason.into()))
}

fn numeric_bus(node: FdtNode<'_, '_>) -> Result<(), AddressError> {
    if matches!(
        node.name.split('@').next().unwrap_or(""),
        "pci" | "pcie" | "isa"
    ) {
        return Err(address_error(
            node,
            "reg",
            None,
            AddressErrorReason::UnsupportedBus,
        ));
    }
    match node.read_string("device_type") {
        Ok("pci" | "pciex" | "vci" | "ht" | "isa") => Err(address_error(
            node,
            "device_type",
            None,
            AddressErrorReason::UnsupportedBus,
        )),
        Ok(_) | Err(PropertyError::Missing) => Ok(()),
        Err(error) => Err(address_error(node, "device_type", None, error.into())),
    }
}

fn address_cell_bytes(
    node: FdtNode<'_, '_>,
    property: &'static str,
    default: u32,
) -> Result<usize, AddressError> {
    let cells = match node.read_u32(property) {
        Ok(cells) => cells,
        Err(PropertyError::Missing) => default,
        Err(PropertyError::WrongType) => {
            return Err(address_error(
                node,
                property,
                None,
                PropertyError::InvalidEncoding.into(),
            ));
        }
        Err(error) => return Err(address_error(node, property, None, error.into())),
    };
    match cells {
        1 | 2 => Ok(cells as usize * 4),
        _ => Err(address_error(
            node,
            property,
            None,
            AddressErrorReason::UnsupportedCells(cells),
        )),
    }
}

fn address_complete_tuples(
    node: FdtNode<'_, '_>,
    property: &'static str,
    bytes: &[u8],
    tuple_bytes: usize,
) -> Result<(), AddressError> {
    if !bytes.len().is_multiple_of(tuple_bytes) {
        return Err(address_error(
            node,
            property,
            Some(bytes.len() / tuple_bytes),
            PropertyError::InvalidEncoding.into(),
        ));
    }
    Ok(())
}

fn address_interval_end(
    node: FdtNode<'_, '_>,
    property: &'static str,
    index: usize,
    interval: (u64, u64),
    address_bytes: usize,
) -> Result<u64, AddressError> {
    let (base, size) = interval;
    if size == 0 {
        return Err(address_error(
            node,
            property,
            Some(index),
            AddressErrorReason::ZeroSize,
        ));
    }
    base.checked_add(size - 1)
        .filter(|end| address_bytes == 8 || *end <= u64::from(u32::MAX))
        .ok_or_else(|| address_error(node, property, Some(index), AddressErrorReason::Overflow))
}

fn translate_mmio(
    mut node: FdtNode<'_, '_>,
    mut address: u64,
    size: u64,
    reg_index: usize,
) -> Result<u64, AddressError> {
    while let Some(parent) = node.parent() {
        numeric_bus(parent)?;
        let child_bytes = address_cell_bytes(node, "#address-cells", 2)?;
        let parent_bytes = address_cell_bytes(parent, "#address-cells", 2)?;
        let size_bytes = address_cell_bytes(node, "#size-cells", 1)?;
        let ranges = node
            .property_strict("ranges")
            .map_err(|error| address_error(node, "ranges", None, error.into()))?
            .value;
        if ranges.is_empty() {
            address_interval_end(node, "ranges", reg_index, (address, size), parent_bytes)?;
        } else {
            address = translate_mmio_ranges(
                node,
                ranges,
                (child_bytes, parent_bytes, size_bytes),
                address,
                size,
                reg_index,
            )?;
        }
        node = parent;
    }
    Ok(address)
}

fn translate_mmio_ranges(
    node: FdtNode<'_, '_>,
    ranges: &[u8],
    widths: (usize, usize, usize),
    address: u64,
    size: u64,
    reg_index: usize,
) -> Result<u64, AddressError> {
    let (child_bytes, parent_bytes, size_bytes) = widths;
    let tuple_bytes = child_bytes + parent_bytes + size_bytes;
    address_complete_tuples(node, "ranges", ranges, tuple_bytes)?;
    let mut translated = None;
    for (index, tuple) in ranges.chunks_exact(tuple_bytes).enumerate() {
        let child = number(&tuple[..child_bytes]);
        let parent = number(&tuple[child_bytes..child_bytes + parent_bytes]);
        let length = number(&tuple[child_bytes + parent_bytes..]);
        address_interval_end(node, "ranges", index, (child, length), child_bytes)?;
        address_interval_end(node, "ranges", index, (parent, length), parent_bytes)?;
        if let Some(offset) = address.checked_sub(child)
            && size <= length
            && offset <= length - size
            && translated.is_none()
        {
            translated = parent.checked_add(offset);
        }
    }
    translated.ok_or_else(|| {
        address_error(
            node,
            "ranges",
            Some(reg_index),
            AddressErrorReason::OutsideRanges,
        )
    })
}

fn address_error(
    node: FdtNode<'_, '_>,
    property: &'static str,
    index: Option<usize>,
    reason: AddressErrorReason,
) -> AddressError {
    AddressError {
        node_offset: node.offset(),
        property,
        index,
        reason,
    }
}

impl From<PropertyError> for AddressErrorReason {
    fn from(error: PropertyError) -> Self {
        Self::Property(error)
    }
}

fn number(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0, |value, byte| (value << 8) | u64::from(*byte))
}

fn resource_names<'b, 'a: 'b>(
    node: FdtNode<'b, 'a>,
    property: &str,
) -> Result<impl Iterator<Item = &'a str> + Clone, (Option<usize>, PropertyError)> {
    let bytes = match node.property_strict(property) {
        Ok(property) => property.value,
        Err(PropertyError::Missing) => return Ok("".split_terminator('\0')),
        Err(reason) => return Err((None, reason)),
    };
    for (index, part) in bytes.split_inclusive(|byte| *byte == 0).enumerate() {
        part.strip_suffix(&[0])
            .and_then(|part| core::str::from_utf8(part).ok())
            .filter(|name| !name.is_empty())
            .ok_or((Some(index), PropertyError::InvalidEncoding))?;
    }
    if bytes.is_empty() {
        return Err((Some(0), PropertyError::InvalidEncoding));
    }
    let text = core::str::from_utf8(bytes).map_err(|_| (None, PropertyError::InvalidEncoding))?;
    Ok(text.split_terminator('\0'))
}
