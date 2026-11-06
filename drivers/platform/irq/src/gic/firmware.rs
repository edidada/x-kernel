// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! GIC device-tree selection and binding-specific configuration decoding.

use memaddr::PhysAddr;

use super::{GicConfig, GicMmioRegion, GicVersion};

pub(super) fn is_gicv2_compatible(compatible: &str) -> bool {
    matches!(
        compatible,
        "arm,gic-400"
            | "arm,cortex-a15-gic"
            | "arm,cortex-a9-gic"
            | "arm,cortex-a7-gic"
            | "arm,gic-v2"
    )
}

pub(super) fn is_gicv3_compatible(compatible: &str) -> bool {
    matches!(compatible, "arm,gic-v3" | "arm,gic-v4")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DiscoveryError {
    Status {
        node: u32,
        reason: rs_fdtree::PropertyError,
    },
    Registers(rs_fdtree::AddressError),
    MissingRegion {
        node: u32,
    },
    AddressWidth {
        node: u32,
    },
}

pub(super) fn config_from_device_tree(
    tree: &rs_fdtree::LinuxFdt<'_>,
) -> Result<Option<(u32, GicConfig)>, DiscoveryError> {
    for node in tree.all_nodes() {
        if node.property("interrupt-controller").is_none() {
            continue;
        }
        let version = if node.compatibles().any(is_gicv3_compatible) {
            GicVersion::V3
        } else if node.compatibles().any(is_gicv2_compatible) {
            GicVersion::V2
        } else {
            continue;
        };
        let offset = node.offset();
        if !node
            .is_available()
            .map_err(|reason| DiscoveryError::Status {
                node: offset,
                reason,
            })?
        {
            continue;
        }
        let mut regions = node
            .mmio_resources()
            .map_err(DiscoveryError::Registers)?
            .into_iter()
            .map(|region| {
                let overflow = DiscoveryError::AddressWidth { node: offset };
                let base = usize::try_from(region.base).map_err(|_| overflow)?;
                let size = usize::try_from(region.size).map_err(|_| overflow)?;
                base.checked_add(size).ok_or(overflow)?;
                Ok(GicMmioRegion {
                    paddr: PhysAddr::from_usize(base),
                    size,
                })
            });
        let missing = DiscoveryError::MissingRegion { node: offset };
        let gicd = regions.next().ok_or(missing)??;
        let interface = regions.next().ok_or(missing)??;
        let config = match version {
            GicVersion::V2 => GicConfig {
                version,
                gicd,
                gicc: Some(interface),
                gicr: None,
                gich: regions.next().transpose()?,
                gicv: regions.next().transpose()?,
            },
            GicVersion::V3 => GicConfig {
                version,
                gicd,
                gicc: None,
                gicr: Some(interface),
                gich: None,
                gicv: None,
            },
        };
        for region in regions {
            region?;
        }
        return Ok(Some((offset, config)));
    }
    Ok(None)
}
