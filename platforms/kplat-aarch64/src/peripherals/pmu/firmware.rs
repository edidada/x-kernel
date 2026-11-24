// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! PMU firmware topology: CPU-to-PMU-interrupt association.
//!
//! This is source-owner policy, not a KIRQ registry. The parser uses only the
//! neutral firmware-node IRQ/reference primitives and returns a boot-time plan;
//! it performs no mapping, affinity write, registration or enable.

use fwnode::{FwError, FwNode};
use kcpu_id_map::{LogicalCpuId, RawCpuId, logical_cpu_id};

/// One validated PMU SPI association.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PmuSpiRoute {
    pub(crate) hwirq: u32,
    pub(crate) cpu: LogicalCpuId,
}

/// Fixed-capacity boot-time PMU SPI plan; no heap and no runtime cache.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PmuSpiRoutes {
    routes: [PmuSpiRoute; kbuild_config::NR_CPUS],
    len: usize,
}

impl PmuSpiRoutes {
    const fn new() -> Self {
        Self {
            routes: [PmuSpiRoute {
                hwirq: 0,
                cpu: LogicalCpuId::new(0),
            }; kbuild_config::NR_CPUS],
            len: 0,
        }
    }

    fn push(&mut self, route: PmuSpiRoute) -> bool {
        if self.len >= self.routes.len() {
            return false;
        }
        self.routes[self.len] = route;
        self.len += 1;
        true
    }

    pub(crate) fn as_slice(&self) -> &[PmuSpiRoute] {
        &self.routes[..self.len]
    }
}

