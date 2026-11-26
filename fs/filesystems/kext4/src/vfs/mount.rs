// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mounted core access and its safe metadata-cache maintenance boundary.

use alloc::sync::Arc;
use core::ops::{Deref, DerefMut};

use kvfs::{SuperBlock, TimestampLimits};

use crate::{
    Ext4Error, Ext4Result, Ext4SbInfo,
    superblock::Ext4Storage,
    sync::{RwLock, RwLockReadGuard, RwLockWriteGuard},
};

/// Per-access inspection budget, independent of journal checkpoint cadence.
const METADATA_CACHE_SCAN_LIMIT: usize = 64;

/// Serializes core mutations and services cache demand after each core access.
///
/// Allocators run under a write guard. Reclaim runs before either guard releases
/// mount protection, after the operation's allocator locks and prefetch windows
/// have ended. Thus read-only traffic can advance reclaim without allowing an
/// eviction between an allocator's prefetch and authoritative in-lock re-read.
/// Without cache demand, maintenance only reads a hint and skips the cache lock.
pub(crate) struct Ext4Mount {
    state: RwLock<Ext4SbInfo<'static>>,
    // Geometry and journal identity are frozen before mount publication.
    // Buffered writes must not wait for persistence to read these capabilities.
    writable_features: Ext4Result<()>,
    bitmap_max_file_size: u64,
    timestamp_limits: TimestampLimits,
    has_huge_file: bool,
    storage: Arc<Ext4Storage>,
    // Acquire before mount write access. File sync and writeback can release
    // admission during persistence once the transaction snapshots are stable.
    metadata_mutation: RwLock<()>,
}

impl Ext4Mount {
    /// Installs one core state with its mount access and maintenance boundary.
    pub(crate) fn new(state: Ext4SbInfo<'static>) -> Self {
        Self {
            writable_features: state.ensure_quota_feature_supported(),
            bitmap_max_file_size: state.bitmap_max_file_size(),
            timestamp_limits: state.timestamp_limits(),
            has_huge_file: state.superblock().features().has_huge_file(),
            storage: state.storage.clone(),
            metadata_mutation: RwLock::new(()),
            state: RwLock::new(state),
        }
    }

    /// Checks the retained journal's live abort state without mount exclusion.
    /// VFS mount writability remains a separate check at write admission.
    pub(crate) fn ensure_writable(&self) -> Ext4Result<()> {
        if self
            .storage
            .journal
            .as_ref()
            .is_some_and(|journal| journal.is_aborted())
        {
            return Err(Ext4Error::JournalAborted);
        }
        Ok(())
    }

    pub(crate) const fn bitmap_max_file_size(&self) -> u64 {
        self.bitmap_max_file_size
    }

    pub(crate) const fn timestamp_limits(&self) -> TimestampLimits {
        self.timestamp_limits
    }

    pub(crate) const fn has_huge_file(&self) -> bool {
        self.has_huge_file
    }

    /// Completes file durability after PageCache settled the caller's range.
    pub(super) fn sync_inode(
        &self,
        sb: &SuperBlock,
        inode: &crate::Ext4Inode,
        intent: crate::Ext4SyncIntent,
    ) -> Ext4Result<()> {
        self.writable_features?;
        let snapshot = inode.sync_target(intent);
        if !snapshot.needs_timestamp_staging && snapshot.has_durable_metadata() {
            let result =
                self.storage
                    .sync_data(inode, snapshot.generation, snapshot.needs_data_barrier);
            self.publish_abort(sb);
            return result;
        }
        let mut state = self.writeback(sb);
        self.ensure_writable()?;
        if intent.requires_full_metadata() && inode.has_dirty_timestamps() {
            state.write_inode_timestamps(inode)?;
        }
        state.sync_inode(inode, intent)
    }

