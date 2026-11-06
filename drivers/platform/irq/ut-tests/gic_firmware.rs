// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::vec::Vec;
use unittest::{assert_eq, def_test};

struct Dtb {
    structure: Vec<u8>,
    strings: Vec<u8>,
}

impl Dtb {
    fn new() -> Self {
        Self {
            structure: Vec::new(),
            strings: Vec::new(),
        }
    }

    fn word(&mut self, value: u32) {
        self.structure.extend_from_slice(&value.to_be_bytes());
    }

    fn align(&mut self) {
        while !self.structure.len().is_multiple_of(4) {
            self.structure.push(0);
        }
    }

    fn begin(&mut self, name: &str) {
        self.word(1);
        self.structure.extend_from_slice(name.as_bytes());
        self.structure.push(0);
        self.align();
        self.word(4);
    }

    fn property(&mut self, name: &str, bytes: &[u8]) {
        self.word(3);
        self.word(bytes.len() as u32);
        self.word(self.strings.len() as u32);
        self.strings.extend_from_slice(name.as_bytes());
        self.strings.push(0);
        self.structure.extend_from_slice(bytes);
        self.align();
    }

    fn end(&mut self) {
        self.word(2);
    }

    fn finish(mut self) -> Vec<u8> {
        self.word(9);
        let strings_offset = 56 + self.structure.len();
        let mut bytes = Vec::new();
        for value in [
            0xd00d_feed,
            (strings_offset + self.strings.len()) as u32,
            56,
            strings_offset as u32,
            40,
            17,
            16,
            0,
            self.strings.len() as u32,
            self.structure.len() as u32,
        ] {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        bytes.extend_from_slice(&[0; 16]);
        bytes.extend_from_slice(&self.structure);
        bytes.extend_from_slice(&self.strings);
        bytes
    }
}

fn cells(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|value| value.to_be_bytes()).collect()
}

fn fixture(status: Option<&[u8]>, ranges: Option<&[u32]>, reg: &[u32]) -> Vec<u8> {
    fixture_with_compatible(b"arm,gic-400\0", status, ranges, reg)
}

fn fixture_with_compatible(
    compatible: &[u8],
    status: Option<&[u8]>,
    ranges: Option<&[u32]>,
    reg: &[u32],
) -> Vec<u8> {
    let mut blob = Dtb::new();
    blob.begin("");
    blob.property("#address-cells", &cells(&[1]));
    blob.property("#size-cells", &cells(&[1]));
    blob.begin("bus");
    blob.property("#address-cells", &cells(&[1]));
    blob.property("#size-cells", &cells(&[1]));
    if let Some(ranges) = ranges {
        blob.property("ranges", &cells(ranges));
    }
    blob.begin("interrupt-controller");
    blob.property("compatible", compatible);
    blob.property("interrupt-controller", &[]);
    if let Some(status) = status {
        blob.property("status", status);
    }
    blob.property("reg", &cells(reg));
    blob.end();
    blob.end();
    blob.end();
    blob.finish()
}

#[def_test]
fn discovery_skips_disabled_controller() {
    let bytes = fixture(Some(b"disabled\0"), Some(&[]), &[0x1000, 0x1000, 0x2000, 0x1000]);
    let tree = of::LinuxFdt::new(&bytes).unwrap();
    assert_eq!(super::config_from_device_tree(&tree), Ok(None));
}

#[def_test]
fn discovery_translates_complete_register_regions() {
    let bytes = fixture(None, Some(&[0, 0x8000000, 0x10000]), &[0x1000, 0x1000, 0x2000, 0x1000]);
    let tree = of::LinuxFdt::new(&bytes).unwrap();
    let (_, config) = super::config_from_device_tree(&tree).unwrap().unwrap();
    assert_eq!(config.gicd.paddr.as_usize(), 0x8001000);
    assert_eq!(config.gicc.unwrap().paddr.as_usize(), 0x8002000);
}

#[def_test]
fn discovery_rejects_malformed_status_instead_of_skipping_it() {
    let bytes = fixture(Some(b"okay"), Some(&[]), &[0x1000, 0x1000, 0x2000, 0x1000]);
    let tree = of::LinuxFdt::new(&bytes).unwrap();
    unittest::assert!(matches!(
        super::config_from_device_tree(&tree),
        Err(super::firmware::DiscoveryError::Status { .. })
    ));
}

