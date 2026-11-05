// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn>
// See LICENSES for license details.

//! Bring-up probe: read the ext4 root directory over the mass-storage path.
//!
//! This is deliberately a probe and not a filesystem: it walks exactly the four
//! on-disk structures needed to print a directory listing — superblock, group
//! descriptor, the root inode's extent tree, and its directory data — so that
//! "the bulk path works" can be checked against real file names instead of a
//! magic number. The production path is `kext4` over the block class with a
//! `kvfs` mount; nothing here should grow into that.
//!
//! On-disk layout taken from the reference (`fs/ext4/ext4.h`, `fs/ext4/super.c`,
//! `fs/ext4/extents.c`, `fs/ext4/dir.c`), which is authoritative for field
//! offsets and for the two features this probe requires.

use alloc::vec::Vec;

use kerrno::{KError, KResult};

use crate::{Hcd, bot::BotEndpoints};

/// `EXT4_SUPER_MAGIC`.
const EXT4_SUPER_MAGIC: u16 = 0xEF53;
/// `EXT4_FEATURE_INCOMPAT_EXTENTS`.
const INCOMPAT_EXTENTS: u32 = 0x0040;
/// `EXT4_FEATURE_INCOMPAT_64BIT`.
const INCOMPAT_64BIT: u32 = 0x0080;
/// Root directory inode number (`EXT4_ROOT_INO`).
const ROOT_INO: u32 = 2;

fn u16_at(buf: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([buf[offset], buf[offset + 1]])
}

fn u32_at(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ])
}

/// Reads `length` bytes starting at byte offset `offset` through the BOT layer.
fn read_bytes(
    hcd: &mut dyn Hcd,
    endpoints: &BotEndpoints,
    offset: u64,
    length: usize,
) -> KResult<Vec<u8>> {
    let first_sector = (offset / 512) as u32;
    let leading = (offset % 512) as usize;
    let sectors = (leading + length).div_ceil(512) as u16;
    let mut raw = super::bot::read_sectors(hcd, endpoints, first_sector, sectors)?;
    if raw.len() < leading + length {
        return Err(KError::Io);
    }
    raw.drain(..leading);
    raw.truncate(length);
    Ok(raw)
}

/// One directory entry as printed.
struct Entry {
    inode: u32,
    kind: u8,
    name: Vec<u8>,
}

/// Decodes ext4 directory entries from one data block (`ext4_readdir`).
fn parse_dirents(block: &[u8], out: &mut Vec<Entry>) {
    let mut offset = 0usize;
    while offset + 8 <= block.len() {
        let inode = u32_at(block, offset);
        let record_length = u16_at(block, offset + 4) as usize;
        let name_length = block[offset + 6] as usize;
        let kind = block[offset + 7];
        if record_length < 8 || offset + record_length > block.len() {
            break;
        }
        if inode != 0 && name_length > 0 && offset + 8 + name_length <= block.len() {
            out.push(Entry {
                inode,
                kind,
                name: block[offset + 8..offset + 8 + name_length].to_vec(),
            });
        }
        offset += record_length;
    }
}

/// Reads `count` filesystem blocks starting at extent entry `index`.
///
/// Only a depth-0 extent tree is handled: the root directory of a freshly made
/// filesystem fits in the four inline extents an inode carries, and a probe that
/// silently mis-reads a deeper tree would be worse than one that says so.
fn read_extent_block(
    hcd: &mut dyn Hcd,
    endpoints: &BotEndpoints,
    inode: &[u8],
    block_size: u64,
    index: usize,
) -> KResult<Vec<u8>> {
    let extent_header = &inode[0x28..];
    let magic = u16_at(extent_header, 0);
    if magic != 0xF30A {
        log::warn!("ext4: root inode has no extent header (magic {magic:#06x})");
        return Err(KError::Unsupported);
    }
    let entries = u16_at(extent_header, 2) as usize;
    let depth = u16_at(extent_header, 6);
    if depth != 0 || index >= entries {
        log::warn!("ext4: extent depth {depth}, entries {entries}: not a depth-0 tree");
        return Err(KError::Unsupported);
    }
    // Extent 4-byte tail: ee_block, ee_len, ee_start_hi, ee_start_lo.
    let extent = &extent_header[12 + index * 12..];
    let start_hi = u16_at(extent, 6) as u64;
    let start_lo = u32_at(extent, 8) as u64;
    let physical = start_lo | (start_hi << 32);
    read_bytes(hcd, endpoints, physical * block_size, block_size as usize)
}

