// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, vec, vec::Vec};

use unittest::{assert_eq, def_test};

use super::*;

fn word(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn padded(bytes: &mut Vec<u8>, value: &[u8]) {
    bytes.extend_from_slice(value);
    while !bytes.len().is_multiple_of(4) {
        bytes.push(0);
    }
}

fn property(structure: &mut Vec<u8>, strings: &mut Vec<u8>, name: &str, value: &[u8]) {
    word(structure, 3);
    word(structure, value.len() as u32);
    word(structure, strings.len() as u32);
    strings.extend_from_slice(name.as_bytes());
    strings.push(0);
    padded(structure, value);
}

fn fixture() -> Vec<u8> {
    topology_fixture(false, b"okay\0", 1, false)
}

fn topology_fixture(nested: bool, status: &[u8], ram_count: usize, malformed_reg: bool) -> Vec<u8> {
    fixture_with_ram_status(nested, status, ram_count, malformed_reg, b"okay\0")
}

fn fixture_with_ram_status(
    nested: bool,
    status: &[u8],
    ram_count: usize,
    malformed_reg: bool,
    last_ram_status: &[u8],
) -> Vec<u8> {
    fixture_with_ancestor_status(
        nested,
        status,
        ram_count,
        malformed_reg,
        last_ram_status,
        None,
    )
}

fn fixture_with_ancestor_status(
    nested: bool,
    status: &[u8],
    ram_count: usize,
    malformed_reg: bool,
    last_ram_status: &[u8],
    ancestor_status: Option<&[u8]>,
) -> Vec<u8> {
    let mut structure = Vec::new();
    let mut strings = Vec::new();
    word(&mut structure, 1);
    padded(&mut structure, &[0]);
    for name in ["#address-cells", "#size-cells"] {
        property(&mut structure, &mut strings, name, &1u32.to_be_bytes());
    }
    property(&mut structure, &mut strings, "dma-coherent", &[]);
    property(&mut structure, &mut strings, "compatible", b"test,board\0");
    if !nested && let Some(status) = ancestor_status {
        property(&mut structure, &mut strings, "status", status);
    }
    for index in 0..ram_count {
        word(&mut structure, 1);
        padded(&mut structure, b"memory@40000000\0");
        property(&mut structure, &mut strings, "device_type", b"memory\0");
        if index + 1 == ram_count {
            property(&mut structure, &mut strings, "status", last_ram_status);
        }
        let mut reg: Vec<u8> = [0x40000000u32, 0x10000000]
            .iter()
            .flat_map(|cell| cell.to_be_bytes())
            .collect();
        if malformed_reg {
            reg.push(0);
        }
        property(&mut structure, &mut strings, "reg", &reg);
        word(&mut structure, 2);
    }
    if nested {
        word(&mut structure, 1);
        padded(&mut structure, b"bus\0");
        property(&mut structure, &mut strings, "compatible", b"test,board\0");
        if let Some(status) = ancestor_status {
            property(&mut structure, &mut strings, "status", status);
        }
    }
    for (name, base) in [
        (b"good\0".as_slice(), 0x1000u32),
        (b"bad\0".as_slice(), 0x2000u32),
    ] {
        word(&mut structure, 1);
        padded(&mut structure, name);
        property(
            &mut structure,
            &mut strings,
            "compatible",
            b"test,dma\0test,fallback\0",
        );
        property(&mut structure, &mut strings, "status", status);
        let reg: Vec<u8> = [base, 0x100]
            .iter()
            .flat_map(|cell| cell.to_be_bytes())
            .collect();
        property(&mut structure, &mut strings, "reg", &reg);
        if base == 0x2000 {
            property(&mut structure, &mut strings, "iommus", &[]);
        }
        word(&mut structure, 2);
    }
    if nested {
        word(&mut structure, 2);
    }
    word(&mut structure, 2);
    word(&mut structure, 9);
    let mut bytes = Vec::new();
    let strings_offset = 56 + structure.len() as u32;
    for value in [
        0xd00dfeed,
        strings_offset + strings.len() as u32,
        56,
        strings_offset,
        40,
        17,
        16,
        0,
        strings.len() as u32,
        structure.len() as u32,
    ] {
        word(&mut bytes, value);
    }
    bytes.extend_from_slice(&[0; 16]);
    bytes.extend(structure);
    bytes.extend(strings);
    bytes
}

#[def_test]
fn discovery_view_identity_and_compatibles_ignore_matcher_selection() {
    let bytes = Box::leak(fixture().into_boxed_slice());
    let tree = Box::leak(Box::new(of::LinuxFdt::new(bytes).unwrap()));
    let mut first_results = vec![];
    visit_tree_devices(
        tree,
        |compatible| (compatible == "test,dma").then_some("alias,first"),
        |view| {
            first_results.push((
                view.node_id(),
                view.description().firmware_id,
                view.is_available(),
                view.compatibles()
                    .map(alloc::string::String::from)
                    .collect::<Vec<_>>(),
            ));
        },
    );
    let first_ids: Vec<_> = first_results.iter().map(|result| result.0).collect();
    assert_eq!(first_ids, vec![2, 3]);
    for (_, firmware_id, is_available, compatibles) in first_results {
        assert_eq!(firmware_id, "alias,first");
        assert_eq!(is_available, true);
        assert_eq!(compatibles, vec!["test,dma", "test,fallback"]);
    }
    let mut second_ids = vec![];
    visit_tree_devices(
        tree,
        |compatible| match compatible {
            "test,board" => Some("alias,root"),
            "test,fallback" => Some("alias,second"),
            _ => None,
        },
        |view| second_ids.push(view.node_id()),
    );
    assert_eq!(second_ids, vec![0, 2, 3]);
    assert_eq!(&second_ids[1..], first_ids.as_slice());
}

#[def_test]
fn discovery_view_availability_preserves_unavailable_nodes() {
    for (status, expected) in [
        (b"okay\0".as_slice(), true),
        (b"ok\0".as_slice(), true),
        (b"disabled\0".as_slice(), false),
        (b"unknown\0".as_slice(), false),
        (b"okay".as_slice(), false),
        (b"okay\0garbage".as_slice(), false),
    ] {
        let bytes = Box::leak(topology_fixture(false, status, 1, false).into_boxed_slice());
        let tree = Box::leak(Box::new(of::LinuxFdt::new(bytes).unwrap()));
        let mut results = vec![];
        visit_tree_devices(
            tree,
            |compatible| match compatible {
                "test,board" | "test,dma" => Some("alias"),
                _ => None,
            },
            |view| results.push((view.node_id(), view.is_available())),
        );
        assert_eq!(results, vec![(0, true), (2, expected), (3, expected)]);
    }
}

#[def_test]
fn discovery_view_availability_checks_root_and_intermediate_ancestors() {
    for nested in [false, true] {
        for (status, expected) in [
            (b"okay\0".as_slice(), true),
            (b"disabled\0".as_slice(), false),
            (b"unknown\0".as_slice(), false),
            (b"okay".as_slice(), false),
        ] {
            let bytes = Box::leak(
                fixture_with_ancestor_status(nested, b"okay\0", 1, false, b"okay\0", Some(status))
                    .into_boxed_slice(),
            );
            let tree = Box::leak(Box::new(of::LinuxFdt::new(bytes).unwrap()));
            let mut results = vec![];
            visit_tree_devices(
                tree,
                |compatible| (compatible == "test,dma").then_some("alias"),
                |view| results.push(view.is_available()),
            );
            assert_eq!(results, vec![expected; 2]);
        }
    }
}

#[def_test]
fn discovery_preserves_devices_with_rejected_dma() {
    let bytes = Box::leak(fixture().into_boxed_slice());
    let tree = Box::leak(Box::new(of::LinuxFdt::new(bytes).unwrap()));
    let mut devices = vec![];
    visit_tree_devices(
        tree,
        |compatible| (compatible == "test,dma").then_some("test,dma"),
        |view| devices.push(*view.description()),
    );
    assert_eq!(devices.len(), 2);
    for (index, device) in devices.iter().enumerate() {
        assert_eq!(device.firmware_id, "test,dma");
        assert_eq!(device.source, FirmwareSource::DeviceTree);
        assert_eq!(device.mmio.unwrap().base, (index + 1) * 0x1000);
        assert_eq!(device.mmio.unwrap().size, 0x100);
        assert_eq!(device.irq.is_none(), true);
    }
    assert_eq!(
        devices[0].dma,
        Ok(DmaFacts {
            range: None,
            coherent: Some(true)
        })
    );
    assert_eq!(devices[1].dma, Err(DmaResolveError::UnsupportedPath));
}

#[def_test]
fn discovery_view_proves_topology_without_trusting_matcher_alias() {
    for (nested, status, expected_root_child) in [
        (false, b"okay\0".as_slice(), true),
        (true, b"okay\0".as_slice(), false),
        (false, b"disabled\0".as_slice(), false),
        (false, b"okay".as_slice(), false),
    ] {
        let bytes = Box::leak(topology_fixture(nested, status, 1, false).into_boxed_slice());
        let tree = Box::leak(Box::new(of::LinuxFdt::new(bytes).unwrap()));
        let mut results = vec![];
        visit_tree_devices(
            tree,
            |compatible| (compatible == "test,dma").then_some("virtio,mmio"),
            |view| {
                results.push((
                    view.is_root_child_of("test,board"),
                    view.is_compatible("virtio,mmio"),
                    view.is_root_child_of("other,board"),
                ));
            },
        );
        assert_eq!(results, vec![(expected_root_child, false, false); 2]);
    }
}

#[def_test]
fn discovery_view_requires_unambiguous_complete_ram_tuple() {
    for (count, malformed, expected) in [
        (1, false, Some((0x40000000, 0x10000000))),
        (0, false, None),
        (2, false, None),
        (1, true, None),
    ] {
        let bytes =
            Box::leak(topology_fixture(false, b"okay\0", count, malformed).into_boxed_slice());
        let tree = Box::leak(Box::new(of::LinuxFdt::new(bytes).unwrap()));
        let mut results = vec![];
        visit_tree_devices(
            tree,
            |compatible| (compatible == "test,dma").then_some("test,dma"),
            |view| results.push(view.single_root_ram_range()),
        );
        assert_eq!(results, vec![expected; 2]);
    }
}

#[def_test]
fn discovery_view_only_ignores_exactly_disabled_ram() {
    for (status, expected) in [
        (b"disabled\0".as_slice(), Some((0x40000000, 0x10000000))),
        (b"disabled\0garbage".as_slice(), None),
        (b"unknown\0".as_slice(), None),
    ] {
        let bytes = Box::leak(
            fixture_with_ram_status(false, b"okay\0", 2, false, status).into_boxed_slice(),
        );
        let tree = Box::leak(Box::new(of::LinuxFdt::new(bytes).unwrap()));
        let mut results = vec![];
        visit_tree_devices(
            tree,
            |compatible| (compatible == "test,dma").then_some("test,dma"),
            |view| results.push(view.single_root_ram_range()),
        );
        assert_eq!(results, vec![expected; 2]);
    }
}
