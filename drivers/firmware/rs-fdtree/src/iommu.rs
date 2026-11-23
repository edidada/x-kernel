// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `iommus` property parsing.
//!
//! A device controlled by one or more IOMMUs declares
//! `iommus = <&iommu-node [specifier cells]>...`, where the cell count comes
//! from the target's `#iommu-cells` (0 for Rockchip media IOMMUs, 1 for
//! SMMU Stream IDs). The specifiers are passed through verbatim: interpreting
//! them is the IOMMU driver's job, exactly like interrupt cells.

use crate::{
    FdtNode,
    property::{Cells, PropertyError},
    reference::Phandles,
};

/// Why `iommus` cannot be interpreted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IommuSpecError {
    /// The property payload is malformed.
    Property(PropertyError),
    /// A phandle does not name a node in this tree.
    UnresolvedPhandle(u32),
    /// The target node does not declare `#iommu-cells`.
    MissingIommuCells,
    /// The target's `#iommu-cells` is not a valid count.
    InvalidCellCount,
}

impl From<PropertyError> for IommuSpecError {
    fn from(error: PropertyError) -> Self {
        Self::Property(error)
    }
}

/// One `iommus` entry: the IOMMU node and the device's specifier cells.
#[derive(Clone, Copy, Debug)]
pub struct DeviceIommuSpec<'b, 'a> {
    /// IOMMU controller node.
    pub controller: FdtNode<'b, 'a>,
    /// Specifier cells after the phandle, in firmware order.
    pub params: Cells<'a>,
}

impl<'b, 'a: 'b> FdtNode<'b, 'a> {
    /// Parses every `iommus` entry on this node.
    ///
    /// The tuple width is read per target from its `#iommu-cells`, so a list
    /// mixing zero-cell (Rockchip) and one-cell (SMMU) entries parses
    /// correctly. `Ok(None)` means the node declares no `iommus` property.
    ///
    /// # Errors
    ///
    /// Returns [`IommuSpecError`] for a malformed payload, dangling phandle
    /// or a target without a usable `#iommu-cells`.
    #[cfg(feature = "alloc")]
    pub fn iommu_specs(
        self,
    ) -> Result<Option<alloc::vec::Vec<DeviceIommuSpec<'b, 'a>>>, IommuSpecError> {
        self.iommu_specs_resolved(Phandles::Linear(self.as_tree()))
    }

    /// [`Self::iommu_specs`] with a prebuilt phandle index.
    #[cfg(feature = "alloc")]
    pub fn iommu_specs_with<'i>(
        self,
        phandles: &'i crate::index::NodeIndex<'a>,
    ) -> Result<Option<alloc::vec::Vec<DeviceIommuSpec<'i, 'a>>>, IommuSpecError> {
        self.iommu_specs_resolved(Phandles::Indexed(phandles))
    }

    #[cfg(feature = "alloc")]
    fn iommu_specs_resolved<'r>(
        self,
        phandles: Phandles<'r, 'a>,
    ) -> Result<Option<alloc::vec::Vec<DeviceIommuSpec<'r, 'a>>>, IommuSpecError> {
        let Some(property) = self.property("iommus") else {
            return Ok(None);
        };
        if property.value.is_empty() {
            return Ok(None);
        }
        let cells = Cells::from_bytes(property.value)?;
        let mut specs = alloc::vec::Vec::new();
        let mut cursor = 0usize;
        while cursor < cells.len() {
            let Some(phandle) = cells.get(cursor) else {
                return Err(IommuSpecError::Property(PropertyError::InvalidEncoding));
            };
            let controller = phandles
                .resolve(phandle)
                .ok_or(IommuSpecError::UnresolvedPhandle(phandle))?;
            let count = controller
                .property_u32("#iommu-cells")
                .ok_or(IommuSpecError::MissingIommuCells)?;
            let count = usize::try_from(count).map_err(|_| IommuSpecError::InvalidCellCount)?;
            let start = cursor + 1;
            let Some(params) = cells.slice(start, count) else {
                return Err(IommuSpecError::Property(PropertyError::InvalidEncoding));
            };
            specs.push(DeviceIommuSpec { controller, params });
            cursor = start + count;
        }
        Ok(Some(specs))
    }
}