/// Reads one inode's mode and size from the inode table.
fn inode_metadata(
    hcd: &mut dyn Hcd,
    endpoints: &BotEndpoints,
    inode_table: u64,
    block_size: u64,
    inode_size: u64,
    ino: u32,
) -> KResult<(u16, u64)> {
    let offset = inode_table * block_size + u64::from(ino - 1) * inode_size;
    let inode = read_bytes(hcd, endpoints, offset, inode_size as usize)?;
    let mode = u16_at(&inode, 0);
    let size_lo = u32_at(&inode, 4);
    let size_hi = u32_at(&inode, 0x6c);
    let size = u64::from(size_lo) | (u64::from(size_hi) << 32);
    Ok((mode, size))
}

fn kind_name(kind: u8) -> &'static str {
    match kind {
        1 => "file",
        2 => "dir",
        3 => "chr",
        4 => "blk",
        5 => "fifo",
        6 => "sock",
        7 => "link",
        _ => "?",
    }
}

/// Lists the ext4 root directory.
pub fn list_root(hcd: &mut dyn Hcd, endpoints: &BotEndpoints) -> KResult<()> {
    // The superblock lives at byte 1024.
    let superblock = read_bytes(hcd, endpoints, 1024, 1024)?;
    let magic = u16_at(&superblock, 0x38);
    if magic != EXT4_SUPER_MAGIC {
        log::warn!("ext4: no superblock, magic {magic:#06x}");
        return Err(KError::Unsupported);
    }
    let block_size = 1024u64 << u32_at(&superblock, 0x18);
    let first_data_block = u32_at(&superblock, 0x14) as u64;
    let inodes_per_group = u32_at(&superblock, 0x28);
    // s_inode_size is only meaningful from revision 1 onwards; revision 0 means
    // the original 128-byte inode.
    let revision = u32_at(&superblock, 0x4c);
    let inode_size = if revision >= 1 {
        u16_at(&superblock, 0x58) as u64
    } else {
        128
    };
    let incompat = u32_at(&superblock, 0x60);
    let label = core::str::from_utf8(&superblock[0x78..0x88])
        .unwrap_or("?")
        .trim_end_matches('\0')
        .trim();
    log::info!(
        "ext4: label `{label}`, {block_size}-byte blocks, {inodes_per_group} inodes/group, \
         {inode_size}-byte inodes, incompat {incompat:#x}"
    );
    if incompat & INCOMPAT_EXTENTS == 0 {
        log::warn!("ext4: no extents feature; this probe only reads extent trees");
        return Err(KError::Unsupported);
    }

    // Group descriptor 0: the inode table holds the root inode.
    let descriptor_offset = (first_data_block + 1) * block_size;
    let descriptor = read_bytes(hcd, endpoints, descriptor_offset, 64)?;
    let table_lo = u32_at(&descriptor, 0x08) as u64;
    let table_hi = if incompat & INCOMPAT_64BIT != 0 {
        u32_at(&descriptor, 0x28) as u64
    } else {
        0
    };
    let inode_table = table_lo | (table_hi << 32);

    let root_offset = inode_table * block_size + u64::from(ROOT_INO - 1) * inode_size;
    let root_inode = read_bytes(hcd, endpoints, root_offset, inode_size as usize)?;
    let mode = u16_at(&root_inode, 0);
    let size_lo = u32_at(&root_inode, 4);
    let size_hi = u32_at(&root_inode, 0x6c);
    let size = u64::from(size_lo) | (u64::from(size_hi) << 32);
    log::info!("ext4: root inode mode {mode:#06o}, size {size}");

    let block = read_extent_block(hcd, endpoints, &root_inode, block_size, 0)?;
    let mut entries = Vec::new();
    parse_dirents(&block, &mut entries);
    log::info!("ext4: root directory has {} entries", entries.len());
    for entry in entries.iter().take(64) {
        let name = core::str::from_utf8(&entry.name).unwrap_or("<non-utf8>");
        if entry.kind == 1 {
            // A regular file's size and mode come from its own inode, which is
            // one more read of the same inode table.
            match inode_metadata(
                hcd,
                endpoints,
                inode_table,
                block_size,
                inode_size,
                entry.inode,
            ) {
                Ok((mode, size)) => log::info!(
                    "ext4:   {name:<24} inode {:<8} {}, mode {mode:#06o}, {size} bytes",
                    entry.inode,
                    kind_name(entry.kind),
                ),
                Err(err) => log::info!(
                    "ext4:   {name:<24} inode {:<8} {}, inode unreadable: {err:?}",
                    entry.inode,
                    kind_name(entry.kind),
                ),
            }
        } else {
            log::info!(
                "ext4:   {name:<24} inode {:<8} {}",
                entry.inode,
                kind_name(entry.kind)
            );
        }
    }
    Ok(())
}