    /// Metadata admission stabilizes quota/orphan state across the journal drain.
    pub(crate) fn sync_filesystem(
        &self,
        sb: &SuperBlock,
        sync_quotas: bool,
    ) -> kvfs::VfsResult<()> {
        let _admission = self.metadata_mutation.write();
        let mut state = Ext4SbInfo::Mounted {
            state: self.state.write(),
            writeback_admission: None,
        };
        let quota = if sync_quotas {
            state.sync_quotas(sb)
        } else {
            Ok(())
        };
        let sync = if state.ensure_quota_feature_supported().is_ok() {
            let clear_orphan_present = state.can_clear_orphan_present();
            drop(state);
            clear_orphan_present.and_then(|clear| self.storage.sync_filesystem(clear))
        } else {
            Ok(())
        };
        self.publish_abort(sb);
        quota.and(sync.map_err(super::into_vfs_err))
    }

    fn publish_abort(&self, sb: &SuperBlock) {
        if self
            .storage
            .journal
            .as_ref()
            .is_some_and(|journal| journal.is_aborted())
        {
            sb.mark_read_only();
        }
    }

    /// Enters a shared core operation; may sleep until mutations complete.
    pub(crate) fn read(&self) -> Ext4MountReadGuard<'_> {
        Ext4MountReadGuard(self.state.read())
    }

    /// Enters an exclusive core operation, including allocator prefetch windows.
    ///
    /// `super_block` must own this mount. Journal abort publishes its read-only
    /// flag before the guard releases mutation protection.
    pub(crate) fn write<'a>(&'a self, super_block: &'a SuperBlock) -> Ext4MountWriteGuard<'a> {
        let admission = self.metadata_mutation.write();
        Ext4MountWriteGuard {
            state: Ext4SbInfo::Mounted {
                state: self.state.write(),
                writeback_admission: None,
            },
            super_block,
            _admission: Some(admission),
        }
    }

    /// Inode writeback may admit another metadata operation during log I/O.
    /// Compound namespace operations use ordinary write access to preserve
    /// their lookup and validation results until the mutation completes.
    pub(super) fn writeback<'a>(&'a self, super_block: &'a SuperBlock) -> Ext4MountWriteGuard<'a> {
        let guard = self.metadata_mutation.write();
        Ext4MountWriteGuard {
            state: Ext4SbInfo::Mounted {
                state: self.state.write(),
                writeback_admission: Some(crate::superblock::WritebackAdmission {
                    lock: &self.metadata_mutation,
                    guard: Some(guard),
                }),
            },
            super_block,
            _admission: None,
        }
    }
}

/// Shared core access; drop performs bounded, I/O-free cache maintenance.
pub(crate) struct Ext4MountReadGuard<'a>(RwLockReadGuard<'a, Ext4SbInfo<'static>>);

impl Deref for Ext4MountReadGuard<'_> {
    type Target = Ext4SbInfo<'static>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for Ext4MountReadGuard<'_> {
    fn drop(&mut self) {
        let _ = self.0.metadata_io.reclaim_cache(METADATA_CACHE_SCAN_LIMIT);
    }
}

/// Exclusive core access; drop occurs after the operation's allocator work.
pub(crate) struct Ext4MountWriteGuard<'a> {
    state: Ext4SbInfo<'a>,
    super_block: &'a SuperBlock,
    _admission: Option<RwLockWriteGuard<'a, ()>>,
}

impl<'a> Deref for Ext4MountWriteGuard<'a> {
    type Target = Ext4SbInfo<'a>;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl DerefMut for Ext4MountWriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}

impl Drop for Ext4MountWriteGuard<'_> {
    fn drop(&mut self) {
        // Mount-unlocked sync publishes abort at its own return boundary.
        if self
            .state
            .journal
            .as_ref()
            .is_some_and(|journal| journal.is_aborted())
        {
            // Publish the write ban before releasing mount mutation protection,
            // including for writes which would otherwise only dirty PageCache.
            self.super_block.mark_read_only();
        }
        let _ = self
            .state
            .metadata_io
            .reclaim_cache(METADATA_CACHE_SCAN_LIMIT);
    }
}
