// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! GIC device-tree selection and binding-specific register configuration.

use memaddr::PhysAddr;

use super::{GicConfig, GicMmioRegion, GicVersion};

fn is_gicv2_compatible(compatible: &str) -> bool {
    matches!(
        compatible,
        "arm,gic-400"
            | "arm,cortex-a15-gic"
            | "arm,cortex-a9-gic"
            | "arm,cortex-a7-gic"
            | "arm,gic-v2"
    )
}

fn is_gicv3_compatible(compatible: &str) -> bool {
    matches!(compatible, "arm,gic-v3" | "arm,gic-v4")
}

pub(super) fn config_from_device_tree() -> Option<GicConfig> {
    let node = of::fdt()?.all_nodes().find(|node| {
        node.property("interrupt-controller").is_some()
            && node.compatibles().any(|compatible| {
                is_gicv2_compatible(compatible) || is_gicv3_compatible(compatible)
            })
    })?;

    let version = if node.compatibles().any(is_gicv3_compatible) {
        GicVersion::V3
    } else if node.compatibles().any(is_gicv2_compatible) {
        GicVersion::V2
    } else {
        return None;
    };

    let mut regs = node.reg()?;
    let gicd = regs.next()?;
    let gicd = GicMmioRegion {
        paddr: PhysAddr::from_usize(gicd.starting_address as usize),
        size: gicd.size,
    };

    match version {
        GicVersion::V2 => {
            let gicc = regs.next()?;
            let gich = regs.next().map(|reg| GicMmioRegion {
                paddr: PhysAddr::from_usize(reg.starting_address as usize),
                size: reg.size,
            });
            let gicv = regs.next().map(|reg| GicMmioRegion {
                paddr: PhysAddr::from_usize(reg.starting_address as usize),
                size: reg.size,
            });
            Some(GicConfig {
                version,
                gicd,
                gicc: Some(GicMmioRegion {
                    paddr: PhysAddr::from_usize(gicc.starting_address as usize),
                    size: gicc.size,
                }),
                gicr: None,
                gich,
                gicv,
            })
        }
        GicVersion::V3 => {
            let gicr = regs.next()?;
            Some(GicConfig {
                version,
                gicd,
                gicc: None,
                gich: None,
                gicv: None,
                gicr: Some(GicMmioRegion {
                    paddr: PhysAddr::from_usize(gicr.starting_address as usize),
                    size: gicr.size,
                }),
            })
        }
    }
}