/// Firmware-shape result: absence, the existing PPI path, SPI routes, or a
/// closed failure. No variant silently chooses the first IRQ.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PmuIrqPlan {
    Absent,
    Ppi { irq: u32 },
    Spi(PmuSpiRoutes),
    Malformed(&'static str),
    Unsupported(&'static str),
}

#[derive(Clone, Copy)]
struct IrqCell {
    kind: u32,
    irq: u32,
    _flags: u32,
}

/// Resolves the authoritative firmware root and parses its PMU node. The root
/// comes from `khal::firmware::fwnode_root`, the same published source used by
/// GIC discovery; PMU does not construct a second DT backend/root entry.
pub(crate) fn parse_pmu_firmware() -> PmuIrqPlan {
    let Some(root) = khal::firmware::fwnode_root() else {
        return PmuIrqPlan::Absent;
    };
    parse_pmu_plan(root)
}

/// Parses the PMU node under `root`.
///
/// The caller supplies the firmware root; this function performs no global
/// lookup and no side effect. A matching PMU node must have a valid complete
/// IRQ list; multi-CPU SPI requires an explicit `interrupt-affinity` list with
/// the same length and one distinct CPU per entry.
pub(crate) fn parse_pmu_plan(root: FwNode<'_>) -> PmuIrqPlan {
    let pmu = match find_pmu(root) {
        Ok(Some(pmu)) => pmu,
        Ok(None) => return PmuIrqPlan::Absent,
        Err(plan) => return plan,
    };
    let (cells, cell_count) = match parse_irq_cells(pmu) {
        Ok(value) => value,
        Err(plan) => return plan,
    };
    if cell_count == 0 {
        return PmuIrqPlan::Absent;
    }

    let has_spi = cells[..cell_count].iter().any(|cell| cell.kind == 0);
    let has_ppi = cells[..cell_count].iter().any(|cell| cell.kind == 1);
    if has_spi && has_ppi {
        return PmuIrqPlan::Unsupported("mixed PPI/SPI PMU");
    }
    if has_ppi {
        if cell_count != 1 {
            return PmuIrqPlan::Unsupported("multiple PMU PPIs");
        }
        return PmuIrqPlan::Ppi { irq: cells[0].irq };
    }

    let (affinity, affinity_count) = match parse_affinity(pmu) {
        Ok(value) => value,
        Err(plan) => return plan,
    };
    spi_routes(&cells[..cell_count], &affinity[..affinity_count])
}

fn find_pmu(root: FwNode<'_>) -> Result<Option<FwNode<'_>>, PmuIrqPlan> {
    let mut pmu = None;
    for node in root.descendants() {
        if !node.firmware_ids().any(is_pmu_compatible) {
            continue;
        }
        match node.is_available() {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => return Err(PmuIrqPlan::Malformed("PMU availability")),
        }
        if pmu.is_some() {
            return Err(PmuIrqPlan::Unsupported("multiple PMU nodes"));
        }
        pmu = Some(node);
    }
    Ok(pmu)
}

fn parse_irq_cells(
    pmu: FwNode<'_>,
) -> Result<([IrqCell; kbuild_config::NR_CPUS], usize), PmuIrqPlan> {
    let mut cells = [IrqCell {
        kind: 0,
        irq: 0,
        _flags: 0,
    }; kbuild_config::NR_CPUS];
    let mut count = 0;
    loop {
        let spec = match pmu.irq_spec(count) {
            Ok(Some(spec)) => spec,
            Ok(None) => break,
            Err(FwError::Unsupported) => return Err(PmuIrqPlan::Unsupported("unreadable PMU IRQ")),
            Err(_) => return Err(PmuIrqPlan::Malformed("invalid PMU IRQ")),
        };
        if count >= kbuild_config::NR_CPUS {
            return Err(PmuIrqPlan::Malformed("too many PMU IRQs"));
        }
        if !spec.controller.firmware_ids().any(is_gic_compatible) {
            return Err(PmuIrqPlan::Unsupported("non-GIC PMU IRQ"));
        }
        let [kind, irq, flags] = spec.params.as_slice() else {
            return Err(PmuIrqPlan::Malformed("PMU IRQ cells"));
        };
        let hwirq = match *kind {
            0 => 32u32.checked_add(*irq),
            1 => 16u32.checked_add(*irq),
            _ => None,
        };
        let Some(hwirq) = hwirq else {
            return Err(PmuIrqPlan::Malformed("PMU IRQ number"));
        };
        cells[count] = IrqCell {
            kind: *kind,
            irq: hwirq,
            _flags: *flags,
        };
        count += 1;
    }
    Ok((cells, count))
}

fn parse_affinity(
    pmu: FwNode<'_>,
) -> Result<([LogicalCpuId; kbuild_config::NR_CPUS], usize), PmuIrqPlan> {
    let mut affinity = [LogicalCpuId::new(0); kbuild_config::NR_CPUS];
    let mut count = 0;
    loop {
        let reference = match pmu.reference("interrupt-affinity", count, 0) {
            Ok(Some(reference)) => reference,
            Ok(None) => break,
            Err(_) => return Err(PmuIrqPlan::Malformed("interrupt-affinity reference")),
        };
        if count >= kbuild_config::NR_CPUS {
            return Err(PmuIrqPlan::Malformed("too many interrupt-affinity entries"));
        }
        let Some(raw) = reference.node.hardware_cpu_id().ok().flatten() else {
            return Err(PmuIrqPlan::Malformed("interrupt-affinity CPU identity"));
        };
        let Ok(raw) = usize::try_from(raw) else {
            return Err(PmuIrqPlan::Malformed("interrupt-affinity CPU width"));
        };
        let Some(cpu) = logical_cpu_id(RawCpuId::new(raw)) else {
            return Err(PmuIrqPlan::Malformed("interrupt-affinity CPU not present"));
        };
        if affinity[..count].contains(&cpu) {
            return Err(PmuIrqPlan::Malformed("duplicate interrupt-affinity CPU"));
        }
        affinity[count] = cpu;
        count += 1;
    }
    Ok((affinity, count))
}

fn spi_routes(cells: &[IrqCell], affinity: &[LogicalCpuId]) -> PmuIrqPlan {
    if affinity.len() != cells.len() {
        if cells.len() == 1 && affinity.is_empty() {
            // Linux assigns a lone SPI without interrupt-affinity to logical
            // CPU 0 (with a warning) instead of losing PMU service. Multi-entry
            // lists still require explicit owner association.
            let mut routes = PmuSpiRoutes::new();
            if !routes.push(PmuSpiRoute {
                hwirq: cells[0].irq,
                cpu: LogicalCpuId::new(0),
            }) {
                return PmuIrqPlan::Malformed("SPI route capacity");
            }
            return PmuIrqPlan::Spi(routes);
        }
        return PmuIrqPlan::Malformed("PMU IRQ/affinity count mismatch");
    }

    let mut routes = PmuSpiRoutes::new();
    for (cell, cpu) in cells.iter().zip(affinity) {
        if !routes.push(PmuSpiRoute {
            hwirq: cell.irq,
            cpu: *cpu,
        }) {
            return PmuIrqPlan::Malformed("SPI route capacity");
        }
    }
    PmuIrqPlan::Spi(routes)
}

fn is_pmu_compatible(compatible: &str) -> bool {
    compatible == "arm,armv8-pmuv3"
        || (compatible.starts_with("arm,cortex-") && compatible.ends_with("-pmu"))
        || (compatible.starts_with("arm,neoverse-") && compatible.ends_with("-pmu"))
        || (compatible.starts_with("arm,c1-") && compatible.ends_with("-pmu"))
}

fn is_gic_compatible(compatible: &str) -> bool {
    matches!(
        compatible,
        "arm,gic-400" | "arm,cortex-a15-gic" | "arm,cortex-a7-gic" | "arm,gic-v2" | "arm,gic-v3"
    )
}