#[def_test]
fn discovery_requires_both_distributor_and_cpu_interface() {
    for reg in [&[][..], &[0x1000, 0x1000][..]] {
        let bytes = fixture(None, Some(&[]), reg);
        let tree = of::LinuxFdt::new(&bytes).unwrap();
        unittest::assert!(matches!(
            super::config_from_device_tree(&tree),
            Err(super::firmware::DiscoveryError::MissingRegion { .. })
        ));
    }
}

#[def_test]
fn discovery_rejects_missing_or_incomplete_address_translation() {
    for ranges in [None, Some(&[0, 0x8000000, 0x1800][..]), Some(&[0][..])] {
        let bytes = fixture(None, ranges, &[0x1000, 0x1000, 0x2000, 0x1000]);
        let tree = of::LinuxFdt::new(&bytes).unwrap();
        unittest::assert!(matches!(
            super::config_from_device_tree(&tree),
            Err(super::firmware::DiscoveryError::Registers(_))
        ));
    }
}

#[def_test]
fn discovery_validates_trailing_and_zero_sized_regions() {
    for reg in [
        &[0x1000, 0x1000, 0x2000, 0x1000, 7][..],
        &[0x1000, 0, 0x2000, 0x1000][..],
    ] {
        let bytes = fixture(None, Some(&[]), reg);
        let tree = of::LinuxFdt::new(&bytes).unwrap();
        unittest::assert!(matches!(
            super::config_from_device_tree(&tree),
            Err(super::firmware::DiscoveryError::Registers(_))
        ));
    }
}

#[def_test]
fn discovery_preserves_gicv2_optional_regions_and_available_statuses() {
    for status in [None, Some(&b"ok\0"[..]), Some(&b"okay\0"[..])] {
        let bytes = fixture(status, Some(&[]), &[0x1000, 0x1000, 0x2000, 0x1000, 0x3000, 0x1000, 0x4000, 0x1000]);
        let tree = of::LinuxFdt::new(&bytes).unwrap();
        let (_, config) = super::config_from_device_tree(&tree).unwrap().unwrap();
        assert_eq!(config.version, super::GicVersion::V2);
        assert_eq!(config.gich.unwrap().paddr.as_usize(), 0x3000);
        assert_eq!(config.gicv.unwrap().paddr.as_usize(), 0x4000);
        assert_eq!(config.gicr, None);
    }
}

#[def_test]
fn discovery_preserves_gicv3_first_redistributor_selection() {
    let bytes = fixture_with_compatible(b"arm,gic-v3\0", None, Some(&[]), &[0x1000, 0x1000, 0x2000, 0x20000, 0x40000, 0x20000]);
    let tree = of::LinuxFdt::new(&bytes).unwrap();
    let (_, config) = super::config_from_device_tree(&tree).unwrap().unwrap();
    assert_eq!(config.version, super::GicVersion::V3);
    assert_eq!(config.gicr.unwrap().paddr.as_usize(), 0x2000);
    assert_eq!(config.gicc, None);
    assert_eq!(config.gich, None);
}

#[def_test]
fn discovery_does_not_fall_back_after_an_invalid_available_candidate() {
    for first_status in [b"okay\0".as_slice(), b"disabled\0".as_slice()] {
        let mut blob = Dtb::new();
        blob.begin("");
        blob.property("#address-cells", &cells(&[1]));
        blob.property("#size-cells", &cells(&[1]));
        for (name, status, reg) in [
            ("first", first_status, &[0x1000, 0x1000][..]),
            ("second", b"okay\0".as_slice(), &[0x3000, 0x1000, 0x4000, 0x1000][..]),
        ] {
            blob.begin(name);
            blob.property("compatible", b"arm,gic-400\0");
            blob.property("interrupt-controller", &[]);
            blob.property("status", status);
            blob.property("reg", &cells(reg));
            blob.end();
        }
        blob.end();
        let bytes = blob.finish();
        let tree = of::LinuxFdt::new(&bytes).unwrap();
        let result = super::config_from_device_tree(&tree);
        if first_status == b"okay\0" {
            unittest::assert!(matches!(result, Err(super::firmware::DiscoveryError::MissingRegion { .. })));
        } else {
            assert_eq!(result.unwrap().unwrap().1.gicd.paddr.as_usize(), 0x3000);
        }
    }
}
