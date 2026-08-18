#!/usr/bin/env python3
"""Generate binary fuzz corpus seeds for x-kernel harnesses."""

from __future__ import annotations

import struct
from pathlib import Path

SEED_DIR = Path(__file__).resolve().parent / "seeds"

EXT4_FEATURE_COMPAT_HAS_JOURNAL = 0x0004
EXT4_FEATURE_INCOMPAT_EXTENTS = 0x0040
EXT4_FEATURE_INCOMPAT_64BIT = 0x0080
EXT4_FEATURE_INCOMPAT_FLEX_BG = 0x0200


def write(name: str, data: bytes) -> None:
    path = SEED_DIR / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    print(f"wrote {path} ({len(data)} bytes)")


def ext4_superblock(
    *,
    feature_compat: int = 0,
    feature_incompat: int = 0,
    feature_ro_compat: int = 0,
    desc_size: int = 32,
    log_block_size: int = 2,
) -> bytes:
    sb = bytearray(1024)
    struct.pack_into("<I", sb, 0x00, 1024)
    struct.pack_into("<I", sb, 0x04, 32768)
    struct.pack_into("<I", sb, 0x14, log_block_size)
    struct.pack_into("<I", sb, 0x18, 8192)
    struct.pack_into("<I", sb, 0x1C, 256)
    struct.pack_into("<H", sb, 0x38, 0xEF53)
    struct.pack_into("<I", sb, 0x44, 1)
    struct.pack_into("<I", sb, 0x54, 11)
    struct.pack_into("<H", sb, 0x58, 256)
    struct.pack_into("<I", sb, 0x50, feature_compat)
    struct.pack_into("<I", sb, 0x60, feature_incompat)
    struct.pack_into("<I", sb, 0x64, feature_ro_compat)
    struct.pack_into("<H", sb, 0xFE, desc_size)
    struct.pack_into("<I", sb, 0xE8, 0x12345678)
    return bytes(sb)


def ext4_gdt(desc_size: int, groups: int) -> bytes:
    meta = (0 if desc_size == 32 else 1) | ((groups - 1) << 1)
    payload = bytearray(desc_size * groups)
    for i in range(groups):
        off = i * desc_size
        struct.pack_into("<III", payload, off + 0, i + 1, i + 2, i + 10)
        struct.pack_into("<HHH", payload, off + 12, 100, 50, 5)
        if desc_size >= 64:
            struct.pack_into("<I", payload, off + 32, i + 100)
            struct.pack_into("<I", payload, off + 36, i + 200)
    return bytes([meta]) + bytes(payload)


def ext4_inode(size: int, mode: int = 0x81A4, flags: int = 0) -> bytes:
    inode = bytearray(size)
    struct.pack_into("<H", inode, 0x00, mode)
    struct.pack_into("<I", inode, 0x04, 4096)
    struct.pack_into("<H", inode, 0x1A, 1)
    struct.pack_into("<I", inode, 0x20, flags)
    struct.pack_into("<I", inode, 0x28, 0x1234)
    return bytes(inode)


