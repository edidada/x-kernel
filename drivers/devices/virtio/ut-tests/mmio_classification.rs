// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::{DeviceKind, MmioVersion, classify_mmio_device};

fn registers(version: u32, device_id: u32) -> [u32; 64] {
    let mut words = [0xa5a5_5a5a; 64];
    words[0] = 0x7472_6976;
    words[1] = version;
    words[2] = device_id;
    words
}

#[def_test]
fn mmio_classification_repeated_reads_leave_all_registers_unchanged() {
    for (version, expected) in [(1, MmioVersion::Legacy), (2, MmioVersion::Modern)] {
        let mut words = registers(version, 4);
        let before = words;
        for _ in 0..3 {
            // SAFETY: the initialized aligned fixture covers the full header,
            // remains live, and is not accessed concurrently during the call.
            let result = unsafe { classify_mmio_device(words.as_mut_ptr().cast(), 256) };
            assert_eq!(result, Some((DeviceKind::Hwrng, expected)));
            assert_eq!(words, before);
        }
    }
}

#[def_test]
fn transport_drop_control_resets_the_same_register_fixture() {
    let mut words = registers(2, 4);
    // SAFETY: the aligned initialized fixture covers the complete header and
    // has no aliases in use until the transport is dropped below.
    let transport = unsafe { super::probe_mmio_device(words.as_mut_ptr().cast(), 256) };
    let was_recognized = transport.is_some();
    drop(transport);
    assert_eq!(was_recognized, true);
    assert_eq!(words[0x70 / 4], 0);
}

#[def_test]
fn mmio_classification_rejects_bad_identification_without_reset() {
    for (magic, version, device_id) in [
        (0, 2, 4),
        (0x7472_6976, 0, 4),
        (0x7472_6976, 3, 4),
        (0x7472_6976, 2, 0),
        (0x7472_6976, 1, 3),
        (0x7472_6976, 2, 3),
        (0x7472_6976, 2, u32::MAX),
    ] {
        let mut words = registers(version, device_id);
        words[0] = magic;
        let before = words;
        // SAFETY: the full initialized header fixture is aligned and exclusively
        // held. Invalid register values do not invalidate its backing memory.
        let result = unsafe { classify_mmio_device(words.as_mut_ptr().cast(), 256) };
        assert_eq!(result, None);
        assert_eq!(words, before);
    }
}

#[def_test]
fn mmio_classification_validates_extent_and_alignment_before_reading() {
    let mut words = registers(2, 4);
    let pointer = words.as_mut_ptr().cast::<u8>();
    for (address, size) in [
        (core::ptr::null_mut(), 256),
        (pointer, 0),
        (pointer, 12),
        (pointer, 255),
        (pointer.wrapping_add(1), 255),
        (pointer.wrapping_add(1), 256),
        ((usize::MAX - 3) as *mut u8, 256),
        (pointer, usize::MAX),
    ] {
        // SAFETY: each input is rejected by null/alignment/extent checks before
        // any read. The valid pointer cases also retain their initialized array.
        let result = unsafe { classify_mmio_device(address, size) };
        assert_eq!(result, None);
    }
    assert_eq!(words, registers(2, 4));
}
