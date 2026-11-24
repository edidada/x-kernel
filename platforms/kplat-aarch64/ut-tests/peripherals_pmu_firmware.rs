// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Deterministic PMU firmware association fixtures.

use fwnode::{
    FwDmaError, FwError, FwIrqSpec, FwNode, FwNodeId, FwNodeOps, FwParams, FwProperty, FwReference,
    FwSource,
};
use kcpu_id_map::{LogicalCpuId, raw_cpu_id};
use unittest::{TestResult, assert, assert_eq, def_test};

use super::firmware::{PmuIrqPlan, PmuSpiRoute, parse_pmu_plan};

const ROOT: u32 = 0;
const CPUS: u32 = 1;
const CPU_BASE: u32 = 2;
const PMU0: u32 = 10;
const PMU1: u32 = 11;
const GIC: u32 = 12;

#[derive(Clone, Copy)]
struct IrqCell {
    kind: u32,
    irq: u32,
    flags: u32,
}

struct Fixture {
    cpu_raw: [u64; 4],
    irqs: &'static [IrqCell],
    affinity: &'static [u32],
    pmu_present: bool,
    second_pmu: bool,
}

impl Fixture {
    fn new(irqs: &'static [IrqCell], affinity: &'static [u32]) -> Self {
        Self {
            cpu_raw: core::array::from_fn(|cpu| {
                raw_cpu_id(LogicalCpuId::new(cpu)).unwrap().as_usize() as u64
            }),
            irqs,
            affinity,
            pmu_present: true,
            second_pmu: false,
        }
    }

    fn root(&self) -> FwNode<'_> {
        FwNode::new(self, FwNodeId::new(ROOT))
    }
}

impl<'a> FwNodeOps<'a> for Fixture {
    fn source(&self) -> FwSource {
        FwSource::DeviceTree
    }

    fn name(&self, id: FwNodeId) -> Option<&'a str> {
        match id.raw() {
            ROOT => Some(""),
            CPUS => Some("cpus"),
            CPU_BASE..=5 => Some("cpu"),
            PMU0 | PMU1 => Some("pmu"),
            GIC => Some("intc"),
            _ => None,
        }
    }

    fn parent(&self, id: FwNodeId) -> Option<FwNodeId> {
        match id.raw() {
            ROOT => None,
            CPUS | PMU0 | PMU1 | GIC => Some(FwNodeId::new(ROOT)),
            CPU_BASE..=5 => Some(FwNodeId::new(CPUS)),
            _ => None,
        }
    }

    fn first_child(&self, id: FwNodeId) -> Option<FwNodeId> {
        match id.raw() {
            ROOT => Some(FwNodeId::new(CPUS)),
            CPUS => Some(FwNodeId::new(CPU_BASE)),
            _ => None,
        }
    }

    fn next_sibling(&self, id: FwNodeId) -> Option<FwNodeId> {
        match id.raw() {
            CPUS if self.pmu_present => Some(FwNodeId::new(PMU0)),
            PMU0 if self.second_pmu => Some(FwNodeId::new(PMU1)),
            CPU_BASE..=4 => Some(FwNodeId::new(id.raw() + 1)),
            _ => None,
        }
    }

    fn is_available(&self, id: FwNodeId) -> Result<bool, FwError> {
        match id.raw() {
            ROOT | CPUS | CPU_BASE..=5 | PMU0 | PMU1 | GIC => Ok(true),
            _ => Err(FwError::InvalidNode),
        }
    }

    fn is_interrupt_controller(&self, id: FwNodeId) -> Result<bool, FwError> {
        Ok(id.raw() == GIC)
    }

    fn firmware_id(&self, id: FwNodeId, index: usize) -> Option<&'a str> {
        if index != 0 {
            return None;
        }
        match id.raw() {
            ROOT => Some(""),
            CPUS => Some("simple-bus"),
            CPU_BASE..=5 => Some("arm,cortex-a55"),
            PMU0 | PMU1 => Some("arm,armv8-pmuv3"),
            GIC => Some("arm,gic-400"),
            _ => None,
        }
    }

    fn property(&self, _id: FwNodeId, _name: &str) -> Result<Option<FwProperty<'a>>, FwError> {
        Ok(None)
    }

    fn hardware_cpu_id(&self, id: FwNodeId) -> Result<Option<u64>, FwError> {
        match id.raw() {
            CPU_BASE..=5 => Ok(Some(self.cpu_raw[(id.raw() - CPU_BASE) as usize])),
            ROOT | CPUS | PMU0 | PMU1 | GIC => Ok(None),
            _ => Err(FwError::InvalidNode),
        }
    }

    fn mmio_resource(
        &self,
        _id: FwNodeId,
        _index: usize,
    ) -> Result<Option<fwnode::FwMmioResource<'a>>, FwError> {
        Ok(None)
    }

    fn irq_spec(&'a self, id: FwNodeId, index: usize) -> Result<Option<FwIrqSpec<'a>>, FwError> {
        if !matches!(id.raw(), PMU0 | PMU1) {
            return Ok(None);
        }
        let Some(cell) = self.irqs.get(index) else {
            return Ok(None);
        };
        let params =
            FwParams::new(&[cell.kind, cell.irq, cell.flags]).map_err(|_| FwError::InvalidValue)?;
        Ok(Some(FwIrqSpec {
            controller: FwNode::new(self, FwNodeId::new(GIC)),
            params,
        }))
    }

    fn reference(
        &'a self,
        id: FwNodeId,
        property: &str,
        index: usize,
        arg_cells: usize,
    ) -> Result<Option<FwReference<'a>>, FwError> {
        if !matches!(id.raw(), PMU0 | PMU1) || property != "interrupt-affinity" || arg_cells != 0 {
            return Ok(None);
        }
        let Some(cpu) = self.affinity.get(index) else {
            return Ok(None);
        };
        let args = FwParams::empty();
        Ok(Some(FwReference {
            node: FwNode::new(self, FwNodeId::new(CPU_BASE + cpu)),
            args,
        }))
    }

    fn dma_ranges(&self, _id: FwNodeId) -> Result<fwnode::FwDmaRanges, FwError> {
        Err(FwError::Unsupported)
    }

    fn dma_range(
        &self,
        _id: FwNodeId,
        _index: usize,
    ) -> Result<Option<fwnode::FwDmaRange>, FwError> {
        Err(FwError::Unsupported)
    }

    fn dma_coherency(&self, _id: FwNodeId) -> Result<Option<bool>, FwError> {
        Err(FwError::Unsupported)
    }

    fn dma_window(&self, _id: FwNodeId) -> Result<Option<fwnode::FwDmaWindow>, FwDmaError> {
        Err(FwDmaError::Firmware(FwError::Unsupported))
    }

    fn dma_effective_coherency(&self, _id: FwNodeId) -> Result<Option<bool>, FwError> {
        Err(FwError::Unsupported)
    }
}

