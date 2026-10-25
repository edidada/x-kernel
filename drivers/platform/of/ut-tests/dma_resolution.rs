// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::{LinuxFdt, TestDtbBuilder, Vec};
use crate::dma::{DmaFacts, DmaResolveError, ResolvedDmaRange, resolve_dma};

const FDT_NOP: u32 = 4;

#[def_test]
fn dma_rejects_flag_encoded_buses_without_device_type() {
    for name in ["isa", "isa@0", "pcie", "pcie@0"] {
        let mut builder = TestDtbBuilder::new();
        builder.begin_node("");
        builder.prop_u32("#address-cells", 1);
        builder.begin_node(name);
        builder.prop_u32("#address-cells", 2);
        builder.prop_u32("#size-cells", 1);
        builder.prop_u32s("dma-ranges", &[1, 0x100, 0x8000, 0x100]);
        builder.begin_node("device");
        builder.end_node();
        builder.end_node();
        builder.end_node();
        let bytes = builder.finish();
        let fdt = LinuxFdt::new(&bytes).unwrap();
        let node = fdt.all_nodes().find(|node| node.name == "device").unwrap();
        assert_eq!(resolve_dma(node), Err(DmaResolveError::UnsupportedPath));
    }
}

fn tree(inner: Option<&[u32]>, outer: Option<&[u32]>, device_property: Option<&str>) -> Vec<u8> {
    let mut builder = TestDtbBuilder::new();
    builder.begin_node("");
    TestDtbBuilder::push_be32(&mut builder.structs, FDT_NOP);
    builder.prop_u32("#address-cells", 1);
    builder.prop_u32("#size-cells", 1);
    builder.prop_empty("dma-coherent");
    builder.begin_node("bus");
    TestDtbBuilder::push_be32(&mut builder.structs, FDT_NOP);
    builder.prop_u32("#address-cells", 1);
    builder.prop_u32("#size-cells", 1);
    if let Some(cells) = outer {
        builder.prop_u32s("dma-ranges", cells);
    }
    builder.begin_node("bus");
    TestDtbBuilder::push_be32(&mut builder.structs, FDT_NOP);
    builder.prop_u32("#address-cells", 1);
    builder.prop_u32("#size-cells", 1);
    if let Some(cells) = inner {
        builder.prop_u32s("dma-ranges", cells);
    }
    builder.begin_node("device");
    if let Some(property) = device_property {
        builder.prop_empty(property);
    }
    builder.end_node();
    builder.end_node();
    builder.end_node();
    builder.end_node();
    builder.finish()
}

fn resolve(bytes: &[u8]) -> Result<DmaFacts, DmaResolveError> {
    let fdt = LinuxFdt::new(bytes).unwrap();
    resolve_dma(fdt.find_node("/bus/bus/device").unwrap())
}

#[def_test]
fn dma_root_child_keeps_unknown_facts_and_rejects_bad_coherency() {
    for marker in ["absent", "dma-coherent", "conflict", "malformed"] {
        let mut builder = TestDtbBuilder::new();
        builder.begin_node("");
        if marker == "dma-coherent" {
            builder.prop_empty("dma-coherent");
        }
        builder.begin_node("device");
        if marker == "conflict" {
            builder.prop_empty("dma-coherent");
            builder.prop_empty("dma-noncoherent");
        }
        if marker == "malformed" {
            builder.prop_u32("dma-coherent", 1);
        }
        builder.end_node();
        builder.end_node();
        let bytes = builder.finish();
        let fdt = LinuxFdt::new(&bytes).unwrap();
        let result = resolve_dma(fdt.find_node("/device").unwrap());
        match marker {
            "absent" => assert_eq!(
                result,
                Ok(DmaFacts {
                    range: None,
                    coherent: None
                })
            ),
            "dma-coherent" => assert_eq!(
                result,
                Ok(DmaFacts {
                    range: None,
                    coherent: Some(true)
                })
            ),
            _ => assert_eq!(result, Err(DmaResolveError::InvalidCoherency)),
        }
    }
}

#[def_test]
fn dma_resolves_nested_windows_and_nearest_coherency() {
    let bytes = tree(
        Some(&[0x100, 0x220, 0x80]),
        Some(&[0x200, 0x8000, 0x100]),
        Some("dma-noncoherent"),
    );
    assert_eq!(
        resolve(&bytes),
        Ok(DmaFacts {
            range: Some(ResolvedDmaRange {
                bus_addr: 0x100,
                cpu_addr: 0x8020,
                size: 0x80
            }),
            coherent: Some(false),
        })
    );
    let fdt = LinuxFdt::new(&bytes).unwrap();
    for node in [
        fdt.find_node("/bus/bus/device").unwrap(),
        fdt.all_nodes().find(|node| node.name == "device").unwrap(),
    ] {
        let inner = node.parent().unwrap();
        assert_eq!(resolve_dma(node), resolve(&bytes));
        let outer = inner.parent().unwrap();
        assert_eq!(inner.name, outer.name);
        assert_eq!(outer.parent().unwrap().name, "/");
        assert_eq!(outer.parent().unwrap().parent().is_none(), true);
    }
}

#[def_test]
fn dma_missing_is_not_identity_or_permission() {
    assert_eq!(
        resolve(&tree(None, None, None)),
        Ok(DmaFacts {
            range: None,
            coherent: Some(true)
        })
    );
    assert_eq!(
        resolve(&tree(None, Some(&[0, 0x8000, 0x100]), None))
            .unwrap()
            .range,
        None
    );
    assert_eq!(
        resolve(&tree(Some(&[]), Some(&[0, 0x8000, 0x100]), None))
            .unwrap()
            .range,
        Some(ResolvedDmaRange {
            bus_addr: 0,
            cpu_addr: 0x8000,
            size: 0x100
        })
    );
    assert_eq!(
        resolve(&tree(Some(&[0, 0x200, 0x100]), None, None)),
        Err(DmaResolveError::MissingParentRange)
    );
    assert_eq!(
        resolve(&tree(Some(&[0, 0x200, 0x100]), Some(&[]), None))
            .unwrap()
            .range,
        Some(ResolvedDmaRange {
            bus_addr: 0,
            cpu_addr: 0x200,
            size: 0x100
        })
    );
}

#[def_test]
fn dma_rejects_partial_parent_coverage_and_multiple_windows() {
    assert_eq!(
        resolve(&tree(
            Some(&[0, 0x200, 0x101]),
            Some(&[0x200, 0x8000, 0x100]),
            None
        )),
        Err(DmaResolveError::OutsideParentRange)
    );
    assert_eq!(
        resolve(&tree(
            Some(&[0, 0x1ff, 1]),
            Some(&[0x200, 0x8000, 0x100]),
            None
        )),
        Err(DmaResolveError::OutsideParentRange)
    );
    assert_eq!(
        resolve(&tree(Some(&[0, 0x200, 1, 2, 0x202, 1]), Some(&[]), None)),
        Err(DmaResolveError::MultipleWindows)
    );
    for property in ["iommus", "iommu-map", "interconnects", "interconnect-names"] {
        assert_eq!(
            resolve(&tree(None, None, Some(property))),
            Err(DmaResolveError::UnsupportedPath)
        );
    }
}
