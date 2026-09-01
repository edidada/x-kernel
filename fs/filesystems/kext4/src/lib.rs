// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ext4 filesystem support for X-Kernel's KVFS.
//!
//! The crate registers the device-backed `ext4` filesystem during kernel
//! initialization. It owns validation and interpretation of ext4 metadata,
//! extent and bitmap allocation, JBD2 recovery and metadata transactions,
//! namespace and extended-attribute updates, truncate/orphan cleanup, and
//! ordered-data writeback. KVFS remains responsible for pathname traversal,
//! resident inode identity, page-cache state, and open-file lifetimes; the
//! block layer owns device discovery and request execution.
//!
//! # Integration
//!
//! Enable `KFEAT_FS_EXT4` to link the crate and register the filesystem type.
//! Mounts then use the ordinary KVFS device-backed mount path. Mount setup
//! validates the superblock, feature set, group geometry, checksums, journal,
//! and root inode before publishing the filesystem. If recovery is required,
//! the mount path replays committed journal updates and cleans the legacy
//! orphan list before retrying initialization.
//!
//! Most kernel callers use the filesystem through KVFS operations rather than
//! constructing storage objects directly. Exported types such as
//! [`Ext4Inode`], [`BlockMapping`], and [`Ext4Error`] describe the checked
//! inode state, mapping results, and failures used at that integration
//! boundary.
//!
//! Mount, recovery, namespace mutation, writeback, and synchronization may
//! allocate and issue blocking device I/O, so they require sleepable task
//! context. The entire crate forbids unsafe Rust; raw device access and
//! user-memory handling stay outside this crate.
//!
//! # Format boundary
//!
//! Mounts require extent-based block mapping. Mount validation rejects unknown
//! incompatible ext4 features, `BIGALLOC`, and unsupported JBD2 features;
//! operation-specific checks reject other known layouts that KExt4 cannot
//! preserve. Malformed ranges, metadata, and checksums produce corruption or
//! bounds errors rather than unchecked access. See `docs/design.md` for the
//! component and lifecycle model and `docs/security.md` for trust boundaries,
//! implemented controls, and known limitations.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

#[macro_use]
extern crate klogger;

#[cfg(not(target_os = "none"))]
extern crate std;

mod balloc;
mod bitmap_allocator;
/// Coordinates the journal-aware metadata cache and checkpoint writeback.
///
/// One cache slot owns the loaded bytes and transaction/checkpoint state for a
/// physical metadata block. Storage algorithms enter through `Ext4MetadataIo`;
/// cache misses and checkpoint writes may wait for device I/O, and mutable
/// access requires ownership by the active journal transaction.
mod buffer;
/// Decodes, validates, streams, and looks up ext4 directory entries.
///
/// Directory operations walk inode mappings through `Ext4SbInfo`, validate
/// record boundaries and checksums, and either emit borrowed entries or return
/// owned lookup results. Reading an uncached directory block may block on I/O.
mod dir;
/// Implements ext4 HTree and Orlov directory-name hashing policy.
///
/// Hash inputs are raw name bytes plus the seed and signedness policy frozen in
/// the mounted superblock; the module owns no persistent state.
mod dirhash;
/// Defines checked codecs and representations for ext4 on-disk structures.
///
/// These modules translate untrusted byte slices into checked values and
/// encode validated updates. They perform no device I/O and do not own mount
/// state; higher layers supply complete buffers and enforce operation policy.
mod disk;
/// Defines the error taxonomy returned by the checked storage core.
mod error;
/// Maps and mutates extent trees and legacy read-only block maps.
///
/// Lookup validates each traversed node before returning a `BlockMapping`.
/// Mutation is limited to extent-format inodes and updates inode or external
/// tree blocks only through a journal handle with sufficient metadata credits.
mod extent;
/// Implements regular-file reads, ordered writeback, and inode synchronization.
///
/// Ordered writeback writes file data before committing mappings and inode
/// metadata that expose it. A request may span transactions and reports a
/// completed byte prefix when a later transaction fails.
mod file;
mod ialloc;
/// Owns decoded inode state, delayed-allocation reservations, and inode-table updates.
///
/// `Ext4Inode` is the ext4-private component of a KVFS resident inode, not a
/// second identity cache. Its locks protect decoded and transient metadata;
/// cross-structure updates also maintain mount-wide allocation accounting.
mod inode;
/// Adapts block devices to ext4 filesystem-block addressing.
///
/// `FilesystemDevice` validates geometry and complete-range bounds before
/// translating filesystem blocks into driver requests. Reads, writes, and
/// flushes may block and propagate block-driver failures.
mod io;
/// Implements JBD2 log parsing, transaction state, commit encoding, and replay.
///
/// The module keeps journal block numbering and transaction phases separate
/// from ext4 mount policy. Callers provide a checked journal mapping and drive
/// commit, replay, and checkpoint I/O through explicit reader/writer traits.
mod jbd2;
mod journal;
/// Exercises KExt4 against filesystem images built and checked by e2fsprogs.
#[cfg(test)]
mod linux_image_tests;
mod mballoc;
mod namei;
mod orphan;
/// Builds and owns validated per-mount ext4 state and filesystem geometry.
///
/// `Ext4SbInfo::open` validates the superblock, group layout, system zones, and
/// journal before the mount is published. The resulting object owns the
/// device, metadata cache, allocator state, delayed-allocation accounting, and
/// optional mounted journal for the KVFS superblock lifetime.
mod superblock;
/// Selects the kernel or host synchronization primitives used by the crate.
mod sync;
mod truncate;
/// Provides unit-bearing newtypes for ext4 block, group, and inode identifiers.
mod types;
mod vfs;
/// Reads, lists, updates, and removes ext4 inline and external extended attributes.
///
/// Reads validate inode-body and external-block layouts before exposing names
/// or values. Mutations plan the complete new layout first, then journal inode
/// and external-block changes while preserving checksum and reference-count
/// rules.
mod xattr;