def ext4_dirent_entry(inode: int, file_type: int, name: str) -> bytes:
    name_b = name.encode("ascii", errors="ignore")[:255]
    rec_len = ((8 + len(name_b) + 3) // 4) * 4
    blob = struct.pack("<IHBB", inode, rec_len, len(name_b), file_type) + name_b
    return blob + b"\x00" * (rec_len - len(blob))


def ext4_dirent_chain() -> bytes:
    def entry(inode: int, file_type: int, name: str) -> bytes:
        return ext4_dirent_entry(inode, file_type, name)

    body = entry(2, 2, ".") + entry(2, 2, "..") + entry(12, 1, "hello.txt")
    body += entry(0, 0, "")[:4096 - len(body)] if len(body) < 4096 else b""
    return body[:4096]


def ext4_extent_leaf(entries: int = 2) -> bytes:
    hdr = struct.pack("<HHHHI", 0xF30A, entries, 4, 0, 0)
    body = bytearray()
    for i in range(entries):
        body += struct.pack("<IHI I", i * 8, 4, 0, 100 + i)
    return hdr + bytes(body)


def ext4_extent_index(entries: int = 1) -> bytes:
    hdr = struct.pack("<HHHHI", 0xF30A, entries, 4, 1, 0)
    body = bytearray()
    for i in range(entries):
        body += struct.pack("<IIHH", i * 16, 200 + i, 0, 0)
    return hdr + bytes(body)


def efi_descriptor(mem_type: int, phys: int, pages: int) -> bytes:
    return struct.pack("<I4xQQQQ8x", mem_type, phys, 0, pages, 0)


def efi_memmap(types: list[tuple[int, int, int]]) -> bytes:
    desc_size = 48
    descs = b"".join(efi_descriptor(t, phys, pages) for t, phys, pages in types)
    header = struct.pack(
        "<IIIII",
        len(descs),
        desc_size,
        1,
        0,
        len(descs),
    )
    return header + descs


def sm2_generator_sec1() -> bytes:
    gx = bytes([
        0x32, 0xc4, 0xae, 0x2c, 0x1f, 0x19, 0x81, 0x19, 0x5f, 0x99, 0x04, 0x46, 0x6a, 0x39, 0xc9, 0x94,
        0x8f, 0xe3, 0x0b, 0xbf, 0xf2, 0x66, 0x0b, 0xe1, 0x71, 0x5a, 0x45, 0x89, 0x33, 0x4c, 0x74, 0xc7,
    ])
    gy = bytes([
        0xbc, 0x37, 0x36, 0xa2, 0xf4, 0xf6, 0x77, 0x9c, 0x59, 0xbd, 0xce, 0xe3, 0x6b, 0x69, 0x21, 0x53,
        0xd0, 0xa9, 0x87, 0x7c, 0xc6, 0x2a, 0x47, 0x40, 0x02, 0xdf, 0x32, 0xe5, 0x21, 0x39, 0xf0, 0xa0,
    ])
    return b"\x04" + gx + gy


def kbpf_programs() -> dict[str, bytes]:
    return {
        "kbpf/mov_exit42.bin": bytes([
            0xB7, 0x00, 0x00, 0x00, 0x2A, 0x00, 0x00, 0x00,
            0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]),
        "kbpf/add64_reg.bin": bytes([
            0xB7, 0x01, 0x00, 0x00, 0x0C, 0x00, 0x00, 0x00,
            0xB7, 0x00, 0x00, 0x00, 0x1E, 0x00, 0x00, 0x00,
            0x0F, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]),
        "kbpf/add64_imm.bin": bytes([
            0xB7, 0x00, 0x00, 0x00, 0x28, 0x00, 0x00, 0x00,
            0x07, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00,
            0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]),
        "kbpf/loop_sum.bin": bytes([
            0xB7, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0xB7, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
            0x0F, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x07, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
            0x15, 0x01, 0x01, 0x00, 0x06, 0x00, 0x00, 0x00,
            0x05, 0x00, 0xFC, 0xFF, 0x00, 0x00, 0x00, 0x00,
            0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]),
        "kbpf/jump_oob.bin": bytes([
            0x05, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]),
        "kbpf/call_helper.bin": bytes([
            0x85, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
            0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]),
        "kbpf/unknown_opcode.bin": bytes([0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    }


def main() -> None:
    write("rsext4/ext4_superblock.bin", ext4_superblock())
    write(
        "rsext4/ext4_superblock_extents.bin",
        ext4_superblock(feature_incompat=EXT4_FEATURE_INCOMPAT_EXTENTS),
    )
    write(
        "rsext4/ext4_superblock_journal.bin",
        ext4_superblock(
            feature_compat=EXT4_FEATURE_COMPAT_HAS_JOURNAL,
            feature_incompat=EXT4_FEATURE_INCOMPAT_EXTENTS,
        ),
    )
    write(
        "rsext4/ext4_superblock_64bit_flex.bin",
        ext4_superblock(
            feature_compat=EXT4_FEATURE_COMPAT_HAS_JOURNAL,
            feature_incompat=(
                EXT4_FEATURE_INCOMPAT_EXTENTS
                | EXT4_FEATURE_INCOMPAT_64BIT
                | EXT4_FEATURE_INCOMPAT_FLEX_BG
            ),
            desc_size=64,
        ),
    )
    write("rsext4/ext4_gdt_32.bin", ext4_gdt(32, 4))
    write("rsext4/ext4_gdt_64.bin", ext4_gdt(64, 8))
    write("rsext4/ext4_gdt_32_16.bin", ext4_gdt(32, 16))
    write("rsext4/ext4_inode_128.bin", ext4_inode(128))
    write("rsext4/ext4_inode_256.bin", ext4_inode(256))
    write("rsext4/ext4_inode_dir_128.bin", ext4_inode(128, mode=0x41ED))
    write("rsext4/ext4_inode_symlink_256.bin", ext4_inode(256, mode=0xA1FF))
    write("rsext4/ext4_gdt_32_raw.bin", ext4_gdt(32, 4)[1:])
    write("rsext4/ext4_gdt_64_raw.bin", ext4_gdt(64, 8)[1:])
    write("rsext4/ext4_dirent_chain.bin", ext4_dirent_chain())
    write("rsext4/ext4_dirent_longname.bin", ext4_dirent_chain() + ext4_dirent_entry(99, 1, "a-very-long-filename-for-fuzz"))
    write("rsext4/ext4_extent_leaf.bin", ext4_extent_leaf(2))
    write("rsext4/ext4_extent_index.bin", ext4_extent_index(2))
    write(
        "firmware_handoff/efi_memmap.bin",
        efi_memmap([(7, 0x1000, 2), (9, 0x8000, 3)]),
    )
    write(
        "firmware_handoff/efi_memmap_mixed.bin",
        efi_memmap([
            (7, 0x1000, 4),
            (9, 0x9000, 2),
            (10, 0xA000, 1),
            (11, 0xF000_0000, 16),
            (14, 0xB000, 8),
        ]),
    )
    write("tee/sm2_generator.sec1", sm2_generator_sec1())
    for name, data in kbpf_programs().items():
        write(name, data)


if __name__ == "__main__":
    main()
