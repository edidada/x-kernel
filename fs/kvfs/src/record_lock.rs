// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Advisory byte-range locks owned by descriptor tables.
//!
//! Callers serialize acquisition with descriptor removal using the table lock.
//! All APIs require sleepable task context. Actual waiting takes place after
//! releasing descriptor and registry locks; dropping a wait cancels its graph edge.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    vec::Vec,
};
use core::{future::poll_fn, task::Poll};

use kerrno::{KError, KResult, LinuxError};
use kpoll::{PollEvent, PollRegistrations};
use ksync::{Mutex, static_lock};

use crate::VfsInode;

type OwnerId = u64;
type FileId = usize;

/// Advisory lock access mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordLockKind {
    /// Shared access, compatible with other readers.
    Read,
    /// Exclusive access, conflicting with readers and writers.
    Write,
}

/// Validated inclusive byte range within signed 64-bit file offsets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordLockRange {
    start: u64,
    end: u64,
}

impl RecordLockRange {
    /// Creates a nonempty inclusive range. `i64::MAX` represents through EOF.
    ///
    /// # Errors
    /// Returns `InvalidInput` for reversed or unrepresentable bounds.
    pub fn new(start: u64, end: u64) -> KResult<Self> {
        if start > end || end > i64::MAX as u64 {
            return Err(KError::InvalidInput);
        }
        Ok(Self { start, end })
    }

    /// Returns the first locked byte.
    pub fn start(self) -> u64 {
        self.start
    }

    /// Returns the last locked byte, or `i64::MAX` for through EOF.
    pub fn end(self) -> u64 {
        self.end
    }

    fn overlaps(self, other: Self) -> bool {
        self.start <= other.end && other.start <= self.end
    }
}

/// A conflicting lock reported by a query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordLockConflict {
    /// Mode of the existing lock.
    pub kind: RecordLockKind,
    /// Existing lock's normalized range.
    pub range: RecordLockRange,
    /// TGID supplied when that lock was established.
    pub pid: i32,
}

#[derive(Clone, Copy)]
struct HeldLock {
    owner: OwnerId,
    conflict: RecordLockConflict,
}

#[derive(Default)]
struct LockRanges(Vec<HeldLock>);

impl LockRanges {
    fn blockers(
        &self,
        owner: OwnerId,
        range: RecordLockRange,
        kind: RecordLockKind,
    ) -> BTreeSet<OwnerId> {
        self.0
            .iter()
            .filter(|held| {
                held.owner != owner
                    && held.conflict.range.overlaps(range)
                    && (kind == RecordLockKind::Write
                        || held.conflict.kind == RecordLockKind::Write)
            })
            .map(|held| held.owner)
            .collect()
    }

    fn conflict(
        &self,
        owner: OwnerId,
        range: RecordLockRange,
        kind: RecordLockKind,
    ) -> Option<RecordLockConflict> {
        self.0
            .iter()
            .find(|held| {
                held.owner != owner
                    && held.conflict.range.overlaps(range)
                    && (kind == RecordLockKind::Write
                        || held.conflict.kind == RecordLockKind::Write)
            })
            .map(|held| held.conflict)
    }

    fn replace(
        &mut self,
        owner: OwnerId,
        range: RecordLockRange,
        kind: Option<RecordLockKind>,
        pid: i32,
    ) {
        let mut updated = Vec::with_capacity(self.0.len() + 2);
        for held in self.0.drain(..) {
            if held.owner != owner || !held.conflict.range.overlaps(range) {
                updated.push(held);
                continue;
            }
            if held.conflict.range.start < range.start {
                let mut left = held;
                left.conflict.range.end = range.start - 1;
                updated.push(left);
            }
            if held.conflict.range.end > range.end {
                let mut right = held;
                right.conflict.range.start = range.end + 1;
                updated.push(right);
            }
        }
        if let Some(kind) = kind {
            updated.push(HeldLock {
                owner,
                conflict: RecordLockConflict { kind, range, pid },
            });
        }
        updated.sort_unstable_by_key(|held| (held.owner, held.conflict.range.start));
        let mut merged: Vec<HeldLock> = Vec::with_capacity(updated.len());
        for held in updated {
            if let Some(last) = merged.last_mut()
                && last.owner == held.owner
                && last.conflict.kind == held.conflict.kind
                && held.conflict.range.start <= last.conflict.range.end + 1
            {
                last.conflict.range.end = last.conflict.range.end.max(held.conflict.range.end);
                continue;
            }
            merged.push(held);
        }
        self.0 = merged;
    }
}

