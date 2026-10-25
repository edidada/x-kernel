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
    let mut structure = Vec::new();
    let mut strings = Vec::new();
    word(&mut structure, 1);
    padded(&mut structure, &[0]);
    for name in ["#address-cells", "#size-cells"] {
        property(&mut structure, &mut strings, name, &1u32.to_be_bytes());
    }
    property(&mut structure, &mut strings, "dma-coherent", &[]);
    for (name, base) in [
        (b"good\0".as_slice(), 0x1000u32),
        (b"bad\0".as_slice(), 0x2000u32),
    ] {
        word(&mut structure, 1);
        padded(&mut structure, name);
        property(&mut structure, &mut strings, "compatible", b"test,dma\0");
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
fn discovery_preserves_devices_with_rejected_dma() {
    let bytes = Box::leak(fixture().into_boxed_slice());
    let tree = Box::leak(Box::new(of::LinuxFdt::new(bytes).unwrap()));
    let mut devices = vec![];
    visit_compatible_nodes(
        tree.all_nodes(),
        |compatible| (compatible == "test,dma").then_some("test,dma"),
        |device| devices.push(device),
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
