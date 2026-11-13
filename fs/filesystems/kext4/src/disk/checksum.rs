// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

const CRC32C_POLYNOMIAL: u32 = 0x82f6_3b78;

/// Reflected CRC32C lookup table.
///
/// Entry `i` is the register value after running byte `i` through all eight
/// polynomial steps. The old bit-at-a-time loop did that work on every byte of
/// every checksummed block; metadata updates checksum several 4 KiB blocks per
/// syscall, which made the arithmetic the dominant cost of a create or unlink.
const fn crc32c_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ CRC32C_POLYNOMIAL
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}

static CRC32C_TABLE: [u32; 256] = crc32c_table();

/// Updates a Linux-style CRC32C value without applying a final complement.
pub(crate) fn crc32c(mut crc: u32, bytes: &[u8]) -> u32 {
    for &byte in bytes {
        crc = (crc >> 8) ^ CRC32C_TABLE[((crc ^ u32::from(byte)) & 0xff) as usize];
    }
    crc
}

pub(crate) fn group_descriptor_checksum(
    descriptor_bytes: &[u8],
    group: u32,
    checksum_seed: u32,
) -> Option<u16> {
    let before_checksum = descriptor_bytes.get(..30)?;
    let after_checksum = descriptor_bytes.get(32..)?;

    let mut checksum = crc32c(checksum_seed, &group.to_le_bytes());
    checksum = crc32c(checksum, before_checksum);
    checksum = crc32c(checksum, &[0, 0]);
    checksum = crc32c(checksum, after_checksum);
    Some(checksum as u16)
}

pub(crate) fn bitmap_checksum(bitmap: &[u8], checksum_seed: u32) -> u32 {
    crc32c(checksum_seed, bitmap)
}

unittest::path_mod_test!(tests, "../../ut-tests/disk/checksum/tests.rs");