struct FileLocks {
    // Pin identity while records or waiters exist; pointer keys cannot be reused.
    _inode: Arc<VfsInode>,
    ranges: LockRanges,
    changed: Arc<PollEvent>,
    waiter_count: usize,
}

#[derive(Clone, Copy)]
struct PendingLock {
    file: FileId,
    range: RecordLockRange,
    kind: RecordLockKind,
}

struct Registry {
    next_id: u64,
    files: BTreeMap<FileId, FileLocks>,
    owner_files: BTreeMap<OwnerId, BTreeSet<FileId>>,
    waits: BTreeMap<OwnerId, BTreeMap<u64, PendingLock>>,
}

impl Registry {
    const fn new() -> Self {
        Self {
            next_id: 0,
            files: BTreeMap::new(),
            owner_files: BTreeMap::new(),
            waits: BTreeMap::new(),
        }
    }

    fn allocate_id(&mut self) -> u64 {
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("record lock identities exhausted");
        self.next_id
    }

    fn has_cycle(&self, owner: OwnerId, blockers: BTreeSet<OwnerId>) -> bool {
        let mut pending: Vec<_> = blockers.into_iter().collect();
        let mut visited = BTreeSet::new();
        while let Some(blocker) = pending.pop() {
            if blocker == owner {
                return true;
            }
            if !visited.insert(blocker) {
                continue;
            }
            if let Some(waits) = self.waits.get(&blocker) {
                for wait in waits.values() {
                    if let Some(file) = self.files.get(&wait.file) {
                        pending.extend(file.ranges.blockers(blocker, wait.range, wait.kind));
                    }
                }
            }
        }
        false
    }

    fn remove_empty_file(&mut self, file: FileId) -> Option<FileLocks> {
        if self
            .files
            .get(&file)
            .is_some_and(|state| state.ranges.0.is_empty() && state.waiter_count == 0)
        {
            self.files.remove(&file)
        } else {
            None
        }
    }

    fn track_owner(&mut self, owner: OwnerId, file: FileId) {
        if self
            .files
            .get(&file)
            .is_some_and(|state| state.ranges.0.iter().any(|held| held.owner == owner))
        {
            self.owner_files.entry(owner).or_default().insert(file);
        } else if let Some(files) = self.owner_files.get_mut(&owner) {
            files.remove(&file);
            if files.is_empty() {
                self.owner_files.remove(&owner);
            }
        }
    }
}

static_lock! {
    static REGISTRY: Mutex<Registry> = Mutex::new(Registry::new());
}

fn file_id(inode: &Arc<VfsInode>) -> FileId {
    Arc::as_ptr(inode) as usize
}

/// Descriptor-table lock owner; copied tables must construct a new owner.
///
/// Sharing the table shares this identity. Fork copies descriptors without
/// inheriting records. Dropping the final table releases all remaining records.
pub struct RecordLockOwner(OwnerId);

impl Default for RecordLockOwner {
    fn default() -> Self {
        Self(REGISTRY.lock().allocate_id())
    }
}

impl RecordLockOwner {
    /// Queries a conflict, excluding this owner's own records.
    pub fn conflict(
        &self,
        inode: &Arc<VfsInode>,
        range: RecordLockRange,
        kind: RecordLockKind,
    ) -> Option<RecordLockConflict> {
        REGISTRY
            .lock()
            .files
            .get(&file_id(inode))?
            .ranges
            .conflict(self.0, range, kind)
    }