pub use dir::{DirectoryEntry, Ext4DirEntryRef, Ext4DirPos, Ext4DirSink};
pub use disk::{
    BlockGroupDescriptor, CompatFeatures, DirectoryFileType, Ext4DiskSuperblock, FeatureSet,
    IncompatFeatures, JournalFields, ReadOnlyCompatFeatures,
};
pub use error::{
    ChecksumTarget, CorruptKind, Ext4Error, Ext4Result, FeatureClass, UnsupportedKind,
};
pub use extent::{BlockMapping, BlockMappingFlags};
pub use file::{Ext4SyncIntent, Ext4WritebackError, Ext4WritebackResult};
pub use inode::{
    Ext4DeviceId, Ext4Inode, Ext4InodeMetadataUpdate, Ext4InodeStat, Ext4Timestamp, InodeKind,
};
pub(crate) use superblock::Ext4SbInfo;
pub use superblock::{
    Ext4RecoveryReport, Ext4StatFs, FilesystemLayout, JournalLocation, JournalStatus,
};
pub use types::{
    BlockCount, BlockGroupNumber, FilesystemBlock, InodeNumber, LogicalBlock, PhysicalBlock,
};
use vfs::{Ext4MountOptions, Ext4SuperOperations};
pub use xattr::{
    Ext4Xattr, Ext4XattrNameRef, Ext4XattrNameSink, Ext4XattrNamespace, Ext4XattrSetMode,
};

fn ext4_get_tree(
    context: &mut kvfs::FsContext<'_>,
    lookup_root: &kvfs::Path,
    lookup_pwd: &kvfs::Path,
) -> kvfs::VfsResult<alloc::sync::Arc<kvfs::SuperBlock>> {
    let options = *context.private::<Ext4MountOptions>()?;
    kvfs::get_tree_bdev(context, lookup_root, lookup_pwd, move |super_block| {
        Ext4SuperOperations::fill_super(super_block, options)
    })
}

fn ext4_reconfigure(context: &mut kvfs::FsContext<'_>) -> kvfs::VfsResult<()> {
    let options = *context.private::<Ext4MountOptions>()?;
    let super_block = context.super_block()?;
    if let Some(statfs_mode) = options.statfs_mode {
        sync::write_lock(super_block.private::<sync::RwLock<Ext4SbInfo>>()?)
            .set_statfs_mode(statfs_mode);
    }
    Ok(())
}

static FS_CONTEXT_OPERATIONS: kvfs::FsContextOperations =
    kvfs::FsContextOperations::with_reconfigure(ext4_get_tree, ext4_reconfigure);

fn init_fs_context(context: &mut kvfs::FsContext<'_>) -> kvfs::VfsResult<()> {
    context.set_operations(&FS_CONTEXT_OPERATIONS);
    context.set_private(Ext4MountOptions::parse(context.data())?);
    Ok(())
}

static FILE_SYSTEM_TYPE: kvfs::FileSystemType =
    kvfs::FileSystemType::device_backed("ext4", init_fs_context);

#[macros::register_init]
fn init_ext4_fs() {
    kvfs::register_filesystem(&FILE_SYSTEM_TYPE).expect("ext4 filesystem type must register once");
}
