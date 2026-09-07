// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::fmt::{self, Write};

use kcpu_slot::SlotInitError;

fn error(tag: u8, value: u8) -> SlotInitError<u8> {
    match tag {
        0 => SlotInitError::NoSpace,
        1 => SlotInitError::Misaligned,
        _ => SlotInitError::Init(value),
    }
}

#[kani::proof]
fn equality_matches_variant_and_payload() {
    let a: u8 = kani::any();
    let b: u8 = kani::any();
    let x: u8 = kani::any();
    let y: u8 = kani::any();
    kani::assume(a <= 2 && b <= 2);
    let left = error(a, x);
    let right = error(b, y);
    let expected = a == b && (a != 2 || x == y);
    assert_eq!(left == right, expected);
    assert_eq!(left != right, !expected);
    assert_eq!(left, left);
}

struct Buffer {
    bytes: [u8; 16],
    len: usize,
}
impl Write for Buffer {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            if self.len == self.bytes.len() {
                return Err(fmt::Error);
            }
            self.bytes[self.len] = byte;
            self.len += 1;
        }
        Ok(())
    }
}

#[kani::proof]
#[kani::unwind(33)]
fn debug_matches_variant_and_u8_payload() {
    let tag: u8 = kani::any();
    let value: u8 = kani::any();
    kani::assume(tag <= 2);
    let mut actual = Buffer {
        bytes: [0; 16],
        len: 0,
    };
    write!(&mut actual, "{:?}", error(tag, value)).unwrap();
    let mut expected = Buffer {
        bytes: [0; 16],
        len: 0,
    };
    match tag {
        0 => expected.write_str("NoSpace").unwrap(),
        1 => expected.write_str("Misaligned").unwrap(),
        _ => {
            expected.write_str("Init(").unwrap();
            if value >= 100 {
                expected.bytes[expected.len] = b'0' + value / 100;
                expected.len += 1;
            }
            if value >= 10 {
                expected.bytes[expected.len] = b'0' + value / 10 % 10;
                expected.len += 1;
            }
            expected.bytes[expected.len] = b'0' + value % 10;
            expected.len += 1;
            expected.write_str(")").unwrap();
        }
    }
    assert_eq!(actual.len, expected.len);
    for index in 0..actual.len {
        assert_eq!(actual.bytes[index], expected.bytes[index]);
    }
}