    /// Replaces this owner's records over a range, or unlocks it with `None`.
    ///
    /// A blocking conflict returns a cancellable wait, without sleeping here.
    /// Hold the descriptor-table lock through this call to serialize with close.
    ///
    /// # Errors
    /// Returns `WouldBlock` for a nonblocking conflict or `EDEADLK` for a wait
    /// cycle. Failure preserves every held record.
    pub fn set(
        &self,
        inode: &Arc<VfsInode>,
        range: RecordLockRange,
        kind: Option<RecordLockKind>,
        pid: i32,
        can_wait: bool,
    ) -> KResult<Option<RecordLockWait>> {
        let id = file_id(inode);
        let mut registry = REGISTRY.lock();
        if let Some(kind) = kind {
            let blockers = registry
                .files
                .get(&id)
                .map(|state| state.ranges.blockers(self.0, range, kind))
                .unwrap_or_default();
            if !blockers.is_empty() {
                if !can_wait {
                    return Err(KError::WouldBlock);
                }
                if registry.has_cycle(self.0, blockers) {
                    return Err(LinuxError::EDEADLK.into());
                }
                let wait_id = registry.allocate_id();
                let state = registry
                    .files
                    .get_mut(&id)
                    .expect("conflicting file has lock state");
                state.waiter_count += 1;
                let changed = state.changed.clone();
                let generation = changed.generation();
                registry.waits.entry(self.0).or_default().insert(
                    wait_id,
                    PendingLock {
                        file: id,
                        range,
                        kind,
                    },
                );
                return Ok(Some(RecordLockWait {
                    owner: self.0,
                    id: wait_id,
                    file: id,
                    changed,
                    generation,
                }));
            }
        } else if !registry.files.contains_key(&id) {
            return Ok(None);
        }
        let state = registry.files.entry(id).or_insert_with(|| FileLocks {
            _inode: inode.clone(),
            ranges: LockRanges::default(),
            changed: Arc::new(PollEvent::new()),
            waiter_count: 0,
        });
        state.ranges.replace(self.0, range, kind, pid);
        let wake = state.changed.notify_defer_wake();
        registry.track_owner(self.0, id);
        let retired = registry.remove_empty_file(id);
        drop(registry);
        wake.wake();
        drop(retired);
        Ok(None)
    }

    /// Releases all records on an inode when any descriptor for it is closed.
    ///
    /// Call while descriptor removal is serialized against lock acquisition.
    pub fn release_file(&self, inode: &Arc<VfsInode>) {
        let id = file_id(inode);
        let mut registry = REGISTRY.lock();
        let Some(state) = registry.files.get_mut(&id) else {
            return;
        };
        let old_len = state.ranges.0.len();
        state.ranges.0.retain(|held| held.owner != self.0);
        let wake = (old_len != state.ranges.0.len()).then(|| state.changed.notify_defer_wake());
        registry.track_owner(self.0, id);
        let retired = registry.remove_empty_file(id);
        drop(registry);
        if let Some(wake) = wake {
            wake.wake();
        }
        drop(retired);
    }
}

impl Drop for RecordLockOwner {
    fn drop(&mut self) {
        let mut registry = REGISTRY.lock();
        let files = registry.owner_files.remove(&self.0).unwrap_or_default();
        let mut wakes = Vec::new();
        let mut retired = Vec::new();
        for id in files {
            if let Some(state) = registry.files.get_mut(&id) {
                state.ranges.0.retain(|held| held.owner != self.0);
                wakes.push(state.changed.notify_defer_wake());
            }
            retired.extend(registry.remove_empty_file(id));
        }
        drop(registry);
        for wake in wakes {
            wake.wake();
        }
        drop(retired);
    }
}

/// One pending request, including its deadlock graph registration.
///
/// The caller retains the descriptor-table handle while waiting. Drop cancels
/// the request on success, interruption, registration failure, or abandonment.
pub struct RecordLockWait {
    owner: OwnerId,
    id: u64,
    file: FileId,
    changed: Arc<PollEvent>,
    generation: usize,
}

impl RecordLockWait {
    /// Waits interruptibly until the inode's lock state changes.
    ///
    /// Call without descriptor or registry locks, then retry acquisition.
    /// # Errors
    /// Returns `Interrupted` for a task signal and propagates registration errors.
    pub fn wait(&self) -> KResult<()> {
        let mut registrations = PollRegistrations::new();
        ktask::future::block_on(ktask::future::interruptible(poll_fn(|cx| {
            if self.changed.has_changed_since(self.generation) {
                return Poll::Ready(Ok(()));
            }
            let mut context = registrations.context(cx);
            if let Err(error) = self.changed.register(&mut context) {
                return Poll::Ready(Err(match error {
                    kpoll::PollRegisterError::InvalidState => KError::InvalidInput,
                    _ => KError::NoMemory,
                }));
            }
            drop(context);
            if self.changed.has_changed_since(self.generation) {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        })))
        .map_err(KError::from)?
    }
}

impl Drop for RecordLockWait {
    fn drop(&mut self) {
        let mut registry = REGISTRY.lock();
        if let Some(waits) = registry.waits.get_mut(&self.owner) {
            waits.remove(&self.id);
            if waits.is_empty() {
                registry.waits.remove(&self.owner);
            }
        }
        if let Some(state) = registry.files.get_mut(&self.file) {
            state.waiter_count -= 1;
        }
        let retired = registry.remove_empty_file(self.file);
        drop(registry);
        drop(retired);
    }
}

#[cfg(unittest)]
mod tests;
