// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use unittest::{assert, assert_eq, def_test};

use crate::{AddressErrorReason, LinuxFdt, PropertyError, mmio_resources, test_support::Dtb};

fn cells(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_be_bytes())
        .collect()
}

fn fixture(ranges: Option<&[u8]>, reg: &[u8], names: Option<&[u8]>) -> Vec<u8> {
    let mut blob = Dtb::new();
    blob.begin("");
    blob.begin("bus");
    blob.property("#address-cells", &cells(&[1]));
    blob.property("#size-cells", &cells(&[1]));
    if let Some(ranges) = ranges {
        blob.property("ranges", ranges);
    }
    blob.begin("device");
    blob.property("reg", reg);
    if let Some(names) = names {
        blob.property("reg-names", names);
    }
    blob.end();
    blob.end();
    blob.end();
    blob.finish()
}

#[def_test]
fn multiple_regions_translate_and_names_borrow_the_blob() {
    let bytes = fixture(
        Some(&cells(&[0x100, 0, 0x8000, 0x100])),
        &cells(&[0x110, 0x10, 0x140, 0x20]),
        Some(b"control\0"),
    );
    let tree = LinuxFdt::new(&bytes).unwrap();
    let regions = mmio_resources(tree.find_node("/bus/device").unwrap()).unwrap();
    assert_eq!(regions.len(), 2);
    assert_eq!((regions[0].base, regions[0].size), (0x8010, 0x10));
    assert_eq!((regions[1].base, regions[1].size), (0x8040, 0x20));
    assert_eq!(regions[0].name, Some("control"));
    assert_eq!(regions[1].name, None);
    let name = regions[0].name.unwrap().as_ptr() as usize;
    assert!((bytes.as_ptr() as usize..bytes.as_ptr() as usize + bytes.len()).contains(&name));
}

#[def_test]
fn missing_ranges_is_not_identity() {
    for (ranges, expected) in [
        (
            None,
            Some(AddressErrorReason::Property(PropertyError::Missing)),
        ),
        (Some(&[][..]), None),
    ] {
        let bytes = fixture(ranges, &cells(&[0x20, 0x10]), None);
        let tree = LinuxFdt::new(&bytes).unwrap();
        let result = mmio_resources(tree.find_node("/bus/device").unwrap());
        if let Some(expected) = expected {
            let error = result.unwrap_err();
            assert_eq!(error.reason, expected);
            assert_eq!(error.node_offset, tree.find_node("/bus").unwrap().offset());
        } else {
            assert_eq!(result.unwrap()[0].base, 0x20);
        }
    }
}

#[def_test]
fn every_tuple_and_whole_interval_must_validate() {
    for (reg, expected) in [
        (
            cells(&[0x10, 0x10, 0x20]),
            AddressErrorReason::Property(PropertyError::InvalidEncoding),
        ),
        (
            cells(&[0x10, 0x10, 0xf0, 0x20]),
            AddressErrorReason::OutsideRanges,
        ),
        (cells(&[0x10, 0x10, 0x30, 0]), AddressErrorReason::ZeroSize),
        (cells(&[u32::MAX, 2]), AddressErrorReason::Overflow),
    ] {
        let bytes = fixture(Some(&cells(&[0, 0, 0x8000, 0x100])), &reg, None);
        let tree = LinuxFdt::new(&bytes).unwrap();
        assert_eq!(
            mmio_resources(tree.find_node("/bus/device").unwrap())
                .unwrap_err()
                .reason,
            expected
        );
    }
}

#[def_test]
fn duplicate_names_remain_facts_but_invalid_names_fail() {
    for (names, expected) in [
        (b"same\0same\0".as_slice(), None),
        (
            b"good\0bad",
            Some(AddressErrorReason::Property(PropertyError::InvalidEncoding)),
        ),
        (
            b"good\0\0",
            Some(AddressErrorReason::Property(PropertyError::InvalidEncoding)),
        ),
        (b"a\0b\0c\0", Some(AddressErrorReason::ExcessNames)),
    ] {
        let bytes = fixture(Some(&[]), &cells(&[0x10, 0x10, 0x20, 0x10]), Some(names));
        let tree = LinuxFdt::new(&bytes).unwrap();
        let result = mmio_resources(tree.find_node("/bus/device").unwrap());
        if let Some(expected) = expected {
            assert_eq!(result.unwrap_err().reason, expected);
        } else {
            let resources = result.unwrap();
            assert_eq!(resources[0].name, resources[1].name);
        }
    }
}

#[def_test]
fn nested_windows_and_cpu_addresses_remain_u64() {
    let mut blob = Dtb::new();
    blob.begin("");
    blob.begin("outer");
    blob.property("#address-cells", &cells(&[1]));
    blob.property("#size-cells", &cells(&[1]));
    blob.property("ranges", &cells(&[0x100, 1, 0x8000, 0x100]));
    blob.begin("inner");
    blob.property("#address-cells", &cells(&[1]));
    blob.property("#size-cells", &cells(&[1]));
    blob.property("ranges", &cells(&[0, 0x120, 0x40]));
    blob.begin("device");
    blob.property("reg", &cells(&[0x10, 0x10]));
    blob.end();
    blob.end();
    blob.end();
    blob.end();
    let bytes = blob.finish();
    let tree = LinuxFdt::new(&bytes).unwrap();
    let result = mmio_resources(tree.find_node("/outer/inner/device").unwrap()).unwrap();
    assert_eq!(result[0].base, 0x1_0000_8030);
}

#[def_test]
fn unsupported_bus_and_cell_encodings_do_not_guess() {
    for (bus, width, expected) in [
        ("pci", 1, AddressErrorReason::UnsupportedBus),
        ("isa", 1, AddressErrorReason::UnsupportedBus),
        ("bus", 3, AddressErrorReason::UnsupportedCells(3)),
        ("bus", 0, AddressErrorReason::UnsupportedCells(0)),
    ] {
        let mut blob = Dtb::new();
        blob.begin("");
        blob.begin(bus);
        blob.property("#address-cells", &cells(&[width]));
        blob.property("ranges", &[]);
        blob.begin("device");
        blob.property("reg", &cells(&[0, 0x1000, 0x100]));
        blob.end();
        blob.end();
        blob.end();
        let bytes = blob.finish();
        let tree = LinuxFdt::new(&bytes).unwrap();
        assert_eq!(
            mmio_resources(tree.all_nodes().nth(2).unwrap())
                .unwrap_err()
                .reason,
            expected
        );
    }
}