fn plan(fixture: &Fixture) -> PmuIrqPlan {
    parse_pmu_plan(fixture.root())
}

static SPI_FOUR: [IrqCell; 4] = [
    IrqCell {
        kind: 0,
        irq: 4,
        flags: 4,
    },
    IrqCell {
        kind: 0,
        irq: 5,
        flags: 4,
    },
    IrqCell {
        kind: 0,
        irq: 6,
        flags: 4,
    },
    IrqCell {
        kind: 0,
        irq: 7,
        flags: 4,
    },
];
static AFFINITY_PERMUTED: [u32; 4] = [2, 0, 3, 1];

#[def_test]
fn permuted_spi_affinity_keeps_irq_index_association() -> TestResult {
    let fixture = Fixture::new(&SPI_FOUR, &AFFINITY_PERMUTED);
    let PmuIrqPlan::Spi(routes) = plan(&fixture) else {
        panic!("expected SPI plan");
    };
    assert_eq!(
        routes.as_slice(),
        &[
            PmuSpiRoute {
                hwirq: 36,
                cpu: LogicalCpuId::new(2)
            },
            PmuSpiRoute {
                hwirq: 37,
                cpu: LogicalCpuId::new(0)
            },
            PmuSpiRoute {
                hwirq: 38,
                cpu: LogicalCpuId::new(3)
            },
            PmuSpiRoute {
                hwirq: 39,
                cpu: LogicalCpuId::new(1)
            },
        ]
    );
    TestResult::Ok
}

#[def_test]
fn ppi_shape_accepts_an_affinity_list() -> TestResult {
    static PPI: [IrqCell; 1] = [IrqCell {
        kind: 1,
        irq: 7,
        flags: 4,
    }];
    static AFFINITY: [u32; 4] = [0, 1, 2, 3];
    let fixture = Fixture::new(&PPI, &AFFINITY);
    assert_eq!(plan(&fixture), PmuIrqPlan::Ppi { irq: 23 });
    TestResult::Ok
}

#[def_test]
fn mixed_ppi_spi_is_unsupported_not_malformed() -> TestResult {
    static MIXED: [IrqCell; 2] = [
        IrqCell {
            kind: 0,
            irq: 4,
            flags: 4,
        },
        IrqCell {
            kind: 1,
            irq: 7,
            flags: 4,
        },
    ];
    assert_eq!(
        plan(&Fixture::new(&MIXED, &[])),
        PmuIrqPlan::Unsupported("mixed PPI/SPI PMU")
    );
    TestResult::Ok
}

#[def_test]
fn malformed_count_duplicate_and_absent_are_distinct() -> TestResult {
    static TWO_SPI: [IrqCell; 2] = [
        IrqCell {
            kind: 0,
            irq: 4,
            flags: 4,
        },
        IrqCell {
            kind: 0,
            irq: 5,
            flags: 4,
        },
    ];
    static ONE_AFFINITY: [u32; 1] = [0];
    assert!(matches!(
        plan(&Fixture::new(&TWO_SPI, &ONE_AFFINITY)),
        PmuIrqPlan::Malformed(_)
    ));

    static DUPLICATE: [u32; 2] = [0, 0];
    assert!(matches!(
        plan(&Fixture::new(&TWO_SPI, &DUPLICATE)),
        PmuIrqPlan::Malformed(_)
    ));

    let mut absent = Fixture::new(&SPI_FOUR, &AFFINITY_PERMUTED);
    absent.pmu_present = false;
    assert_eq!(plan(&absent), PmuIrqPlan::Absent);
    TestResult::Ok
}

#[def_test]
fn multiple_pmu_nodes_are_unsupported() -> TestResult {
    let mut fixture = Fixture::new(&SPI_FOUR, &AFFINITY_PERMUTED);
    fixture.second_pmu = true;
    assert_eq!(
        plan(&fixture),
        PmuIrqPlan::Unsupported("multiple PMU nodes")
    );
    TestResult::Ok
}

#[def_test]
fn single_spi_without_affinity_uses_cpu_zero() -> TestResult {
    static ONE_SPI: [IrqCell; 1] = [IrqCell {
        kind: 0,
        irq: 4,
        flags: 4,
    }];
    let fixture = Fixture::new(&ONE_SPI, &[]);
    let PmuIrqPlan::Spi(routes) = plan(&fixture) else {
        panic!("single SPI must remain routable");
    };
    assert_eq!(
        routes.as_slice(),
        &[PmuSpiRoute {
            hwirq: 36,
            cpu: LogicalCpuId::new(0)
        }]
    );
    TestResult::Ok
}
