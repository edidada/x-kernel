# KExt4 — Design Document

## Prepared VFS mount owner

`fill_super` prepares `KBox<Ext4Mount>` with NOWAIT before transferring it to
`SuperBlock::initialize_with_private`. This path already owns a nascent canonical
block-device identity and exclusive claim: owner allocation must not newly enter
reclaim that could wait for mount completion. No mount-state lock is held at this
allocation. NOWAIT is a conservative local policy for that initialization phase,
not a claim that the whole fill operation is atomic or non-sleeping.

## Prepared writeback callback owners

The per-inode completion worker and per-batch finish callback use `KBox` owners.
Both owner allocations use NOWAIT: writeback submission holds inode submission
exclusion, and worker initialization additionally holds its initialization mutex.
These paths are sleepable tasks, but reclaim could need the same writeback
resources. NOWAIT does not make WorkHandle preparation or the full path atomic.

Worker preparation failure leaves the cached handle absent and retryable. Finish
storage is reserved before moving the callback, installing the request or
submitting BIOs. On reservation failure, the caller records the error before
the still-local finish callback drops and releases/redirties cache completion
captures. Successful reservation is initialized in place without another
allocation. Existing ordered completion and task-context capture release remain
unchanged. `Ext4Error::Allocation` preserves the kernel error through `From` and
the existing VFS conversion, including NoMemory.

## Shared metadata snapshots

Metadata cache buffers, journal superblock images and frozen transaction lists
convert owned vectors directly into shared slices with `Arc::from`, without an
intermediate boxed slice. Mutable preparation still uses `Vec`; published
snapshots retain their existing `Arc` ownership and lifetime. Metadata updates
keep the contents lock across transaction validation, copying and publication.

## Test Layout

Guest unit, component and recovery tests live under `ut-tests/`, mirroring the
owning source module. Shared fault devices and superblock fixtures live there
too. `unittest::path_mod_test!` retains private implementation access without
turning these kernel fixtures into Cargo host integration tests.

## Positioning

`fs/filesystems/kext4` is the ext4 filesystem implementation of X-Kernel. It owns both the KVFS operation
objects and the private ext4 algorithms, and is responsible for on-disk format parsing, metadata checksums, JBD2 recovery and transactions, extents,
the allocator, truncate/orphan, namespace mutation, xattr, and ordered-data writeback.
At runtime there is no longer a `kext4_vfs` bridge crate or a separate superblock/inode wrapper.

The compat, incompat and ro-compat feature fields of the on-disk superblock are each represented by a separate
`bitflags` type. Decoding uses `from_bits_retain` to preserve unknown on-disk bits, which avoids conflating bits with the same numeric value in different feature
classes, and ensures that mount negotiation can still report the original unsupported bits.

The current `Ext4SbInfo` mount state publishes both the read and mutation APIs, and the type system does not yet distinguish a read-only
legacy block-map mount. Mount negotiation therefore requires the superblock to enable `EXT4_FEATURE_INCOMPAT_EXTENTS`:
a whole-disk `^extents` format returns
`Unsupported(NonExtentFilesystem)` before reading the group descriptors, replaying the journal, or publishing the VFS root. An image with the extents feature can still have individual legacy inodes parsed read-only,
but its buffered-write reservation, `page_mkwrite` and subsequent mutations return
`Unsupported(NonExtentInode)` before copying or dirtying user data. Fully allowing the whole-disk legacy format must also implement
allocation, freeing, truncate and resize for direct/single/double/triple indirect blocks; it is not enough to merely remove the mount check.

## Background

KExt4 is the only ext4 backend of X-Kernel. It maintains checked Rust code, explicit feature negotiation,
journaled metadata updates, and interoperability with e2fsprogs/e2fsck. `KFEAT_FS_EXT4` links KExt4 directly into the runtime,
without going through a bridge or an implementation selection layer. N0 has completed the KVFS single resident inode identity,
cached attributes, two-phase truncate, and the open-unlink/final-evict baseline. N1 has established the persistent journal
and the mount/journal lifecycle boundary, N2 continues by establishing the buffered concurrent execution framework, and N3 then fills in crash
recovery, error observation, and unmount/freeze in one place.

## Scope

Main source files:

```text
fs/filesystems/kext4/
├── src/lib.rs
├── src/vfs.rs
├── src/superblock.rs
├── src/journal.rs
├── src/jbd2/
├── src/buffer/
├── src/extent/
├── src/balloc.rs
├── src/ialloc.rs
├── src/mballoc.rs
├── src/prealloc_space.rs
├── src/falloc.rs
├── src/file.rs
├── src/truncate.rs
├── src/orphan.rs
├── src/namei.rs
├── src/dir.rs
├── src/dirhash.rs
└── src/xattr.rs
```

## Architecture

`Ext4AllocationRequest` borrows its owning `Ext4Inode` and carries the file-logical
position into multiblock allocation. An explicit physical goal selects the starting
group. Without a goal, data allocation starts in the inode's group and metadata
allocation starts in group zero. The request does not cache a separate locality
group. Directory, extent-tree and xattr allocation, and the corresponding release
paths, receive their actual owner. Group bitmap primitives update physical
allocation and free-space counts; inode entry points retain the ownership boundary.
Preallocation indexes retain a strong inode reference beyond the call and check
that it identifies the same object borrowed by the request before allocation.

`fill_super` calls `prepare_recovery` on the sole `Ext4SbInfo`, replays the journal,
and refreshes the disk mirror, directory hash policy, file-size limits and protected
block ranges before fixing VFS mount capabilities and installing `Ext4Mount`.
`finish_recovery` then cleans orphans through the installed owner before creating
the root inode and publishing the root directory. Replay, orphan cleanup and normal
operations retain the same mount and journal owner. The phases pass existing
journal replay evidence without persistent phase fields. A clean filesystem does
not perform recovery writes. The explicit device-recovery test entry uses these
same algorithms.

Recovery refresh reads and validates one superblock image, checks its journal
backing identity and derives its directory hash policy before installing the
mirror and allocator counters. It then rebuilds file-size limits and protected
ranges. Ordinary checkpoint reload uses the same read/validation and allocator
reload algorithms while preserving the existing mount mirror and hash policy.

`build_system_zones` derives the complete protected-block index from filesystem
geometry and the mounted journal's physical extents. Initial mount retains the
metadata index used to validate the backing inode and completes it with journal
ranges. Recovery builds a fresh replacement through the same checked insertion
path. Ranges remain sorted and
non-overlapping, and adjacent ranges merge only when they have the same owner.
The previous derived index is not an input to recovery reconstruction. A refresh also
rejects changes to the loaded journal's backing identity before assigning an owner
to its existing physical mapping.

```text
KVFS syscall / PageCache
    |
    v
fs/filesystems/kext4
    |-- Ext4SuperOperations (static KVFS s_op, no mount identity)
    |-- Ext4AddressSpaceOperations (static KVFS a_ops, no inode identity)
    |-- Ext4Mount { RwLock<Ext4SbInfo> } (the sole ext4 s_fs_info object)
    |-- layout / feature negotiation / device
    |-- HTree hash_unsigned mount policy
    |-- metadata buffer / checksum validation
    |-- group descriptors / allocator state
    |     |-- per-group free-extent cache
    |     |-- orphan head and RECOVER/ORPHAN_PRESENT bits
    |     `-- speculative prealloc space two-sided index (inode logical map + group twin table)
    |-- MountedJournal (similar to journal_t)
    |     |-- internal journal mapping / on-disk superblock
    |     |-- transaction state
    |     |     `-- one object: Running -> Committing -> Checkpoint -> Finished
    |     `-- FIFO checkpoint queue / runtime head-tail state
    |-- delayed-allocation mount aggregate
    `-- extent / orphan / namei / xattr / truncate algorithms
           ^
           `-- borrow Ext4Inode private state composed in the VFS inode
    v
block::BlockDevice
```

The core modification path enters a transaction through `JournalHandle`, first obtains metadata create/write access through the buffer layer,
and then modifies the metadata bytes and the in-memory layout state. `MountedJournal` is the only journal identity on the production path:
it owns the on-disk superblock, the ring runtime state, and the transaction state. A single transaction object goes through the
Running, Committing, Checkpoint and Finished phases in turn; the FIFO queue holds only that object itself, and the persistence evidence
belongs to its Checkpoint phase, with no copy of the commit payload and no reference pointing back to another coordinator.
An ordinary mutation is not responsible for synchronously completing the home-block checkpoint.

## Whole-Block File Reads and Cache Commit

`Ext4SbInfo::read_inode_bytes` 按映射 run 读取文件。只有 initialized 映射、文件偏移位于
filesystem block 边界且本段长度为整块倍数时，才把调用者已有的目标切片交给
`FilesystemDevice::read_blocks`；省去该段临时零初始化 `Vec` 的分配和一次回拷。
目标切片可以从任意字节地址开始；`BlockDevice` 的同步读写将该范围直接交给 Bio，
由后端与既有 DMA 能力层解决设备地址对齐、连续性、映射及缓存维护要求。
同步调用在设备访问结束后才返回，通用层不复制完整块缓冲区。
ext4 不保存设备对齐常量或驱动清单，也不为设备地址要求额外创建回退缓冲。
`FilesystemDevice` validates filesystem bounds and buffer lengths, then converts
filesystem blocks directly to 512-byte sectors. Queue geometry, live media bounds
and read-only policy are checked by block core; no device geometry is cached.
部分块仍需 staging 来提取文件范围，与设备地址要求无关；驱动的 DMA/bounce buffer 所有权
协议不变，这不是端到端零拷贝。

部分首尾块继续用 staging buffer；hole 和 unwritten extent 仍只填零，返回长度仍受 inode
EOF 限制。`read_at` 返回错误时目标缓冲可能已被后端修改，不能把其前缀当作成功读取。
KExt4's `read_folio` and readahead share `vfs/read.rs`: block mapping runs under
the core read guard, then `FolioRead` owners move directly into bios. Physically
contiguous pages are submitted together without waiting for data I/O. Completion
recovers the original tokens, zeroes bytes beyond the sampled EOF, and publishes
the whole-page result before unlocking; it takes no inode, mapping, or core lock.
Holes and unwritten extents are zeroed directly. For 1/2 KiB filesystem blocks,
fragmented pages and data following a hole split initialization into disjoint
`FolioReadPart` owners at maximal physical-run boundaries. Adjacent mapped
blocks share one part and one bio, while consecutive holes/unwritten blocks
share a zeroed part. A logical hole or physical discontinuity ends a data run;
physical adjacency across a hole cannot merge the destinations. Submission
never waits for data I/O. The last part
releases the page lock; only success for every part publishes uptodate. This
temporary convergence state exists only for split reads, not in every folio.
The ownership layers correspond to Linux `fs/ext4/readpage.c`'s
`ext4_mpage_readpages`/`mpage_end_io` and `fs/buffer.c`'s
`block_read_full_folio`/`end_buffer_async_read`: ext4 owns block mapping and bio
submission; the generic cache owns disjoint initialization and whole-folio
publication. Linux's fragmented fallback submits buffer heads individually;
this path coalesces adjacent ranges before submission while preserving those
completion responsibilities. Consuming tokens replace persistent buffer-head
read flags and the final completion scan for these full-folio reads.
Filesystem blocks larger than the 4 KiB cache page retain the synchronous
partial-block reader. Metadata and symlink reads also remain synchronous.
Failed pages remain non-uptodate until a demand retry fully initializes them.

`ext4_truncate_drains_fragmented_readahead_before_freeing_blocks` uses writable
1/2 KiB ext4 allocator/journal fixtures and the real VFS truncate entry. Demand
returns after its own bio, with both fragmented readahead runs retained by the
test device. After shrink publishes i_size and blocks on that folio, both extent
mappings and free-block counts remain unchanged, including after a failed first
part. Only final completion permits block release. Regrowth then verifies the
retained 17-byte prefix and zeroed tail. The rendezvous observes task state and
held bios; it has no clock threshold or fixed yield count.

`file/read_tests.rs` uses combinations of 1/2/4 KiB filesystem blocks and 512 B/4 KiB device blocks, covering
aligned direct reads, unaligned-slice fallback, short buffers, holes, unwritten extents, and a non-zero 17-byte EOF tail block; the mock likewise rejects
underlying transfers that are not 4-byte aligned, but is placed behind the production block adapter; an unaligned call must still succeed with correct contents,
and the device bounce must not copy the staged contents of a failed block back to the caller. `vfs/read_tests.rs` makes a real
ext4 backing read modify a 17-byte prefix and then keep returning I/O errors, verifying that a single page stays non-uptodate, that batched
readahead does not publish the prefix, that a buffered read does not copy the failed contents, and that the data and the EOF zero fill are correct after a full retry. The writable journal
fixture additionally verifies a partial buffered write after a failed read: while the read error persists, the write must not advance; a successful retry must first read
the page in full, and then modify only the specified bytes, with the read-back preserving the remaining data, so that the failed prefix can never become valid dirty data.

## Calling Constraints / Execution Context

The KExt4 core API may perform block-device I/O, memory allocation, JBD2 transactions, checkpoints and device flushes,
so it is a task-context API: it is allowed to block and must not be called from interrupt context. It is also unsuitable for calling during early boot,
before the block device, the allocator and the journal state are available.

The canonical `FileSystemType` of ext4 is registered by this crate's initcall. `get_tree_bdev` first applies the Linux `sget_dev()` semantics against that static
type object and the canonical `BlockDevice`, obtains an exclusive device claim, and
allocates a nascent KVFS `SuperBlock`; `fill_super` only populates this existing object and does not receive or copy the `s_type`,
device or flags identity. An existing instance with the same identity is reused by the VFS, and on a failed fill the VFS releases the claim and
wakes the waiters.

The KVFS `SuperBlock` stores a stateless `Ext4SuperOperations` and a unique
`RwLock<Ext4SbInfo>` private object separately, corresponding to Linux's separate `s_op`/`s_fs_info`; the operation table
is a genuine Rust `static` shared by all ext4 mounts, not a second filesystem object, and no
`Arc` is allocated for it at mount time. The private object is owned exclusively by the superblock, and `private::<RwLock<Ext4SbInfo>>()` only returns a reference
bound to the superblock borrow. Read-only calls may enter in shared mode, while mutations that still jointly modify superblock/group metadata
are serialized by the write guard. The delalloc mount aggregate is already protected by a separate mutex inside `Ext4SbInfo`,
and reserve/release/truncate reservation only take the shared mount guard and that real counter lock, no longer taking the
mount write guard. This mount-level lock does not protect the KVFS inode cache, the AddressSpace or the open-file lifecycle.
The ext4 private state held by the VFS inode composition is protected by a per-inode
`RwLock<Ext4InodeState>`, which covers the on-disk metadata working state and the delayed-allocation extents:
read-only paths (`size`/`stat`/`metadata_snapshot`/delalloc queries) enter in shared mode through `with_state` with a read guard,
mutations take exclusive access through `update_state` with a write guard; the resident lifecycle belongs entirely to KVFS.
Callers must not assume that the allocator or the journal
already supports per-group parallel modification. The writable-open count of a file is
owned by the KVFS `VfsInode`, and kext4 only reads it in the release callback.

All regular-file/symlink mappings share the same static `Ext4AddressSpaceOperations`. The aops callbacks
obtain the unique `VfsInode` from `mapping->host` and then borrow the `Ext4Inode` private component inside it;
`Ext4Inode` no longer doubles as the aops object, so there are not two inode identities that must be kept consistent.

allocator 可变状态（逐组可变 descriptor 字段、per-group free-extent cache、
free-blocks/free-inodes/directory-inode 全局计数、orphan 头、`RECOVER` 与
`ORPHAN_PRESENT` 位）由 `Ext4SbInfo` 内的
`Mutex<AllocatorState>` 保护；mount-wide delayed-allocation 总量刻意放在该锁之外，
由独立的 `delalloc_reserved_blocks: Mutex<u64>` 保护，对应 Linux
`s_dirtyclusters_counter`（per-CPU 原子计数）的独立更新域——reserve/release 只与
其他 delalloc 修改互相串行，不阻塞全局组分配。静态几何不受本锁变动：
`layout` and `group_geometry` retain the validated mount geometry. Metadata
reload checks a decoded superblock against that geometry before rebuilding
mutable counters. Only recovery refresh replaces the mount mirror and hash policy.
逐组地址（block/inode bitmap、inode table）只保存在挂载时冻结的 `group_geometry`
一维表中——`AllocatorState` 的逐组条目（`GroupMutableState`）只镜像计数、flags 与
bitmap checksum 等可变字段，刻意不含地址字段，地址因此只有一个读取源，对应 Linux
`ext4_get_group_desc(..., NULL)` 对静态地址字段的无锁读，inode 表定位等热路径据此
无锁读。journal replay 可能重写 descriptor block，`reload_mutable_metadata_state`
重解码并校验 replay 后的 descriptor 表：计数等可变字段照常重建，地址字段与冻结表
不一致时 fail-closed 拒绝（KExt4 支持的操作不会 journal 地址变更，变化即损坏或不
支持特性），避免冻结几何与重载状态分裂后操作错误的 bitmap/inode table。取锁前，
balloc/ialloc 入口先完成冻结几何解析与元数据缓存预热（`prefetch_metadata_blocks`），
冷缓存设备读在锁外完成、失败也在锁外返回，不串行化全体分配器（对应 Linux 先读
bitmap buffer 再 `ext4_lock_group`）；返回的 `MetadataBuffer` 集合保留到 allocator guard
释放之后，同时 pin 不可变字节和槽位身份，回收不能在预取与重读之间重建槽位。
权威快照在锁内重读，预热结果绝不作为权威
数据。balloc/ialloc 的
单组分配/释放入口（`*_in_group`）整体为单一临界区：从读组描述符、位图 buffer 读改、
journal write-access 到内存状态发布全程持锁，入口之间不会基于同一位图快照重复选取
同一块/同一 inode，也不会丢失计数更新——对应 Linux 组锁"锁窗覆盖分配决策与
bitmap 更新"的语义。多组扫描入口（`allocate_blocks_for_write()` /
`allocate_inode_with_name()`）不是单一临界区：扫描循环逐组以短锁窥探 hint、随即
释放，再逐个在 `*_in_group` 中重新取锁尝试；由于该锁不可重入，不能在扫描循环内持锁
调用 `*_in_group`，goal 选择与分配也不构成原子操作，组间竞态由 `*_in_group` 锁内
重校验兜底。delalloc 总量更新位于独立小锁的单临界区，读改写不会丢计数；预算准入
检查与累计位于同一临界区（allocator 锁读 free-blocks + delalloc 锁读并累加，
锁序固定 allocator → delalloc），check-then-act 基于同一快照原子完成，不存在
并发超预留窗口，也不依赖 bridge 挂载级写锁的外部串行化；临界区有界（仅读计数
并做一次加法），与 Linux 直接原子累加 percpu 计数的近似记账相比保持精确语义。

The lock order is fixed as the `Ext4Mount` mount-level lock → the allocator lock → the delalloc lock / the prealloc map leaf lock → the metadata-buffer /
journal internal locks, and waiting in the reverse direction is forbidden. The delalloc total lock and the per-inode prealloc map leaf lock are both
independent leaf locks beneath the allocator lock and never nest with each other: the former only protects the mount-wide counter, and the latter only protects the logical-range table of a single file; the `update` closure takes and releases that leaf lock
inside the allocator critical section and acquires no other lock; when group-side discard needs to lock the map of a foreign owner, it takes it in the
same order, so prealloc operations form no ABBA with the inode-state lock. While holding the allocator lock, no path that re-takes that lock may be called:
this prohibition covers not only the public entry points but also the lock-taking accessors (`groups()`/`free_blocks_count()`/`free_inodes_count()`/
`needs_recovery()`/`has_orphan_present()`), since inside the guard the fields under the lock are accessed directly; the ENOSPC retry valve
(`discard_preallocs_for_space()`) is likewise covered by this prohibition: it takes the allocator lock itself, so it can only be placed outside
the locked section of the scan loop;
geometry computations such as inode lookup
read the frozen `group_geometry` table (lock-free), and allocation paths that hold the lock use the lock-free `*_in_group`
variants (the lock is not reentrant, and ksync panics directly on same-task reentry). The delalloc lock is a separate
`Mutex<u64>`, delalloc paths lock only it, and when reading free-blocks they first take the allocator
short lock according to the lock order above. The alignment target remains Linux's per-group spinlock + percpu counters: the current single lock is a transitional form,
and the critical section is bounded (single-group bitmap scan + in-memory read-modify-write of cached metadata + journal publication; blocking device reads
have been warmed up and moved out of the lock, and only the inode-table block whose address depends on the allocation result is still read under the lock); it will later be refined per group so that allocation in different
groups can resume running in parallel (the Linux `ext4_lock_group` lock window = a single-group scan, never holding a lock across groups).

This mount-level read-write lock is a transitional implementation and not a long-term calling contract. N1 has already brought the on-disk journal, the transaction engine and the
checkpoint queue into the same `MountedJournal` lifetime boundary; metadata, allocator, device and
geometry continue to be held by the `Ext4SbInfo` mount state, corresponding to the aggregating role of Linux `ext4_sb_info`.
N2 establishes journal, per-inode, metadata-buffer and per-group locks according to the real executors, replacing mount-wide
global serialization. The N2 per-group lock acceptance criterion: the lock window must not contain blocking I/O. Bitmaps/descriptors are prefetched outside the lock
(aligned with Linux reading the bitmap buffer first and then calling `ext4_lock_group`), and publication of dirty metadata moves out of the lock
(aligned with Linux calling `ext4_handle_dirty_metadata` only after `ext4_unlock_group`), leaving in the lock window only
pure in-memory bit operations and counter updates; moving publication out must be accompanied by re-validation under the lock and retry on failure, to prevent concurrent observation of a stale
bitmap. Until N2 is complete, adding new blocking I/O or new lock-taking calls to the allocator-lock critical section is forbidden.
All these paths still belong to blockable task context and cannot be called from interrupt context.

Mount first decodes the primary superblock into on-disk state validated for structure and metadata checksum, and then
`Ext4SbInfo::open()` performs feature negotiation; rejection for runtime capabilities such as non-extent happens before layout,
journal recovery and VFS root publication, but it must not take precedence over a checksum failure and thereby change the error semantics.
When the mutable superblock is reloaded after recovery replay, the same mount capability validation is performed again.
On-disk helpers that only update superblock counters, the recovery bit or the orphan head reuse the format decoder and do not carry
mount policy.

## State Machine

### VFS resident inode and ext4 private state

```text
KVFS SuperBlock::get_or_try_init_inode(ino)
  -> absent: reserve New, decode one Ext4Inode private state, publish Live VfsInode
  -> New: wait for initialization
  -> Live: return the existing VfsInode
  -> Freeing: wait for final eviction, then retry

absent -> New -> Live -> Freeing -> absent
```

`VfsInode`/`AddressSpace` is the sole resident identity, and its state hierarchy corresponds to Linux `struct inode`'s
`I_NEW`, the ordinary usable state, and `I_FREEING`. The KVFS-wide `(SuperBlock, ino)` table first claims the `New`
slot, and then lets the sole initializer
decode the `RawInode` and construct the sole `Ext4Inode` private component; if initialization fails, the slot is revoked and the waiters are woken. When the last
VFS reference enters drop, KVFS publishes `Freeing` before calling the filesystem eviction hook; a lookup of the same number
waits for cleanup to complete and then looks up again, so the race is not exposed as `EINVAL` or `ESTALE`.

`RawInode` is only the validated on-disk representation. `Ext4Inode` corresponds to the `ext4_inode_info` private part embedded within the Linux `struct inode`: it has no inode-number cache, resident lifecycle, or eviction
reference count. An ordinary lookup allows only the owner of the KVFS `New` slot to construct this state from disk; unlink, rmdir,
link, and rename must be passed the child/moved/replaced private state already held by the VFS, and the core does not re-decode a live inode by number. Recovery, before the mount/VFS identity is published, may construct temporary state to handle orphans.

A metadata mutation first stages/publishes the journal buffer, and then publishes the result in the same inode component.
This component carries both the generic fields that KVFS accesses through `InodeAttributeOperations` and the ext4-private fields such as
`i_disksize`, the extent root, ext4 flags, and the xattr block; this corresponds to Linux
`ext4_inode_info` embedding `struct inode`, not to two copies of an attribute cache. `i_size` may run ahead of `i_disksize` before ordered
writeback; the two are distinct fields that Linux already has. The kext4 KVFS layer no longer performs post-mutation
metadata snapshot write-back, and does not create a spare resident wrapper or reload live state by inode number.
A regular-file metadata publish only commits ext4 state such as `i_disksize`, and must not modify `i_size` on the grounds that the old `i_size ==
i_disksize`; the latter is published only by the VFS `write_end_set_size()` or
`truncate_setsize()` at the PageCache ordering point. For objects whose length is changed directly by ext4 algorithms, such as directories and symbolic links,
the visible length is published explicitly in the corresponding metadata mutation.

`i_size` is a generic inode attribute, while `i_disksize`, the extent root, and the extent-status proof are ext4-private
mapping state; being composed in the same object does not mean that single-value reads of them must use the same lock. The sole `i_size`
is stored in `Ext4Inode::visible_size: AtomicU64`, moved out of `Ext4InodeState` rather than copied;
`InodeAttributeOperations` accesses this field directly, and creates no extra attribute object or shadow cache.
All supported architectures are 64-bit, which corresponds to the Linux `include/linux/fs.h` `i_size_read()`
acquire load and `i_size_write()` release store: a cache-hit read of EOF does not take the ext4
inode-state lock; a reader that sees a size publication also sees the related modifications made before that publication. This single-value read
does not guarantee that the length will not change afterwards, nor does it pin the block mapping.

Size publication continues to hold the inode-state write lock, so that the shrink's extent-proof clearing and the new EOF are
indivisible to mapping readers; `sizes()`, `stat()`, and the extent-status overwrite proof sample the size under this lock,
excluding concurrent publication of the size and the mapping state. `sizes()` only guarantees that there are no such two kinds of writers during sampling, and does not merge the different publication phases of
`i_size` and `i_disksize` into a transaction; the two may differ during writeback/truncate.
This lock is the synchronization implementation of the current ext4 state, and the generic `i_size` single-value read must not depend on it.

`kind` is decoded once from the on-disk `i_mode` at inode load (iget) and stored as a plain
`Ext4Inode` field that never changes afterwards. The raw `i_flags` has a single resident copy,
the `Ext4Inode` atomic; it is decoded alongside `kind` and never stored in the resident
metadata. Non-format bits change at runtime (e.g. setting `EXT4_INDEX_FL` when a directory
gains an index) and are published only by `set_flags` under the inode-state write lock. The
generic `update_dirty_inode_metadata` raw-byte path must not touch `i_flags` because it does not
synchronize the atomic; flag changes go through `update_inode_flags_timestamps_metadata`, which
rejects a mapping-format change before writing the entry. The mapping-format bit
`EXT4_EXTENTS_FL` is immutable for a resident inode after iget/create:
lock-free readers such as `has_extents()` select the mapping interpreter from it, so `set_flags`
rejects a format-bit change as corruption rather than silently diverging. Write paths read the
plain field for `S_ISREG` and the atomic for `EXT4_EXTENTS_FL` without taking the inode-state
lock, matching Linux's read pattern, where `ext4_iget` fills `inode->i_mode` and `ei->i_flags`
once and both are read lock-free at runtime.

The on-disk `i_mode` is split the same way instead of kept as a full resident copy: the type bits
live only in the immutable `kind`, and the permission/special bits (`i_mode & 0o7777`) live in the
`permissions` atomic. `mode()` composes the two into the full 16-bit `i_mode` at the stat and encode
boundaries; `set_permission` publishes only permission bits with a release store and masks any type
bits, so it cannot move the type. The chmod setattr path publishes the new on-disk mode and then
calls `set_permission` with the same value, keeping disk and resident permission bits in lockstep.
This removes the former `metadata.mode` copy whose type bits duplicated `kind`; the type invariant
is now a construction-time fact (decoded once by `from_raw`, never rewritten) rather than a debug
assertion. Linux keeps type and permissions in the single `inode->i_mode` and tests the type with
`S_IS*`; the split is an equivalent mapping where the type is a frozen classifier and the permission
bitmask is the mutable part.

The VFS `NodeFlags` returned by `InodeAttributeOperations::flags()` are not cached: `node_flags()`
re-derives the disk-projected bits (immutable / append-only / no-atime) from the raw `flags` atomic
on every read and adds the runtime-only `NO_QUOTA`. `NO_QUOTA` lives in its own `no_quota` atomic
(the Linux `S_NOQUOTA` analogue), is set or cleared independently, and is never projected from disk.
Deriving on read keeps the VFS view coherent with `flags` without a second resident copy or a
cross-atomic synchronization window.

The extent-status proof tree additionally tracks the contiguous written prefix
`[0, covered_prefix_end)`; as the prefix grows, the inode atomically publishes
`initialized_extent_end` (in bytes). A delalloc reservation that falls entirely inside that
prefix returns immediately without taking the inode-state write lock — the slow path would only
re-prove the same range and return the same way. Losing a proof to capacity eviction does not
retract the published prefix (the mapping is unchanged); only explicit proof invalidation
(shrink truncate, mapping changes during metadata publication, `invalidate_extent_status`
after an extent mutation) resets the prefix, and only through the protected
`clear_extent_proofs`.

The prefix is published and cleared under the inode-state write lock but read lock-free on the
reservation fast path. Clearing wins any ordering that matters: extent mutations clear the prefix
(`begin_extent_mutation`) before rewriting the tree, and shrink or inode-table block / disk-size
changes clear it while holding the same write lock. A reader that loads the old value therefore
does so before the mapping changes, when the prefix is still valid; a reader that loads after the
clear sees zero and takes the slow path. The fast path deliberately skips the slow path's
delalloc bookkeeping self-check (`reserved_data_blocks > delalloc_reserved_blocks`), which still
guards every range outside the prefix.

The upper-layer inode data lock corresponds to the Linux `i_rwsem` serialization of size transactions; truncate additionally holds the mapping
invalidation exclusive lock. `write_end_set_size()` publishes growth after the page contents are ready,
and `truncate_setsize()` executes in the order of publishing the size, PTE unmap, PageCache resize, and unmap again,
and only afterwards releases the backing blocks. The atomic provides only single-value publication and memory visibility, not these transactions or invalidation mutual exclusion,
and does not change the responsibility that regular-file backing metadata/writeback publishes only `i_disksize`.

### Metadata Transactions

```text
join or create the running transaction, and reserve credits
  -> obtain metadata create/write access
  -> modify metadata bytes and in-memory counters
  -> complete ordered-data dependency
  -> freeze the transaction based on credits, age, space, or explicit sync
  -> before the first journal write, activate the clean journal into a recoverable state
  -> persist the journal commit
  -> advance checkpoint / journal tail independently
```

Meaning:

1. A handle joins the mount-wide running transaction and reserves journal credits according to the mutation type;
   the transaction engine holds the credit limit fixed at mount time, and both `begin()` and in-flight extension validate against that same limit,
   so a caller cannot pass in a different limit to bypass the constraint.
2. Each modified metadata block records undo/write access through the buffer layer.
3. Metadata bytes and in-memory counters are updated within the same transaction.
4. Ordered data completes before the metadata commit that makes it reachable.
5. A commit freezes and persists only the corresponding transaction, and then allows a new running transaction.
6. Checkpoint writes home blocks independently; `fsync`, `syncfs`, unmount, and journal-space pressure wait for the relevant state according to
   their respective durability intent.

In the current implementation the mount holds a single `MountedJournal`, and the journal sequence, the single transaction phase
state, and the checkpoint completion watermark are no longer rebuilt on every mutation. The on-disk journal can
append multiple committed transactions consecutively from the runtime append head: the active journal superblock persists only the sequence/start pointing to the oldest un-checkpointed
transaction; `s_head` is clean/unmount information and is not treated as the runtime head during the active period.
The next append position is determined by the most recent persisted commit together with the in-memory mount state. FIFO checkpoint advances only the tail,
and only when the last transaction completes does it zero start, write the clean head, and clear the ext4 `needs_recovery` flag.
The ring-space computation always keeps one empty block, to avoid the head catching up to the tail and overwriting a descriptor that can still be replayed.
The first commit of a clean journal first persists and flushes a non-zero `s_start`, and only then writes the descriptor/data/commit;
therefore, if power is lost between activation and the commit block, recovery identifies the journal as active and ignores the incomplete transaction,
rather than wrongly skipping the scan as if the journal were empty.

A synchronous commit aggregates the descriptor, data, revoke, and commit blocks into a bounded
write batch of no more than 128 KiB; a batch is split at a journal ring wrap and at a discontiguous physical extent of the internal journal,
and never crosses a non-contiguous disk mapping in order to reduce the number of requests. The complete journal-superblock block image validated at mount time
is cached by `JournalSuperblock`; subsequent sequence/start/feature updates are generated from that image and decoded again,
so journal block 0 is not re-read on the commit hot path. The clean-journal activation flush and the transaction's
final durability flush remain two independent boundaries; `sync_inode` omits the redundant device flush immediately following only when the commit has already completed its final flush,
and a mapped-data overwrite without a metadata transaction still flushes explicitly.

When the ext4 recovery feature is set, only the on-disk recovery evidence and the in-memory feature state are updated; the newer in-memory allocator counters are no longer overwritten with the old home-block superblock that has not yet been
checkpointed. The real Linux ext4
image regression covers committed-transaction scanning, final clean state, and e2fsck; component tests further verify that
two transactions sharing metadata retain the latest version after a batched checkpoint, and that the home/tail barriers are merged.
If a transaction contains the primary superblock, the persist path merges the recovery feature into both the journal
record and that transaction's frozen checkpoint image; therefore, when an older checkpoint completes while a later commit is still pending,
the on-disk `needs_recovery` is not wrongly cleared. That flag is cleared separately only after the journal tail is truly empty.

An ordinary mutation decides success or failure inside the handle, and can share the same running
transaction with subsequent mutations. Before a new handle joins, roughly one third of the journal capacity, derived from the journal format overhead, is used as the ordinary transaction
upper bound; handle stop returns unused credits, so outstanding credits express the real
capacity still occupied by the transaction. An ordinary commit is no longer triggered by a fixed operation count or by "half a journal". Home-block checkpoint
remains in the FIFO queue; `syncfs` and KVFS unmount writeback first commit the current running transaction,
then drain all pending checkpoints; an ordinary mount runs the same sync path again after dentry eviction,
to cover new metadata produced by final inode eviction. A normal commit directly checks the running/committing slots,
and only a retry targeting an old pending transaction scans the FIFO, so that each commit does not bear a cost proportional to the backlog length.

The checkpoint worker performs two kinds of work under the journal persistence execution lock (production still holds the mount write lock): an explicit sync snapshots the entire queue and drains it; when append
returns `JournalBusy`, it selects the shortest contiguous prefix that reaches the space target. Let `A` be the number of descriptor, metadata, revoke, and commit blocks
actually needed by the current frozen transaction, `B` the worst-case journal overhead corresponding to the mounted transaction credit
limit, and `C` the total number of usable ring blocks; the reclaim target is then `min(C - 1, A + B)` writable
journal blocks; physical free space must be strictly greater than this target, still keeping one empty block. Each credit owns at most one
metadata block or revoke record; if a descriptor holds `D` tags and the credit limit is `L`, then
`B = L + ceil(L / D) + 1`. If the number of metadata blocks is `m`, the number of revoke records is `r`, and the capacity of each revoke
block is `R`, then credit accounting guarantees `m + r <= L`, and format capacity validation guarantees `D >= 1` and `R >= 1`.
Therefore the actual encoding overhead satisfies:

```text
m + ceil(m / D) + ceil(r / R) + 1
    <= m + r + ceil(m / D) + 1
    <= L + ceil(L / D) + 1 = B
```

This bound does not require any ordering relationship between `R` and `D`. The number of descriptor/revoke control blocks for a mixed record set may be greater than
the number of descriptors for all-metadata, but replacing one metadata credit with a revoke saves at least one
full metadata data block, which is enough to pay for at most one revoke journal block; the trailing `+1` is the commit record.
Therefore the budget covers any legal mix, with no need to add an extra revoke share. A small ring can cut the optional budget
of the next append through the `C - 1` cap, but cannot cut the space required by the current append; after truncation, there is no guarantee that the next append will be free from reclaim again.
`usable_log_capacity()` uniformly defines the ring capacity minus one reserved empty block; append admission, the check for a single oversized
transaction, and the cap on the checkpoint target all use this value, which ensures that the target is always reachable after a full drain.

This is the reclaim batch target after space runs out, not a journal-space reservation protocol at handle admission.
`begin` / `reserve_more` currently check transaction credits, and an append that does not hit `JournalBusy` does not establish
a reservation for the next one; a commit may still trigger a synchronous checkpoint after metadata has already been modified. Bringing a space guarantee into
transaction admission would need to cover handle extension, running/committing quotas, and the release lifecycle at the same time, independently of this policy.

Pressure selection reads only the selected prefix, the first entry of the suffix, and the tail credential, without first copying the entire queue. The selected transactions write back
the frozen metadata in order (the newer version of the same block is written last), perform a single home-block flush, and then update
and flush the journal tail once. Only after these two persistence barriers and the applicable recovery-state handling succeed are
the transactions and their journal space released one by one; if a write-back, flush, or tail update fails, all pending work, both selected and unselected, is retained and the
journal is aborted. When a suffix is retained, clean metadata cache entries with no pin and no new owner are first reclaimed in a targeted way by the block keys involved in the retired transactions. Checkpoint is only responsible for releasing the retired transactions' footprint and for the targeted reclaim, and does not use a batch count
to drive general cache cleanup. An explicit `sync_filesystem` retains full-cache reclaim after the drain and the final flush,
while the checkpoint worker itself handles only the retired transactions in a targeted way, and does not scan unrelated cache just because the entire FIFO was selected.
Cache invalidation required by persisting the recovery flag continues to follow the existing consistency protocol.

The demand of the ordinary cache is computed by `MetadataBlockCache` itself: when the resident block payload exceeds the 4 MiB
soft retention quota, reclaim needs to advance; no checkpoint batch or budget mirror is maintained.
The cache also has a `has_reclaim_demand: AtomicBool` hint, which lets mount
accesses with no reclaim demand skip the cache mutex. Every index insertion, removal, and invalidation updates the hint according to the actual length
while already inside the cache mutex; no counter is incremented on ordinary cache hits or mount accesses. The hint uses only Relaxed reads and writes,
does not publish buffer contents, and does not replace the lock protection of the index or of eviction eligibility; a hint observed before a concurrent change may defer
the current maintenance or trigger a redundant locked recheck, without affecting cache consistency.
`Ext4Mount` is the only mount-private object of the KVFS superblock, and internally still protects the core with `RwLock<Ext4SbInfo>`;
when a read or write access guard ends, before the mount protection is released, it provides the cache with a scan budget of at most 64 checked
entries. Read-only mount accesses provide the same entry point, with no need for journal pressure or syncfs to occur first.

The cache mutex uniformly protects the index, demand recheck, cursor, and eviction eligibility; the budget is charged per checked entry, and dirty, pin,
later ownership, and frozen image are all retained. The cursor fixes the ending block key for each round, so that continuously added high addresses do not
delay wraparound. Each index entry also carries a `was_referenced` bit protected by the same mutex: the first insertion does not count as reuse,
and later hits set the bit inside the existing index critical section, without adding a lock or an atomic read-modify-write. When the incremental scan meets an entry with the bit set, it clears the bit,
retains the entry for this pass, and continues advancing the cursor, still counting against the 64-entry check budget; only entries that are not hit afterwards are evicted under the original eligibility rules.
This is a one-bit second-chance policy; it does not maintain an exact LRU list, and it does not wrap around and rescan within a single call to make up an eviction count.
For request counts under a hot set mixed with cold accesses and under cyclic access exceeding the capacity, see
[Metadata Cache Reclaim Validation](metadata-cache-reclaim-validation.md).
Checkpoint-directed reclaim and explicit full reclaim are not delayed by the access bit, and still check pin/ownership/frozen image.
One access performs only a bounded scan and never a periodic full scan; remaining demand is advanced by subsequent mount accesses.
Ordinary read/insert does not evict by itself. An allocator mutation still holds the mount write guard, and the maintenance entry point runs after its allocator
lock and the prefetch/authoritative re-read window end; the prefetch set itself already pins its slots and does not rely on outer exclusion to withstand ordinary reclaim.
Maintenance under a read guard still runs through the existing entry point. Reclaim does no
device I/O, so no new cold-read insertion is placed in the allocator critical section. Explicit full reclaim and invalidation reset the scan cursor.

4 MiB is only a retention policy for resident block payload; it does not count frozen images or index overhead, and is neither a
hard memory limit nor a measured optimum; 64 only bounds the checking work of a single access and does not guarantee a number of successful evictions. When all entries
are held by pins/transactions, when a single access adds many entries, or when the insertion rate continuously exceeds the scan rate, the quota can still be exceeded;
entries that are reused continuously may also repeatedly receive a second chance, and the quota is not a goal of forcibly evicting hot entries;
reclaim is also not run proactively when there are no subsequent accesses. This implementation has no general shrinker, capacity admission, or background thread,
but it no longer uses the journal checkpoint cadence as a substitute for the ordinary cache's reclaim demand.
The batch size is determined by the space target and FIFO order, and does not cross uncommitted running/committing transactions;
revoke still suppresses writeback of old versions through the existing cache rules. After the committer finishes the prefix reclaim, it retries the append, and subsequent transactions remain in the FIFO.

This keeps a space budget for the next transaction between frequent barriers that are "just enough for this append" and long synchronous waits for "draining everything",
without adding a fixed-ratio watermark, background worker, or age trigger. A large transaction or a very small ring may still
need to drain; the mount write lock still covers device I/O, and no upper bound on the latency of a single commit is provided. Whether throughput and tail latency improve must be re-measured against
the shortest-prefix and full-drain baselines separately; the budget itself does not constitute proof of a performance gain.

Only when the batch drains the FIFO is the recovery state handled according to the policy of the last transaction in the batch; when a suffix is retained, `next_oldest` is used to
advance the journal tail to the first remaining transaction, keeping the journal active and the ext4 recovery evidence, without performing cache
invalidation or a mutable metadata reload. After a `PreserveDuringRecovery` transaction is appended, a checkpoint is performed synchronously and immediately under the journal
persistence execution lock, so if it appears in a batch it must be the last one, and no later ordinary transaction
joins before the drain. A batch may contain earlier `ClearAfterCheckpoint` transactions, in which case the whole batch still retains the recovery
evidence and reloads the latest mutable metadata state. All policy records are verified to exist before the frozen image is written back.
Tail verification borrows the persisted-evidence iterator already in the batch and does not construct a second credential container;
the journal superblock is written only after all continuity checks have completed. Frozen images are still written back transaction by transaction here, and
cross-transaction coalescing of metadata block writeback is not yet implemented.

regular inode 的同步目标保存在 VFS inode 组合持有的 `Ext4InodeState` 中，不在 journal 中
建立 inode-number 全局表。普通 inode-table staging 和 zero-link staging 在发布 metadata
时同时保存 handle 所属 `RuntimeTransaction` 的 `Arc`。已提交状态由该对象的
Checkpoint/Finished phase 判断，不用长期保存的 32 位 tid 与当前 sequence 作大小比较；
inode 长期驻留、checkpoint 退休和 sequence 回绕不会使旧目标重新指向新事务。每个 inode
只保留最新目标；checkpoint 退休仍释放该对象的 frozen metadata payload。

`fsync` 先完成所需 PageCache writeback，full fsync 再 stage dirty timestamps，随后在
mount 写锁内检查目标。未提交目标只提交其所属事务；已提交目标不再连带提交别的 inode
新建的 running transaction，但仍执行 device flush，因为此后可能有不带 metadata 的
mapped overwrite。`fdatasync` 暂时使用同一个 metadata 目标，不区分 timestamp-only 事务。
无目标的加载 inode、目录和其他非 regular inode 保留 mount-wide commit/flush。
未报告 transaction 的 metadata publication（包括 xattr）使旧目标失效；extent mutation
先使旧目标失效，只有后续 inode-table staging 才恢复目标，覆盖 external leaf-only rewrite。
Legacy orphan 链指针更新分为两类：当前操作的 resident inode 在 inode-table 更新成功后
登记 handle 的事务，不覆盖常驻时间戳，也不确认尚未 staging 的 dirty 时间戳。删除非头部
节点时，前驱只有 inode number；对其 `dtime` 链指针的拼接属于删除事务的内部恢复账目，
不改变前驱的文件属性或映射，因此不推进前驱的 fsync 目标，也不创建临时 inode 来模拟
常驻对象。拼接与被移除 inode 的链更新及适用的释放操作在同一事务中提交；事务未提交时，
恢复仍看到旧链和未由该事务释放的 inode。该例外不适用于前驱自身的属性或 extent 修改。
journal abort 在任何目标判断前检查；commit/flush 错误沿用原有 abort 和 VFS 错误观察路径。
mount 写锁仍覆盖提交和 flush，本步骤不拆分锁，也不省略数据屏障。
已挂载路径的 abort（包括异步回写的元数据完成）在 mount 写 guard 内执行；abort 返回前
发布原子投影，guard 的 Drop 在释放同一把写锁前观察它并标记 superblock 只读。同一 mount
不能同时有另一份写 guard 在投影发布中途执行 Drop。将来移出 mount 锁的 abort 出口必须
自行发布 superblock 只读状态，不能依赖另一次访问碰巧触发 Drop。
`syncfs` 和 KVFS unmount writeback 仍提交 running transaction 并 drain 全部 checkpoint。
当前 ordered-data dependency 由 running transaction 的 `ordered_generation` 和
`flushed_generation` 记录：发布 metadata 的 writeback batch 在成功的数据 write 后递增
登记代次，多个 PageCache batch 和 inode 可以共享同一事务。普通 batch 结束只更新内存
metadata，不单独 flush；freeze 根据两个代次是否相等生成 committing phase 的待 flush 位，persist 在
任何 recovery evidence 或 journal write 之前统一 flush，成功后才清除依赖并继续提交。
因此 writeback 完成不代表当前事务的数据已持久化；inode/filesystem sync 或容量压力提交提供
持久化边界。metadata-only transaction 不增加数据 flush。仅覆盖已映射且不超过 `i_disksize`
的数据在 data-only 路径不加入 metadata 事务；inode sync 提交已有事务时的同设备 journal flush，
或无事务时的直接设备 flush，以及 filesystem sync 的最终 flush，保证同步返回时的数据持久化。
该路径不承诺覆盖内容先于无关事务的 commit record 持久化。`flush_ordered_data` 在 barrier
开始前快照登记代次，成功后只确认该代次，失败不确认；较旧的 barrier 后完成时也不能降低已确认
代次。barrier 期间不持有 journal/transaction 短状态锁，并发新增依赖留给后续 barrier；
此确认协议不再依赖 mount 排斥其他 handle。冻结后的 flush 与 journal I/O 由持久化执行锁串行。
PageCache 仍以固定上限聚合 folio；dirty range 的必需数据块已由 `write_begin` 或
`page_mkwrite` 纳入 delayed-allocation accounting，writeback 不再扫描完整范围生成静态空间/
credit plan。实际写回由 byte cursor 推进，每处理一个 mapped、unwritten 或 hole allocation run
都重新调用 `map_blocks()`；unwritten conversion 和 hole allocation/insert/conversion 按实际
发生的 run 增长 credit usage，并优先通过 `JournalHandle::reserve_more()` 扩展当前 transaction。
The transaction credit limit is owned and enforced only by the journal engine inside `begin()`/`reserve_more()`;
the file layer neither reads nor reinterprets it. If the handle cannot be extended, core first flushes the current
data, advances only that prefix's `i_disksize`, clears the data dependencies this transaction already satisfied,
synchronously commits the transaction and releases exactly that prefix's delalloc reservation, then opens the
next transaction from the same cursor. Optional preallocation uses only the blocks left after reserved blocks and
every delalloc reservation are subtracted, and the cursor budget is decremented by the number of blocks the
writeback actually registered as in-memory prealloc spaces; the dirty window itself is allocated and marked in
the bitmap as usual, and registration does not change this allocation's size. Only the last transaction
publishes the complete size. Write and shared-mapping fault entry points update timestamps;
completion preserves the inode's current timestamps, including concurrent setattr changes.
Allocation/unwritten completion shares the ordered engine; background writeback scheduling belongs to a later stage.

The operation savepoint, operation token, and operation-local metadata byte copy have been removed. An ext4
mutation completes its checks of format, target state, space, credits, and extent path
expressibility before the first metadata access; the allocator completes its computation on private copies of the bitmap/descriptor/superblock bytes and the free-extent cache
and only then publishes. The JBD2 handle maintains credits and metadata/revoke membership, and records
ordered-data dependencies in the transaction; an explicit stop
returns unused credits and returns accounting errors; the journal itself maintains the abort state. Multiple handles can be stopped
independently. Only a metadata/revoke access marks the current handle as having published an update, even if the corresponding block was already added by
an earlier handle of the
same running transaction; once transaction membership has been published, it cannot be removed locally by a single handle
along its path, because other handles may already share the same metadata block; a later metadata access failure
aborts the journal. The persistence dependency of data I/O is independent of this mark: an ordinary error before a metadata access keeps the data dependency,
allows dirty data to be retried, and does not abort the journal just because the data write succeeded. Device/checksum/state-machine errors, and any ordinary error that occurs after metadata has been published, likewise
abort permanently; a commit or checkpoint I/O failure also records the abort before the error is returned, and subsequent sync and mutation
must not treat a leftover `committing`/checkpoint state as success. Bytes already published in memory, buffer
ownership, and the modifications of other operations that have already succeeded are not
rolled back across syscalls. Private copies not yet published, or ext4 resources just acquired but not yet published, are still cleaned up explicitly by the specific algorithm.
This is consistent with Linux JBD2: the handle is responsible for credits and buffer membership, failures are propagated
through journal abort, rather than building a second syscall transaction system; post-crash consistency is guaranteed by on-disk recovery evidence and replay.

If a clean v2 journal created by Linux has not yet declared the revoke feature, the first mutation first persistently enables that
feature, and only then allows transactions/checkpoints to overlap; a v1 journal has no feature bitmap and continues to fall back to a synchronous checkpoint after every
commit. This way, the release/reuse of metadata blocks still satisfies the revoke/reuse constraints below.
An ordinary mutation no longer unconditionally force-commits; after a successful handle is closed, its modifications stay in the running transaction, and
credits, journal space, and explicit filesystem sync determine the commit timing in the synchronous phase.
The orphan + `i_disksize` update of truncate is still force-committed, guaranteeing that a persistent
recovery point exists before the old block mapping is released; recovery-time orphan cleanup still completes commit/checkpoint synchronously. Time-based triggers and the background
worker are left until after the synchronously driven state machine is stable.

A journal that lacks the JBD2 revoke feature and cannot be upgraded still relies on a synchronous drain as its safety premise: when releasing extent/xattr
metadata blocks, the core forgets the retired block from the current handle's metadata set, rather than generating a
revoke record that the disk does not support. Because no older uncheckpointed transaction exists before a new mutation begins,
recovery has no old metadata image to suppress. An upgradable v2 journal instead
flushes the revoke feature before any transaction overlap, and subsequent release paths must write revoke records.

`ExtentPath` holds the buffers of each level from the inline root to the target leaf, the selected entry, and the logical lower and upper bounds, and is responsible for
leaf rewriting, index key propagation, balanced split, and empty-leaf prune. Path lookup moves each level's bytes directly into the path, avoiding
making another full copy of the metadata block for a parent sidecar. Regular extent insertion and unwritten conversion must be completed within
a single path, and no longer enter a whole-tree rewrite; range removal updates locally only when the entire range belongs to the same leaf, and cross-leaf
truncate/remove falls back to a whole-tree rewrite before any metadata write. A local split balances the old and new nodes, avoiding
a "full node + single-entry node" formed by `capacity + 1` entries, which would repeatedly allocate metadata blocks on subsequent insertions.

### Reclaim Transaction Batching and Physical Space Admission

The prepare, release batch, and finish stages of runtime zero-link eviction each still use a bounded handle,
but the end of a stage no longer forces a commit unconditionally. They can share the running transaction with namespace modifications;
explicit sync, credits, or journal space pressure still drives a commit. Each committed prefix retains the orphan and the remaining extents,
so the recovery flow can still finish the reclaim. After admission and handle creation succeed, each stage separately forbids the dying inode from further
extra-isize expansion, to avoid reallocating an EA block because of subsequent extent/inode updates after the EA has been cleaned up, and it does not rely on a
runtime flag left over from the previous stage. A prepare admission failure does not change that flag. Only the release of the last VFS reference enters
runtime eviction, and these failure paths must not be treated as ordinary inode operations on an inode that still has open fds. The boundaries that recovery forces
commit/checkpoint, and that ordinary truncate first commits the orphan/shrunk on-disk size, are unchanged.

`AllocatorState::pending_frees` holds the physical ranges pending reuse and their block counts, is queried by block group, and is released from protection
by transaction ID. Adjacent ranges in the same group and the same transaction are merged; a per-transaction reverse index lets the completion notification visit only the groups involved in that transaction.
Allocation queries only the allocator state, and no longer copies the global physical ranges through the journal/transaction lock.
JBD2 manages only the transaction lifecycle; after persistence succeeds and the transaction enters checkpoint, the filesystem journal glue
releases the journal lock and then notifies the allocator. Stopping a handle, freeze, cache reclaim/rebuild, and commit failure must not release
the protection, and there is no need to wait for the home checkpoint. The committed log and the revoke protocol guarantee the recovery order.

Contiguous physical ranges are released by the allocator per group, at most 256 blocks per segment: after verifying the bitmap, it updates the local bitmap,
free-extent cache, group counts, and superblock counts, registers one pending-reuse record per segment, and publishes metadata.
Metadata data blocks are still forgotten/revoked block by block; `JournalHandle` does not hold physical release ranges or release-algorithm caches.
A cross-segment failure must go through mutation-error abort, and a partially published release must not be treated as an ordinary admission failure.
When blocks pending reuse exist, surplus registration is suspended under the allocator lock, to avoid promising out blocks that are bitmap-free but not reusable.

Each runtime eviction batch reads only the rightmost leaf and its path, and trims at most the number of data blocks given by the caller.
When a leaf is retained it is rewritten in place, and empty leaves and empty parent nodes are reclaimed along the path; it does not collect the whole file's extents, rebuild the remaining subtree,
or allocate new tree blocks. The plan contains the leaf contents and the quota used for execution; the batch is first checked using the journal's single-handle capacity
check, and if it exceeds the limit it is halved and replanned, and `NoSpace` is returned only if the minimum batch still exceeds the limit. This check is shared
with begin/admission, and must not retry by catching a generic `NoSpace`, which would confuse physical space with commit failure. If the batch fits in an empty transaction but
the remaining quota of the current transaction is insufficient, the existing credits admission commits the transaction. The inode extent-status
cache is invalidated before execution, and the stage prefix continues to satisfy the orphan recovery contract.

`metadata_journal_for_allocation` performs admission under mount mutation exclusion with no active handle.
Physical demand is separate from journal credits: namespace uses the block count returned by the existing directory capacity check, plus the EA block that an
affected inode may newly create; xattr uses the already verified storage plan. When blocks pending reuse exist, writeback
estimates only the mapping operation within the next leaf range: mapped needs no data or tree blocks; unwritten reuses the boundary handling of the state conversion,
and computes the additional tree blocks from the post-conversion leaf entry count and the actual split layout; hole counts the data demand and the insertion layout of one contiguous allocation result.
Physical adjacency is unknown before allocation, so for a hole the upper bound is taken as an insertion result that does not merge into the old extent; when the leaf has capacity, the
tree block demand is zero. Split counting and execution share the path capacity, partition, and root-growth checks.

When blocks pending reuse exist, the optional preallocation of the current writeback is suspended, to avoid committing unrelated frees for optional growth. After each
mapping operation completes, the handle is stopped and admission is redone, continuing to share the same transaction; only credits or actual space pressure triggers a commit.
The writeback cursor retains only the transaction ID to which the current unconfirmed prefix belongs; completion state is still queried from the journal's existing ordered
checkpoint queue and checkpoint completion position, without separately keeping a mirror of the persistence state. After each admission and
handle completion (including error returns), the committed prefix is uniformly confirmed, the corresponding delalloc reservation is released, and the settled offset is
advanced; an explicit Restart also goes through the same path. A later abort does not undo a commit that has already succeeded.
An ordinary handle stop or a failed commit does not confirm the prefix; a later error reports only the completed byte count that has already been confirmed.
A complete unwritten conversion and a local conversion with sufficient capacity need no new physical blocks. The EA expansion demand is counted separately.
Credits admission is completed first, and then space is judged by `free_blocks - preallocated_blocks - pending_free_blocks`;
if insufficient, speculative spaces are reclaimed first and the check is redone, and only if still insufficient is an idle freeing transaction committed.
This check is not a general space reservation, nor does it guarantee a contiguous physical range; the allocator's original search and error contract remain in effect.
A legacy mutation with no upper bound on physical growth still commits conservatively; the whole-tree fallback of ordinary truncate is retained,
and runtime eviction no longer uses it. The entire operation must not be retried inside a handle that has already partially modified metadata.

Synchronization barriers, the precise inode sync tid, the recovery flag lifecycle, and the background worker are outside the scope of this change.
Tests cover reuse isolation after cache rebuild, commit failure, admission with sufficient space/the last block, preallocation bypass protection,
transaction sharing across the reclaim stages, and recovery after losing runtime state before and after a stage commit. The stage image tests are not equivalent to torn-write
or device volatile cache reordering tests.

### Namespace zero-link deletion

```text
namespace transaction
Delete dirent
  -> decrement nlink
  -> when nlink == 0, add to an orphan-file slot first; if full, fall back to the legacy orphan list, and persist zero-link metadata
  -> return the updated parent/target inode to the kext4 KVFS layer

The last VFS inode/open-file reference is gone
  -> KVFS SuperBlockOperations::evict_inode
  -> Drop the PageCache and delalloc reservation
  -> If an external xattr block exists, release it first
  -> truncate extent-backed data blocks
  -> Remove orphan entry
  -> Free the inode bitmap entry
```

The namespace transaction does not release the inode number, xattr, or data blocks. Existing VFS/open-file references continue to
hold the same `VfsInode` and its composed ext4 private state, so it can still be read and written after zero-link; a new
namespace `iget()` rejects a zero-link inode, and does not re-instantiate an orphan as a reachable file. The corresponding
namespace credits cover only the dirent, nlink, and orphan metadata; the extent, external xattr, and inode bitmap work of the later final eviction must not be counted ahead of time into the rename/unlink/rmdir reservation. The old
single-transaction recovery/test eviction path estimates according to the actual metadata targets of the current extent tree, while the runtime
kext4 KVFS layer continues to use the bounded three-stage eviction.

### Writeback speculative prealloc space

```text
surplus sampled by writeback        -> Registered (inode logical map + group twin table, published inside one allocator critical section)
later write hits a registered span  -> Consumed (whole/prefix/suffix/interior consumption, the remainder split into new Registered entries)
shrink truncate                     -> Clipped (the part past EOF reclaimed, the prefix stays Registered)
last writable close                 -> Dropped (only when no delalloc reservation remains)
writeback settlement (reserved==0)  -> Dropped (only when no writable open remains; a racing settlement may defer to the next one or eviction)
eviction                            -> Dropped (unconditional, including cache eviction with nonzero nlink)
```

A space is a promise that "a given logical range of a given file will be fulfilled by this physical run", and it exists only in memory: the bitmap, the group
descriptor and the inode do not record it, and a block does not enter `i_disksize` or get written to disk before it is consumed, so a crash or unmount
leaves zero residue on disk. It is registered in two views at once, and the two sides change together only inside the same allocator critical section: an entry
that is valid on only one side protects no block and carries no promise that can be honored. The inode side is indexed by logical start block for consume lookups, and the group side
is indexed by `(group, start physical block)` for group-scan exclusion and ENOSPC discard, and each twin entry holds a strong reference to its owner.

Conservation invariant: `preallocated_blocks` is always equal to the sum of the lengths of the twin segments on the group side; after every change it is
checked by `debug_assert_prealloc_conservation!`. The strong owner reference on the group side makes it impossible for a space to be released before the
inode object, so eviction must discard unconditionally; otherwise the twin table would be left with dangling entries.

The free accounting is not affected by a space: `free_blocks_count()` returns the on-disk aggregate value, and an unconsumed surplus is still counted as free
(the same accounting as Linux statfs), so "the aggregate is nonzero but no block can be selected" is a normal state, handled by the ENOSPC retry valve, rather than
by subtracting the surplus from free.

Difference from Linux: `MB_GROUP_PA` (the locality-group fallback used when an inode PA misses) has no counterpart here.
kext4 has no locality-group PA producer; the length normalization on the write path does not enlarge requests smaller than `EXT4_PREALLOC_MIN_WRITE_BLOCKS`
(4 blocks), so small writes produce no surplus and therefore register no space.

The writeback Hole branch wires birth and consume into the production path: a registered space is consumed
first (a hit takes the physical span under the 1:1 rule, marks the bitmap and re-splits the remaining head and
tail inside the allocator critical section); only on a miss does the branch normalize the length and allocate,
and a successful allocation registers the surplus inside the same lock section. **Consume is therefore the only
point where a space reaches disk or the bitmap** (Linux `ext4_mb_mark_diskspace_used` marks only the consumed
span), while birth writes only the in-memory indexes; the block bitmap, group descriptor, free-extent cache and
the free aggregate stay unaware of a space until it is consumed.

`MAX_PREALLOC_SPACES_PER_INODE` (16) is a birth-admission threshold, not a live-entry bound: a new entry is
rejected once the inode's map already holds 16, which stops a long-lived writer that jumps between sparse
regions from registering unconsumed free runs without limit. Consuming an interior span replaces one entry
with a head and a tail, so the map can exceed the threshold and those entries live until a later consume or
discard; the threshold also says nothing about other inodes sharing the group. Adjacent births fuse, so an
appending writer normally keeps one. Reaching the threshold is an ordinary "no preallocation this time", not
an error.

## Algorithm Flow

A namei modification first validates parent/name, looks up the target dirent, checks the inode kind and the on-disk format constraints, and then completes the dirent and inode updates in
a single journal transaction. Rename proceeds in the order prepare, replace, delete, finalize,
which keeps the directory parent link count and the `..` update consistent.

The credits for a directory insertion are reserved against the upper bound of the operation; slot lookup, append, HTree conversion and splitting are determined by the execution path.
`DirectoryInsertPlan`, the path enumeration and its derived fields have been removed; the credit computation does not read directory contents, and does not depend on
which path the space pre-check selected. `Ext4Credits` combines the individual budgets of the inode, dirent, HTree and extent.
The extent layer computes, from the current tree depth already validated for the inode, the upper bound of new blocks and old-path rewrites for one append. One insertion
reuses the old external block and adds at most one sibling per level; inline leaf spill-out and root height growth are also counted in the upper bound. Only a single-block directory that has indexing enabled and is not yet indexed
may be converted, and only then are two appends counted; the second append is budgeted for at most one additional level of height growth.
Whether a directory uses HTree is determined jointly by the filesystem features and the inode index flag, and lookup, scanning, modification and credit
computation share that determination. The `BLOCK_ALLOCATION_METADATA_CREDITS` exported by the allocator layer
uniformly represents the allocation metadata plus the create access for the new block; parent-directory growth
and the child block of mkdir/long symlink all reuse this definition.

The space check still validates directory and extent capacity before metadata is published, and returns only the number of blocks required. It preserves the behavior that an in-place insertion can still complete
when there are no free blocks, and prevents namespace paths that currently have no local rollback from leaving
a half-completed modification behind after an allocation failure; it no longer takes part in the journal budget protocol. A split immediately after conversion still checks extent space as two independent single-block mappings
and cannot assume that the physical allocation is contiguous.
A traditional block-pointer directory can still use existing slots; growth of its mapping is rejected by the capacity check before admission,
so block pointers are never misdecoded as an extent root for the sake of the budget.

The budget is a conservative upper bound; an in-place insertion also requests the same operation credits, and handle stop returns the unconsumed portion. A single request that exceeds the
journal transaction capacity returns `NoSpace` without aborting; when the total credits of multiple handles exactly equal the limit, joining is allowed, and
when they exceed the limit, admission to the old transaction is first closed and the commit entry then waits for existing handles to exit. Filesystem mutations execute serially under the mount write lock, and admission uses the existing active-handle count to reject nesting and then persists the old transaction, so
the caller never waits for its own handle while holding the mount lock. The wait reuses the transaction state queue; stop, commit, checkpoint and abort
all notify that queue, and no separate reservation state is added.

If credits are insufficient before metadata is published, `InsufficientJournalCredits` is returned as is, mapped to `ENOSPC` for the VFS,
and the journal is not aborted. If credits are exhausted after metadata has been published, a pre-modification validation was missed; per the transaction invariant the failure is escalated to
`InvalidJournalTransaction` and the journal is aborted, and the current call returns `EIO` to the VFS; subsequent mutations return `JournalAborted` at the journal
entry, mapped to `EROFS`, and do not first modify the journal features or superblock metadata.
The journal abort state is the basis for disabling writes. `Ext4MountWriteGuard` borrows the caller's existing superblock and,
before releasing the mount write lock, publishes the abort to its existing `RDONLY` flag, preventing subsequent buffered writes from entering the page cache;
remount to writable likewise validates the journal abort state, and on failure neither applies the mount options nor clears the read-only flag. Cached-write
validation and delalloc reservation also check the same abort state, so that the update window of the VFS flag does not allow pages to
continue being dirtied; no extra superblock reference or read-only field is stored.

`JournalTransactions::ensure_writable()` and `is_aborted()` read the journal's
`has_abort_error` projection, avoiding repeated shared transaction-mutex access
from healthy buffered writes. The first error remains in `inner.aborted`. Abort
stores that error under the mutex, publishes the flag with Release, then unlocks
and wakes waiters. Acquire observers do not access transaction data through the
flag. Admission, credits, and phase changes keep their locked abort checks. Only
a successful empty-state replay reset clears the error and projection under the
same mutex; failed reset leaves both unchanged. A concurrent admission check may
precede abort publication and does not grant writability for the entire write.

The budget regressions in `ut-tests/namei/` cover in-place insertion, link/rename with fragmented inline/external extents,
HTree conversion, a split immediately after conversion, and a split of an existing HTree leaf with external extents. The special paths complete under their respective credit
limits and are checked for persistence after reopening; with one admission credit fewer, `NoSpace` is returned, nothing is written to disk, the link
count is unchanged, and there is no abort. `ut-tests/jbd2/transaction/` verifies exact capacity, real waiting, and stop/abort wakeups;
`ut-tests/journal/` and `ut-tests/vfs/` verify nested admission, the abort boundary on credit exhaustion,
remount validation, and the errno of cached writes. A fixed 64-credit transaction covers consecutive hard links crossing an HTree conversion,
asserts that they still share the same running transaction, and checks persistence after reopening.

The on-disk directory semantics are carried by the existing object layering: `RawDirectoryEntry` handles `rec_len` encoding and decoding in one place and, consistent with Linux
`ext4_rec_len_from_disk()`, interprets a raw `0` and `0xffff` as the entire directory block; a full-block write-back of 64 KiB uses
`0xffff`. `HTreeRootInfo` validates the root fixed fields and the on-disk hash version in one place, and `Ext4SbInfo::decode_htree_root()`
then combines `large_dir` to validate the tree depth and decode count/limit, so the read and write paths no longer each maintain their own set of root rules. The on-disk root
accepts only Linux's `0/1/2/6`. The on-disk `Ext4DiskSuperblock` stores only the facts `s_def_hash_version`, `s_hash_seed` and
`s_flags`; `Ext4SbInfo` validates the default version at mount time following `ext4_hash_info_init()`, and interprets the signedness flags only when
`DIR_INDEX` is enabled. An explicit unsigned takes precedence and an explicit signed stays signed; when neither
is set, the runtime `hash_unsigned=3` is generated according to the current Linux global unsigned-char semantics. This initialization phase only
establishes the mount state; a writable VFS mount writes `EXT2_FLAGS_UNSIGNED_HASH` back at the finalize boundary after the root inode is established and before the superblock is published, and a read-only mount does not modify the disk. This corresponds to the Linux lifecycle in which
`ext4_hash_info_init()` first parses the policy and `ext4_setup_super()` then commits the superblock.
The HTree entry point combines the root on-disk version with that
runtime offset before running the algorithm, so callers cannot reassemble the policy from bare parameters. SIPHASH is therefore a valid root format,
but because KExt4 has no fscrypt name key yet it returns `Unsupported` before the first hash computation and is not misreported as directory corruption;
a namespace transaction that depends on this hash therefore cannot select or write the corresponding HTree leaf, and ends in failure without committing
the metadata change. Orlov top-level directory placement goes through a separate entry point: Linux `find_group_orlov()` reconstructs
`dx_hash_info` for this entry point, fixing signed `DX_HASH_HALF_MD4` and `s_hash_seed`, and does not read
`s_def_hash_version` or `s_hash_unsigned`. KExt4 keeps the same boundary, so images with a LEGACY, TEA or HALF_MD4
default version all get the same Orlov starting flex; this may change the directory placement of older KExt4, but it restores
Linux allocator semantics and does not change the on-disk HTree format.

The kext4 KVFS-layer callbacks for create, mkdir, mknod and symlink receive the `&Cred` of the same operation and first use
`inode_init_owner()` to derive mode/UID/GID from the parent inode, `fsuid/fsgid` and the setgid inheritance rules,
and then pass the explicit `uid` and `gid` parameters into the KExt4 namei transaction. The core inode constructor does not read the current task,
and provides no runtime default of a fixed root owner; test image construction must explicitly pass its fixture owner.

An xattr modification decodes the inline xattr and external xattr once, and within the same mutation plan completes
the existence check, value update, storage layout selection and journal credits computation. A plain set/remove always uses the inode's
current `i_extra_isize` and does not take the opportunity of an xattr syscall to enlarge the extra fields: set first tries to place the entry that changed
into the inode body, and if it does not fit, places only that entry into the single external block; unrelated entries stay in their original region,
and remove likewise deletes only the target entry. On commit it maintains `i_file_acl`, `i_blocks`, the Linux-compatible external
entry hash, the block hash, the block checksum and the refcount together. The inline entry's
`e_hash` is kept at zero per the ext4 format, while for an external entry a rotate-xor
hash is computed over the name and the value padded to four bytes, and the block header then aggregates `h_hash` in entry order.
`Ext4XattrSetMode` expresses the four combinations no flag, create, replace and create+replace; a combined flag returns `EEXIST` when the attribute
exists and `ENODATA` when it is missing, and this is not implemented through a pre-lookup outside the lock. When replacement is allowed,
if the existing value is byte-for-byte identical, the core returns before layout planning, the journal handle, the metadata write and the ctime update.
The effective want follows Linux's superblock initialization policy: when the inode is larger than 128 bytes, 32 bytes are first reserved for the extended
inode fields known to Linux; only when the filesystem declares `RO_COMPAT_EXTRA_ISIZE` is it then taken as the maximum with the on-disk min/want.
A new inode has this effective want written directly into `i_extra_isize` in the allocation transaction, so the first
ordinary write-back does not need to read and plan xattrs just to fill in a new inode. When an existing external attribute set is byte-for-byte unchanged, the original block is
reused directly, and even if it is a shared block and the allocator is full, no COW is triggered; only when the external content changes
is it required that the existing private block be rewritable in place or that the allocator still have a free block; therefore an update on a full disk whose target entry
can still be completed within the current layout is not misreported as `ENOSPC`.
`Ext4Inode` exposes the immutable and append-only state from the on-disk `i_flags`; the kext4 KVFS layer maps them at iget time
to KVFS `NodeFlags`, so that the generic xattr permission layer returns `EPERM` before entering the namespace or KExt4 mutation.

Except for xattr's own updates and zero-link eviction, every ordinary inode dirty entry point first tries to grow the existing inode's
`i_extra_isize` to the superblock's runtime effective `want_extra_isize`; this covers regular-file
writes/truncation, extent updates, chmod/chown/utimes, and directory namei metadata, not just file writes.
Xattr apply goes through the low-level inode-table helper, so that the expansion process does not recurse into xattr mutation. When the target layout cannot hold
all xattrs, it falls back to the on-disk
`min_extra_isize`, and if that still does not hold, it keeps the current size and continues with the ordinary metadata update. When the inline area needs to shrink,
the planner rescans repeatedly following Linux's entry-by-entry strategy: it first migrates the smallest entry that is sufficient to free the required space in one step,
otherwise it migrates the entry that the last external block seen in this scan pass can accommodate and then continues; `system.data` is not allowed to be moved out.
The migration, the inode-body re-encoding, and the `i_extra_isize` update belong to the same journal transaction; the handle extends credits only for the newly added
external metadata targets, and the transaction engine uses the journal limit fixed at mount time
to reject out-of-bounds extension.
When a new external block is needed, block allocation completes before entering layout apply; if a free-space race after planning returns
`NoSpace`, `no_expand` is recorded and the ordinary inode metadata update continues. Device I/O, checksum, or metadata corruption
is still propagated as a journal integrity error, and must not be silently ignored as an opportunistic expansion failure.
Once it is confirmed that no feasible expansion layout exists, `Ext4Inode` records in its resident private state a flag equivalent to Linux's
`EXT4_STATE_NO_EXPAND`, so that later writes no longer repeatedly read, checksum, and plan the same xattrs; a successful deletion of an
xattr clears this flag, because the inode-body/external space has changed. `JournalBusy` or temporarily insufficient journal credits
occurs before any actual layout migration, is not recorded as a permanent failure, and later transactions can still retry.

`list_xattrs()` uses `Ext4XattrNameSink` to borrow each validated on-disk name one entry at a time, validating only the value range without
copying the value. The kext4 KVFS layer adds the `user.*`, `trusted.*`, and `security.*` prefixes in the sink and continues streaming
the result onward, without constructing `Ext4Xattr` or an intermediate vector of full names. After a successful mutation, the kext4 KVFS layer synchronizes the core inode ctime
back to the shared VFS identity. Zero-link eviction reuses the external xattr block cleanup logic, first releasing the
EA block and then releasing the inode bitmap entry.

Truncate 使用 orphan file（满时回退 legacy orphan list）保护 regular-file shrink。KExt4 的
`InodeOperations::set_len()` 按
`prepare_regular_inode_truncate()` → `VfsInode::truncate_setsize()`（先发布 VFS i_size，
再执行 unmap/cache truncate/unmap）→
delalloc extent-status 尾部删除 → `finish_regular_inode_shrink()` 排序。是否删除 delalloc
以旧 `i_size` 为准，是否释放磁盘 mapping 以旧 `i_disksize` 为准；因此截断到
`[i_disksize, old i_size)` 或恰好等于 `i_disksize` 仍会在 dirty folio 被丢弃后同步减少
`i_reserved_data_blocks` 与 mount aggregate。这与 Linux ext4 `setattr` 路径在
`i_size_write()` 后执行其内部 `truncate_pagecache()`、再进行 filesystem block truncate
的职责层次一致。`prepare_regular_inode_truncate()` 即使先提交了缩小后的 `i_disksize`，也必须
保留旧 `i_size`，使随后 PageCache 能按真实旧 EOF 丢弃 folio、清零同页尾部并失效映射；不增加
第二个 truncate operation hook。显式 recovery 在 journal 需要 replay 时先重放并保持 recovery flag，
再初始化 mount-lifetime orphan info 并遍历 legacy list 和 orphan file；即使 journal 已 clean，
只要 superblock 仍有 orphan head、orphan-file entry 或 `ORPHAN_PRESENT`，也会执行同一 cleanup。
`nlink > 0` regular inode 完成中断的 truncate，`nlink == 0` inode 复用 final
eviction 事务释放 external xattr、extent 和 inode bitmap。`recover()` 返回 `None` 只表示
没有 journal replay report，不表示没有执行 orphan cleanup，也不降低成功返回的持久性保证。
Orphan file backing inode 通过 Linux `EXT4_IGET_SPECIAL` 对等的内部入口加载，不进入 public
iget、inode allocator 或 release。`Ext4SbInfo::orphan_info` 只保存 Linux `ext4_orphan_info`
同层的 block identity、checksum seed 与增量 free-entry 计数；resident `Ext4Inode` 保存
`i_orphan`/`i_orphan_idx` 对等的 `OrphanLocation`，不把运行时索引塞进按需解码的磁盘 inode 值。
`OrphanInfo::add/remove` 直接拥有槽位选择、journaled block 更新和 free-entry 计数维护；
`Ext4SbInfo` 负责 orphan 格式分派、legacy fallback 和 recovery 调度。格式参数借用现有磁盘
superblock，I/O 借用现有 metadata object，不增加 operation context 或重复 geometry 字段。
挂载级独占访问串行化槽位修改，因此不需要 Linux 并发分配器的 CPU hash、CAS 和原子计数。
信息只在 journal replay 后初始化一次；recovery 每块只取一次 entry snapshot，逐 inode
checkpoint 只重载 superblock/group 可变状态并增量维护 free-entry 计数，不重复扫描整个 orphan file。

Fallocate preallocation (`Ext4Mount::fallocate_regular_inode_preallocate`, mode 0
and KEEP_SIZE) walks `map_blocks()` and skips each already-mapped range (including
unwritten, so repeated calls are idempotent). Each hole range opens an independent
journal transaction capped at `EXT_UNWRITTEN_MAX_LEN` per batch: one
`Ext4AllocationRequest` (goal taken after the previous extent, ALLOW_PARTIAL) plus
one unwritten `insert_extent_mapping()`, reusing the ordered-writeback worst-case
credit model of "1 allocation run + 2 extent mutations"; `map_blocks()` already
truncates the hole length to a leaf boundary, so the insertion naturally satisfies
the single-leaf constraint. ENOSPC inside a batch propagates immediately, keeping
the committed batches. On failure kext4 settles the committed prefix before
returning the original error: it advances `i_disksize` to the successfully
allocated end (capped at the request end, never shrinking) and refreshes
ctime/mtime, mirroring Linux `ext4_alloc_file_blocks`'s `min(epos, new_size)`
update and its `file_modified()` entry; for KEEP_SIZE only timestamps are
refreshed and size stays untouched. The allocated batches stay allocated as
unwritten preallocation beyond EOF, observable via df/fiemap/st_blocks, surviving
across close and resumable via idempotent retry. kext4 does not retry the whole
loop as Linux does: its allocator
is goal-driven scanning, so a retry gains nothing. For !KEEP_SIZE it first zeroes
the old EOF tail via `zero_regular_inode_tail_data()`, then once every allocation
is durable commits `i_disksize = end` in a separate small transaction (Linux
`ext4_alloc_file_blocks`'s "extents before size" ordering — a crash loses only the
size transaction, keeping the preallocated blocks), and the VFS layer later
publishes the visible size (`truncate_setsize`; `VfsFile::fallocate` holds the data
and invalidate write locks across the entire dispatch, excluding concurrent
writeback both before the `i_disksize` advance and after the publish, so a writeback
batch carrying a stale visible-size snapshot cannot reach the UnallocatedWrite
guard). KEEP_SIZE leaves size/`i_disksize` untouched and only refreshes ctime/mtime
with a `Full` metadata transaction (Linux `file_modified()`). Delalloc reservations
and unwritten preallocation coexist: a later writeback hits the Unwritten branch
and converts the preallocated block in place without freeing it first. The
supported-mode set is expressed via the kvfs
`FileOperations::supported_fallocate_modes()` capability declaration: kext4
declares `KEEP_SIZE` (UNSHARE_RANGE needs reflink and, like Linux ext4 without
shared blocks, is rejected by the mask outright rather than degrading to
preallocation), `VfsFile::fallocate` rejects mode 0, UNSHARE_RANGE, COLLAPSE_RANGE
and INSERT_RANGE bits outside that declared mask (plain allocate is encoded as no
bits set, as in Linux, and always passes the mask), and the kext4 entry
additionally validates `offset + len` overflow and `max_file_size()`. A backend
that declares a mode but still answers `EOPNOTSUPP` is rejecting the file, so the
error propagates instead of a silent emulation; only a backend that declares no
fallocate capability (empty mask) is emulated by the posix layer. PUNCH_HOLE and
ZERO_RANGE are owned by a backend that declares them; kext4 declares neither, so
the posix entry emulates them by zeroing the range, which neither frees blocks nor
reclaims an unwritten reservation (punch hole converts an unwritten extent to
initialized rather than deallocating). This approximation stays until kext4
implements real punch-hole/zero-range, at which point it declares the modes and
owns the release/zeroing.
Legacy orphan 链的 `i_dtime` 是 journaled inode-table topology，不复制到 resident private
state。链遍历直接读取 metadata cache 中的 inode-table bytes，前驱重连也按已经校验的 inode
number 原位修改同一 bytes；它不会为了读取或更新前驱而构造另一个 `Ext4Inode`。因此两个同时
open-unlink 的 inode 可以按任意顺序完成 final eviction，而不会从旧 private snapshot 恢复已经
移除的 orphan next。
clean-journal cleanup 的首个 transaction 会先建立 ext4 recovery evidence；所有 recovery
cleanup transaction 都采用 `PreserveDuringRecovery`，逐个同步完成 commit/checkpoint，并从
已落盘的 superblock/group descriptors 重新建立内存状态，避免旧 orphan head 或 allocator
counter 被 checkpoint 前的快照重新带回循环。重建时冻结的 `group_geometry` 地址表保持
挂载时的值：replay 后的 descriptor 地址若与冻结表不一致，reload 直接报 corruption
返回，不会带着分裂的地址状态继续清理。全部 orphan 清理完成后，recovery 再确认 JBD2
`s_start` 为零，最后清除并 flush ext4 recovery feature；任一步失败都会返回错误，而不会先
清除最终的磁盘恢复证据。Ext4DiskSuperblock decode 会保留越过 inode table 的原始
`s_last_orphan`，使显式 recovery 有机会处理该损坏，而普通 mount 仍因非零 orphan head 返回
`NeedsRecovery`。Recovery 在 `iget` 前同时校验 inode table 上界、reserved inode 范围和经过
checksum 验证的 inode bitmap 分配位；编号合法但 allocation bit 为零的 head 不读取 inode
table。成功加载 inode 后还会在执行 truncate/eviction 前验证：带链接的 inode kind 必须属于
Linux ext4 可截断的 regular/directory/非 fast symlink，以及原始 `i_dtime` 不得超过 inode
上界；legacy next 还必须为零或合法的下一
orphan 编号。命中这些 bad-orphan 条件时按 Linux `ext4_orphan_cleanup()` 的终止规则，用独立的
一-credit transaction 把 head 清零。该 transaction 同样采用 `PreserveDuringRecovery` 并计为
cleanup work，避免 replay 分支把已经推进的 journal sequence 回写成旧状态；只有 clear 的
commit/checkpoint 完成后才允许清理最终 recovery feature。Bitmap 读取/checksum、inode decode、
合法但 KExt4 尚不支持的格式或 cleanup 错误仍向上返回，不借此路径静默吞掉一般 metadata
corruption 或把未完成的有效 cleanup 伪装成成功。

Truncate and preallocation-discard journal credits are computed from the actual
extent structure: the inode root, the rebuilt extent-tree blocks, the old tree
blocks that need revoking, and one bitmap/descriptor target per distinct block
group the released range touches. "Unwritten preallocation" here means the
unwritten extents beyond `i_disksize` in the on-disk extent tree
(`discard_regular_inode_preallocations()`, corresponding to Linux
`ext3_truncate_failed_write()`-style cleanup), unrelated to the in-memory space
described in the previous section on writeback speculative prealloc space: the
latter never hits disk and needs no credits. The number of data blocks does not
add journal metadata blocks one-to-one, so `i_blocks` or the count of released
data blocks must not be used to scale the reservation directly; otherwise
reclaiming one tiny preallocation tail of a large file would be misjudged as
exceeding an empty journal's capacity. The only exception is the data blocks of
directories and block-mapped symlinks: they are journaled metadata buffers
themselves, so the release path emits one revoke per released block, and the
credits append one revoke credit per actually released data block (matching
Linux `get_default_free_blocks_flags()`'s METADATA|FORGET semantics for
S_ISDIR/S_ISLNK); regular-file data blocks carry no such overhead. The result
still reserves fixed headroom for the allocator entry check and is computed
before any metadata mutation.

Ordered writeback insert and unwritten conversion use only `ExtentPath`, so inside a transaction there is no longer any
switch to a rebuild algorithm whose complexity depends on the size of the whole tree. The writeback execution cursor reads the live mapping at every step;
the credits count one extent mutation for each actual unwritten conversion, and one allocation, insert and conversion for each
hole run actually returned by the allocator. A mapped overwrite keeps only the final inode metadata
margin and no longer scales linearly with data blocks. The computation still covers the maximum extent depth and the existing/new
metadata targets that a split at each level may touch, and does not truncate with a fixed value. Before each mutation begins, the handle must retain
the increment for that operation plus the fixed margin for the final inode publish; when capacity is insufficient, the transaction may be restarted only after the previous run has fully completed,
and a path-local mutation must never be split midway. The whole-tree fallback for cross-leaf range removal still uses the
truncate planner, which estimates it separately from the actual tree blocks, revoke targets and affected groups.

The return value of segmented writeback follows a "may be partially complete" contract: the failure of a later transaction does not undo the
durable prefix that has already been committed, and does not count its delalloc reservation again. The core returns the completed byte prefix through KVFS
to MappingPages, and the KVFS callback uses `Result<usize, (usize, VfsError)>` to directly express
the submitted length, or the completed prefix together with the error, without wrapping it in WritebackRangeOutcome; folios that lie entirely within that prefix end writeback, while folios that straddle the prefix boundary and the remaining suffix stay
dirty. On resubmission only the dirty suffix is retried, and the current state is determined afresh from the live mapping. Ordinary retryable errors
may be retried on the current mount; a device or JBD2 error permanently aborts the journal of the current mount, and the recovery path is
to resubmit after recovery/remount, so it cannot be claimed that convergence is guaranteed on the same mount.

The `huge_file` superblock feature indicates that an inode may use the extended block accounting format; ordinary inodes without
`EXT4_HUGE_FILE_FL` still record `i_blocks` in 512-byte sectors, and KExt4 can safely modify them.
Inodes that actually have that inode flag set and count in filesystem-block units still explicitly return unsupported.

Namei, setattr, writeback and truncate mutations update in place the ext4 private
state held by composition in the VFS inode; the full `stat/getattr` reads nlink, size, blocks, mode/owner, rdev and timestamps in a single inode-state
critical section, through a transient `Ext4InodeStat` equivalent to the Linux `struct kstat`. This value is not resident, does not take part in identity,
and is not an attribute cache. unlink/rmdir/rename obtain the victim/moved/replaced VFS
inode from the locked dentry and pass its private state explicitly to the core, and the core is forbidden from reloading by inode number during a mutation.
The VFS-wide identity table therefore guarantees, keyed by `(SuperBlock, inode number)`, that one live identity corresponds to exactly one `VfsInode`, one
`AddressSpace` and one copy of the ext4 private state.

`Ext4SbInfo::timestamp_limits()` derives the shared
`ktime_types::TimestampLimits` from the filesystem inode size: a 128-byte inode that cannot hold `i_atime_extra` supports only whole seconds and
the signed 32-bit second range; an inode with the full extra timestamp space supports nanoseconds and the extended epoch. The ext4
superblock operations return this capability during fill from the already installed `Ext4SbInfo`, and it is held by the unique KVFS
`SuperBlock`; the bridge no longer stores or converts a parallel range structure. When the VFS generates the current time it first truncates it to this capability, and only then uses it for comparison, filesystem callbacks and
resident publication; the raw inode setter only stores values that have already been prepared. The initialization structure of a newly created inode stores
the full `Ext4Timestamp` and writes atime, ctime and
mtime through the same base/extra encoder as existing inodes, so a 256-byte inode retains the nanosecond and epoch bits, while a 128-byte inode is truncated at the on-disk boundary to the representable
whole-second range. The existing timestamp encode/decode still truncates or validates according to whether each actual extra field is present,
serving as a second layer of protection at the on-disk format boundary.

挂载时由 ext4 fill operation 把 filesystem block size 与 extent-format `s_maxbytes` 写入 KVFS
`SuperBlock`，legacy bitmap maxbytes 只保存在 `Ext4SbInfo`；Ext4Inode 通过其组合持有的 ext4
private state 查询 extent-format 状态。write、truncate、FIEMAP
和 `page_mkwrite` 通过同一个 helper 按该格式状态选择上限，既不复制派生的 per-inode
maxbytes，也不为上限查询重新读取 inode-table 或取得挂载级 core lock。
Block mapping 热路径从同一 inode state 临界区一次取得 ext4 flags 与 60-byte `i_block` root；
extent 和 legacy mapper 随后借用该局部 root 完成一次 run 查询，避免 direct-pointer 合并循环
重复加锁或复制 `i_block`，也不在块设备 I/O 期间持有 inode state lock。
每份 inode private state 用不重叠的 logical-block 区间保存 delayed extent，
对应 Linux `ext4_inode_info::i_es_tree` 中的 delayed entries，并维护
`i_reserved_data_blocks` 等价计数。挂载级 reservation aggregate 对应 Linux
`s_dirtyclusters_counter`，用于 admission 与 `statfs()`，不是第二份 extent identity。
Delayed-allocation admission 使用 primary superblock 的 free-block counter 减去 ext4 reserved
blocks 和 core mount aggregate。reserve/release/truncate/writeback/eviction 只能调用 core 的
区间 API，由 core 在各自临界区内原子更新 inode 区间、per-inode count 和 mount
aggregate（inode 侧在 per-inode 状态锁内，mount 总量在独立 delalloc 锁内）；
writeback 结算时，同一 inode 状态锁内将配额预留转为已用、增加已分配块数并移除已结算
的延迟预留，`getattr` 读取的已分配量和剩余预留不会重复计算同一块。bridge 不读取或调整
任一计数。该 counter 与 group descriptor 由同一
allocation/release mutation 更新，因此 admission 与显式 `statfs()` 都直接读该常
数时间 aggregate，不遍历 group descriptor（避免持锁 O(组数) 折叠阻塞全局分配）。
目录分配的 Orlov goal 选择同样只读常数时间 aggregate：allocator 锁内一次取得
free-inodes/free-blocks 与 mount 级 directory-inode 总量（对应 Linux
`s_dirs_counter`，挂载时一次 O(组数) fold 播种，此后与逐组 `bg_used_dirs_count`
同点更新），不在持锁期间逐组折叠。

Core exposes two Linux ext4 total-capacity conventions through `Ext4StatFsMode`, but does not parse mount-option strings:
`Bsd` subtracts the first-data-block and the metadata system zones from the superblock's total block count, while `Minix` uses the
on-disk `blocks_count` directly. Both modes share the same group free-inode/free-block aggregation and likewise subtract the
reserved blocks and the delayed-allocation mount aggregate, so only `Ext4StatFs::blocks`
may differ; `blocks_free`, `blocks_available` and the inode statistics must be identical. The mode is selected and its lifetime managed by the VFS
context, which parses it from the opaque mount data and stores it in the single `Ext4SbInfo`, corresponding to Linux
`ext4_sb_info::s_mount_opt`; it is not placed in the static operation table and no wrapper is added. A new mount that does not specify it defaults to
`bsddf`; a reconfigure that does not specify this option keeps the current value, and only an explicit `bsddf`/`minixdf` updates the mount-private state.
The positive options that Linux enables by default, `user_xattr` and `acl`, are accepted as mount-data compatibility spellings, but no unused per-mount field is created for them; the syntax compatibility of `acl` does not widen the current KVFS POSIX ACL permission semantics.

Buffered write applies the inode-format limit in `FileOperations::write_iter()`, inside the inode data critical
section of generic write, and then `write_begin()` queries the core mapping. A shared-file write
fault applies the same limit inside the address-space invalidate shared lock and the folio lock, and then calls
`page_mkwrite()`. Both entry points add hole blocks to the same delayed set; therefore a non-`SYNC`
FIEMAP can uniformly report the `DELALLOC | UNKNOWN` produced by both write entry points, without scanning dirty folios to guess the allocation
state.

A FIEMAP query enters through an inode operation and reuses the same cached inode-format limit as write/truncate:
for the extent format it is computed from `(2^32 - 1) << block_bits` and the `i_blocks` limit, and for the legacy
format it also accounts for indirect metadata blocks, `huge_file` and the `MAX_LFS_FILESIZE` limit. It then calls the read-only `report_mapping()` over the requested
range; this corresponds to the level at which Linux `ext4_iomap_begin_report()` observes `EXT4_MAP_DELAYED` through
`ext4_map_blocks()`, and inside core it overlays the inode extent-status ranges onto on-disk holes as `BlockMappingFlags::DELAYED`. The private VFS conversion function only walks the unified mapping result: ordinary holes are skipped,
mapped extents emit physical block ranges, unwritten extents get `UNWRITTEN`, and delayed holes get
`DELALLOC | UNKNOWN`. Legacy pointer mappings are merged by contiguous logical/physical blocks and get
`BlockMappingFlags::MERGED`, which the kext4 KVFS layer converts to `FIEMAP_EXTENT_MERGED`. The walk keeps one
pending extent, and adds `LAST` only after confirming that no later mapping exists within the queried range; when the output capacity is full it stops immediately and
does not wrongly mark `LAST`.

Block allocation entry points are split into two layers: the single-group allocation in `balloc` (`allocate_block_run_in_group()`) and the multi-group scan in `mballoc`
(`allocate_blocks_for_write()`). Only the multi-group scan carries the ENOSPC retry valve: a group scan may fail to pick any block because prealloc space
occupies them (the bitmap scan treats those bits as protected and excludes them, while the free aggregate still counts them as free); in that case it first
discards the space of several groups and then rescans. The single-group entry does not retry, because discard proceeds in ascending group order and cannot guarantee reaching the requested
group, and a global discard would only cost other files their locality. The retry upper bound is fixed by `PREALLOC_DISCARD_SEARCH_LIMIT` (3 searches):
a discard can only occur before each search after the first, and when a discard returns 0 (no space to discard) it ends immediately,
because the rescan result would necessarily be the same. Discard works at whole-group granularity and accumulates in ascending group order until `needed` is reached, so it may exceed the requested amount;
`needed == 0` means sweeping every group that holds space. Unlike Linux's double gate of `freed > 0 || discard_pa_seq advanced`,
only `freed > 0` is kept here: the single allocator mutex already serializes selection and discard, so there is no case where another CPU
frees space behind the caller's back. A retry happens only when "a healthy group reports no space": groups skipped by the geometry or descriptor pre-checks
do not set the flag, because discarding cannot change a group's capacity; an `InvalidBlockBitmap` encountered during the scan is remembered and returned in preference to `NoSpace`,
but it does not forbid healthy groups from retrying after reclamation. The valve takes the allocator lock itself, so it must be called outside the scan's locked section,
otherwise it would deadlock by re-entry.

The writeback Hole branch follows the order of Linux `ext4_mb_new_blocks`: `consume_prealloc_for_write()` first
reuses a registered space (a hit marks only the consumed span, with no length normalization and no prealloc
budget spent); only on a miss does `normalize_write_allocation_len()` compute the surplus budget, allocate with
`expected_len = requested` and mark it on disk, after which the birth hook inside the allocation lock section
(`allocate_blocks_for_write_with_prealloc()`) registers the trailing free window. The budget is decremented by
the number of blocks actually registered.

The physical-space admission preceding each run follows the same split: it subtracts the prefix a registered
space can serve from the run's new data demand and sizes the extent-tree estimate to the run that will be
inserted, so a hit is not charged as new space and cannot trigger a prealloc discard before consumption.
Consumption re-validates the prefix under the allocator lock; a space reclaimed in between simply sends the
run down the normal allocation path.

## Concurrency Model

运行态仍需联合修改 superblock/group metadata 的 mutation 当前通过 `Ext4Mount` 内的挂载级 `RwLock`
write guard 串行化；只读调用以及 delalloc reservation 可共享 read guard，后者再使用独立
reservation mutex。KVFS inode cache mutex 只保护 `New/Live/Freeing` identity state；每个 cache
slot 使用自己的等待队列，等待发生在释放 cache mutex 之后，因此一个 inode 的状态变化不会
唤醒其它 inode 的 `iget`。ext4 private state 使用 per-inode `RwLock<Ext4InodeState>`，普通 metadata 读取或
mutation 不进入另一张 inode cache。
核心内部的 metadata buffer
和 JBD2 transaction handle 仍会记录 buffer ownership、credit consumption 和 revoke 状态。
`writepages()`, the two VFS `set_len()` phases, last-writer preallocation cleanup,
and final eviction share the inode's sleepable submission mutex. Writeback may
release it after installing in-flight requests. Mapping mutation and release
acquire it and drain first; truncate retains it through PageCache adjustment and
physical release, and eviction through all release batches. Acquire KVFS
data/invalidate protection before submission protection, then enter the mount.
Never acquire submission protection while holding a mount or inode-state guard.

恢复期只操作未发布 inode，仍由独占恢复流程保护。进入 PageCache
遍历时不持有挂载级 core mutex；PageCache 在释放 mapping/folio mutex 后调用 batch writer，
batch writer 才短暂取得 core write guard；delalloc accounting 由独立 reservation mutex 更新。这样
普通 cache miss 的 `MappingInner -> core` 路径不会与 writeback 形成反向锁序。
FIEMAP 在 VFS inode shared lock 下执行，regular inode 另以 `writeback_lock` 稳定磁盘
mapping；core `report_mapping()` 只短暂读取 delayed extent lock，随后再查询磁盘 mapping，
并在调用安全输出 writer 前释放 core 锁。Buffered write 和 truncate 使用 inode data lock 的 exclusive 侧；
`page_mkwrite` 使用 address-space invalidate shared lock 和 folio lock，truncate 同时使用
invalidate exclusive 侧。三条路径通过同一个 delayed-set lock 发布 mapping/reservation
状态。用户页错误或大输出因此不会持有挂载级 core read lock。

### Shared State and Mapping Protection Foundations

Production still keeps the outer read-write lock of `Ext4Mount`, and this phase does not enable cross-inode data I/O parallelism.
The newly added internal contracts are meant to remove that dependency step by step later, rather than to turn every `&mut self` into shared access.

| State/Operation | Protection in this phase | Remaining preconditions |
|---|---|---|
| 元数据块读改写 | `MetadataWriteAccess::update_bytes` 在同一 slot mutex 下读取最新字节、执行内存更新并发布；失败保留旧值和状态 | callback 不得 I/O、调用 journal 或重新访问该 buffer/cache |
| 共享 inode-table 发布 | `replace_inode_table_entry` 只把已验证的目标 entry 合入最新 block；普通 inode、xattr、分配/释放及 orphan-next 路径共用 | 同一 inode 的逻辑修改仍须串行；此机制不提供多 inode 操作的原子事务 |
| 不可变读取及预热 | `MetadataBuffer` 同时持有 bytes 和 slot 的 Arc；预热 pins 保持至 allocator 锁释放 | pins 不是权威内容快照，锁内仍重读最新 bytes |
| 回收/forget 后移除槽位 | cache index mutex 下校验 slot 唯一引用及 clean、无 frozen、字节无读者等条件 | 整体失效仍是独占恢复/生命周期操作，不能与活跃访问并行 |
| File mapping lifetime | The per-inode submission mutex serializes admission; truncate and VFS release boundaries drain in-flight requests first | Acquire KVFS invalidate/data locks first; core callers still provide operation exclusion |

Calling `snapshot()` and a full-block `replace_bytes()` separately is not an atomic read-modify-write; it is allowed only for full-block rewrites already guaranteed exclusive by an existing
allocator, directory/extent operation, or mount exclusion. A shared inode table must not publish a whole old block this
way, otherwise it would overwrite the new entries of neighboring inodes. The relationship between the slot mutex and the cache index
stays one-directional: the index may inspect a slot; an update callback must not enter the index in reverse. The commit snapshot and
atomic modifications use the same slot mutex; transaction freezing, ordered-data confirmation and the checkpoint overall order are protected by the protocol below.

### Transaction Admission and Persistence Coordination

`MountedJournal::persistence` is a sleepable execution mutex in task context. It covers transaction freeze,
append, space-pressure checkpoint, home flush, tail flush, acknowledgment/retirement, as well as first-time feature initialization
and the final recovery/orphan flag update of syncfs. Internal entry points borrow the same `JournalPersistence` guard,
and pressure reclaim does not reacquire the execution lock. Errors must record a journal abort before the execution right is released, so that the next persistence request
does not treat a failed transaction as completed; a repeated submission of a target that is already persisted or retired succeeds directly, without rewriting to disk or enqueueing again.

`prepare_commit(tid)` first marks running as closing under the transaction state lock, forbidding new handles from joining,
then releases the state lock and waits for existing handles to exit. The last handle stop, transaction phase advance and abort all wake
the progress waiters; after freeze, closing is cleared and new handles may join the next running transaction.
Existing handles can still register metadata/ordered data and stop normally.
A waiter must not hold its own handle, nor the locks required by stop; nesting a begin that waits for new admission while retaining a handle is not allowed. If the
completion callback of an old handle encounters a newer running transaction, it neither resubmits nor misreports an illegal transaction.
When the capacity precheck exceeds the budget, the old transaction is first sealed, and the commit entry then waits for it to exit; reaching exactly the limit still allows joining,
and a single handle exceeding the capacity retains the `NoSpace` contract. Normal active-handle contention is not exposed as a `JournalBusy`
error. Physical space admission is still based on the allocator's pending-reuse records, and the reuse restriction of the corresponding transaction is lifted only after
persistence succeeds; journal coordination does not duplicate physical space state.

Current lock order and I/O boundaries:

```text
KVFS data/invalidate -> inode writeback -> mount mutation
    -> journal persistence -> short journal/transaction, allocator or buffer state critical section
                         -> device I/O (without holding the short-state guards above)
handle stop -> transaction state -> wake after release (does not acquire persistence)
```

Append and checkpoint, from selection through retirement, are protected by the same execution guard; checkpoint uses the frozen version,
the FIFO prefix, the original next-transaction space budget and the ring spare rule. Home writeback and flush come first, then the tail is persisted,
and finally retirement and space release; the unselected suffix and recovery evidence are retained. The append and synchronous drain of `PreserveDuringRecovery`
do not release the execution right. Explicit replay/reset remains exclusive recovery before the mount identity is published,
and does not run concurrently with normal access; this step creates no background checkpoint worker.

Production still retains mount mutation exclusion as well as `&mut Ext4SbInfo`. The capacity precheck/admission between `prepare_reservation` and `begin`
still happens within the same mutation exclusion, and is not a credit ticket that can be detached from that exclusion;
complete block modifications of the allocator/superblock and recovery reloads still obey the original exclusion boundary. Later opening of data and sync concurrency
must adapt to these call windows, and cannot simply remove the outer guard. This step provides the journal-internal wait and persistence protocol,
which is not equivalent to whole-filesystem metadata concurrency or to having already obtained end-to-end performance gains.

N1 has already pinned the journal mapping/superblock, transaction engine and checkpoint queue to the same
`MountedJournal`. N2 will establish the lock order according to the actual concurrency relationships among background commit/checkpoint, inode writeback, metadata buffers and the group
allocator; lock domains are not preset by field grouping. Block I/O must not be performed under a spinlock,
nor may PageCache be waited on or a sleepable lock be acquired.

### Asynchronous data writeback and metadata completion

`file/async_writeback.rs` owns allocation preparation and deferred owned BIO
assembly. `file/mapped_writeback.rs` owns initialized mapping plans, task-bound
BIO waits, and specialized completion. Aligned overwrites without growth prepare
under mount read protection. Holes, unwritten extents, append, and partial blocks
prepare under mount write protection, installing unwritten extents for holes.
All preparation handles end before data submission; neither journal handles nor
mount guards cross data-I/O waits. Preparation changes allocation and unwritten
mappings without initializing extents or advancing `i_disksize`. Space pressure
may safely commit these changes. A later preparation failure retains installed
unwritten prefixes for retry and submits no data BIO from that batch.

Allocation reuses synchronous preallocation consumption, optional surplus
registration, and extent insertion. Once the bitmap and unwritten extent own a
block, its delalloc reservation is released to avoid charging it twice. Failed
data retries reuse the mapping; unallocated holes retain reservations. Optional
preallocation still obeys pending-free admission and the live writable-open count.

BIO assembly and submission run outside mount protection. A single contiguous,
aligned run takes ownership of the PageCache snapshot; multiple runs own separate
buffers. Partial initialized blocks require a synchronous read/modify/write
outside mount protection. Partial unwritten blocks are zero padded, including
the tail beyond appended EOF. Filesystem blocks larger than cache pages retain
the synchronous drain path until block-level exclusion prevents overlapping
partial writes. Allocation, unwritten, and append writes on 1/2/4 KiB blocks use
the asynchronous data path.

The inode submission mutex serializes preparation and publication. Both the
local mapped pipeline and deferred queue use `MAX_INFLIGHT_BATCHES` (four);
each PageCache batch independently holds at most 31 pages. Device callbacks
store results and buffers in IRQ-safe slots and queue a prepared
`kworkng::WorkHandle`. The worker settles requests in submission order.
Allocation/unwritten/growth completion uses the ordered engine's `Completed`
mode: reread live extents, register the ordered-data flush dependency, convert
successfully written unwritten ranges, and advance `i_disksize` no further than
the successful prefix. This mode never rewrites data or allocates holes; it shares
credits, transaction restart, and committed-prefix accounting with `Write` mode.
Data completion is not durability; the pre-commit data barrier remains required.

KVFS writeback always selects the private DataOnly policy: allocation, unwritten
conversion and i_disksize publication remain required, while timestamp-only
metadata belongs to full file sync. Writeback control carries no fsync intent.
Data failures settle only the contiguous successful prefix. If prefix
metadata settlement also fails, folio completion uses the settled length while
preserving the original device error. Failed ranges remain unwritten or retain
their old mapping and stay dirty. The VFS data path no longer drives the private
FullMetadata-only data-error abort branch; real metadata/journal failures still
follow the existing abort policy. Real preparation, submission, and
completion failures enter both KVFS mapping and superblock error channels before
releasing cache completion or waking folio waiters. Preparation failure does not
run post-I/O metadata completion. Queue-capacity throttling is not an I/O error.
Successful retry does not erase an unobserved failure.

Initialized overwrites without growth publish inode metadata, register ordered
data dependencies, and settle reservations without generic extent conversion or
transaction restart. Both synchronous and deferred overwrite completion use this
path, reading current metadata so timestamp changes during data I/O survive.

Preparation proves stable size and mappings, so FullMetadata completion rewrites
the inode-table entry only while resident timestamps are dirty. Every successful
range still joins the current transaction's ordered-data dependency; a barrier
from a previously committed transaction cannot cover later I/O. Later timestamp
updates are staged by another range or full fsync. Failure and reservation
settlement retain their existing rules.

Aligned `All` overwrites use `BioWait` in the submitting task, with no completion
work allocation or queueing. Submission protection lasts through all local
completions; BIO waits hold no mount guard. Drain local batches before allocation
or conversion, and drain deferred requests before returning to the local pipeline.
These transitions and producer exclusion prevent the two bounds from adding up
to eight concurrent batches. Later preparation/admission failure still drains all
previously submitted BIOs and records their errors.

`None` returns after bounded admission and leaves dirty pages when full; preparation
metadata I/O and partial-block reads can still wait. `All` drains old requests
before enumeration, drains local batches, releases submission protection, then
waits for its deferred sequence prefix and flushes existing completion work.
Each worker occurrence handles at most four batches, so later submissions cannot
extend it indefinitely. Truncate, FIEMAP, last-writer close, and eviction acquire
submission protection and drain before changing mappings or releasing blocks.
Workers acquire neither submission nor invalidate locks. Requests retain VFS and
private inodes, the superblock, and cache completion until queue removal and
wakeup. Synchronous callers also wait for task-context capture release so final
eviction cannot cross shutdown. Temporary writeback checks snapshot the current
mapping error generation before waiting for submission protection or existing
I/O, then report failures recorded during the pass without consuming them. They
do not use the open-time sample that inherits unseen history: a successful retry
or clean pass must not reintroduce old mapping errors into syncfs. Each file's
fsync/syncfs cursor independently reports its channel's history; later opens still
inherit unseen errors. Direct submission errors take precedence, after all
required drains and error checks have run.

Range fsync first calls `AddressSpace::write_and_wait_range` for its half-open
byte range, then stages timestamps for full sync and performs the existing inode
journal/device persistence. Empty data ranges still execute filesystem sync;
invalid reversed ranges are rejected by KVFS before dispatch. The filesystem may
persist additional metadata. Raw writepages remains a submission contract even
when the existing All implementation completes data inline; neither raw writeback
nor a generic data wait alone supplies durability.

Background triggering, asynchronous readahead, global writeback throttling, and
sync targets for non-regular inodes, and separate fdatasync metadata targets
remain outside this stage.

## Design Decisions

- The ext4 on-disk format, consistency invariants and inode component are the responsibility of the `kext4` core; resident identity,
  generic attribute semantics, PageCache and open-file reference lifetime are the responsibility of KVFS `VfsInode` alone.
  KVFS accesses the single generic attribute store in that component through attribute operations, and does not maintain a second cache.
- The `kext4` crate uses `#![forbid(unsafe_code)]`, and unsafe or device-specific details stay outside the core boundary.
- Unimplemented ext4 format capabilities are exposed through explicit unsupported errors, to avoid mounting an incomplete format as writable by mistake.
- The new lifetime and I/O semantics of KExt4 land only in the direct KVFS implementation of KExt4, and no second ext4 implementation path is retained.
- The KExt4 core provides only the inode-format-dependent maximum file size and `BlockMapping` carrying mapping flags; the Linux FIEMAP ABI,
  user pointers and output capacity stay at the POSIX/KVFS boundary, and the `vfs` module is only responsible for converting mapping semantics into extents.
- `Ext4SbInfo` retains the overall mount state similar to Linux `ext4_sb_info`; only the journal, which has an independent transaction state machine and
  lifetime invariants, is aggregated as `MountedJournal`, and services are not mechanically created for code grouping.
- KExt4 holds a real `BlockDevice` and submits block reads/writes and flush through the public synchronous adapter; requests, completion notification and VirtIO
  interrupt queues belong to the block/driver layer. KExt4 may merge requests and wire in once the generic interface is available, but does not establish a private driver
  bypass. The current transaction restart still needs a synchronous `flush` to establish the ordered-data durability boundary; only after the generic
  block layer provides request completion/FUA or an equivalent dependency can the data and
  commit of multiple transactions be safely pipelined, and the filesystem layer must not remove this barrier directly.
- A speculative prealloc space is memory accounting only: it is premarked neither in the bitmap nor in the
  in-core free-extent cache (Linux marks the buddy and requires it to track the PA lifetime), and the two
  indexes are the only record; a space reaches the bitmap only when consumed, through the normal allocation
  path (Linux likewise marks only the consumed span). The price is that a space stays invisible to the free
  aggregate, so "aggregate nonzero yet no run can be selected" becomes a normal state, handled by the ENOSPC
  retry valve rather than by changing the accounting.
- `MB_GROUP_PA` is not implemented: kext4 has no locality-group PA producer, and requests smaller than `EXT4_PREALLOC_MIN_WRITE_BLOCKS`
  are neither normalized and enlarged nor produce a surplus, so small writes do not register a space and need no
  group-level fallback when an inode PA misses.
- A shrink truncate trims the space at the new EOF and retains the prefix below it (Linux discards the whole file's
  preallocations here): a kext4 space carries an exact logical range and has a group twin entry that must be kept in sync, and the prefix remains valid and usable.
- ENOSPC discard works at whole-group granularity and does not partially release a group's space: this keeps the simple invariant "either the whole promise remains or the whole range is gone",
  and avoids breaking up the contiguous ranges of multiple groups for a single request.
- errseq, clean unmount/freeze and the complete fault matrix depend on the final background execution graph and are placed together in N3; they
  do not block the N2 main path, but remain a mandatory gate before replacing the old backend.

## Drop / Resource Release

### Deferred Persistence of Automatically Written Timestamps

Buffered writes use KVFS `generic_file_write_iter_with_prepare` to combine ext4
size admission, journal/mount writability checks, and automatic timestamp
publication. The callback runs under the inode data lock, before folio locking
and user-data copying; zero-length or rejected writes do not update timestamps.
It checks live immutable policy before generic size limits, including for empty
requests, so already-open quota files remain protected during quota activation.
Shared writable faults retain KVFS `file_update_time()`. Both entries use VFS
`current_time()` and its superblock range and precision clipping, including when
`KFEAT_FS_TIMES=n`. This feature does not
disable VFS write-time or explicit timestamp updates; the adapter's optional
timestamp helper must not substitute epoch values for these updates.
For regular-file automatic mtime/ctime updates, `InodeOperations::update_time`
checks journal writability,
then compares and updates both fields and their dirty bits under one resident-state
write lock. KVFS neither reads an old snapshot nor republishes callback results.
Unchanged values preserve existing dirty state and superblock timestamp precision.
Other timestamp updates use KVFS `generic_update_time`, also used by the default
callback, to publish only fields confirmed by `setattr`. Explicit setattr/utimensat
retains its journaled mutation path.

`prepare_buffered_write` reads the mapping format, applies its file-size limit,
and updates timestamps under one inode-state write lock. `Ext4Mount` retains the
immutable timestamp/bitmap-size capabilities, huge-file feature and journal
identity established before mount publication. Buffered admission reads the live
journal abort projection through this retained identity and checks VFS writability;
it does not acquire mount protection while holding the inode data lock.

Write-begin first checks the existing initialized-range cache under inode-state
protection. A supported extent inode with complete initialized blocks below both
visible and disk size, and no delayed reservations, needs no mapping lookup or
allocation. This hit can proceed while journal persistence owns mount. KVFS data
protection (or invalidate protection for writable faults) excludes truncate and
fallocate; mapping mutation invalidates the cached proof under the inode-state
lock. Cache misses, holes, unwritten extents, partial EOF blocks and extensions
retain mount-protected mapping and reservation validation. Folio misses still use
the normal backing read. No new cache or synchronization request state is added.

Each inode-table mutation first captures the pending timestamps and merges them in before its own explicit field modifications;
after staging, only the captured values are acknowledged, and fields that change again during I/O keep their latest in-memory values and their dirty state.
xattr and zero-link mutations follow the same rule; growing the extra inode fields does not overwrite
not-yet-persisted automatic timestamps with old on-disk values. A failed staging does not acknowledge dirty fields.

Full fsync writes back timestamps that are still dirty after the data writeback and then commits the journal; this step is still performed even if fdatasync has already cleaned all
folios. A mapped data-only overwrite may flush only the data, without starting a new transaction for the timestamps.
The KVFS `write_inode` hook writes back metadata-only dirty inodes for syncfs/unmount; the final eviction of a linked inode
also stages the timestamps first. Timestamps becoming immediately visible in memory and being persisted on disk are two separate phases;
background inode writeback is outside the scope of this change.

### Fsync data waiting and transaction progress

Each fsync/fdatasync caller completes its own PageCache writeback and in-flight
data wait. Full fsync also stages pending timestamps before selecting the inode's
metadata transaction target. There is no inode request queue or allocated fsync
batch: transaction state owns commit progress, and MountedJournal serializes
persistence. A target already committed by another caller requires no repeated
journal commit, but still requires a device flush for potentially newer data.
An inode without a tracked target retains the conservative mount-wide fallback.

KVFS checks each open file's independent error cursor after its filesystem callback.
Concurrent full/data-only requests keep their own metadata intent and data wait;
an earlier caller's completed scan cannot satisfy a later caller's dirty data.
Existing submission exclusion and truncate/close/eviction drains protect mapping
changes. This removes request batching without adding a background commit worker
or relaxing the current mount/persistence lock order.

Namespace removal verifies the inode's data reclamation format before modifying directory entries. `unlink`, `rmdir`, and overwriting
`rename` allocate an orphan-file slot when the link count drops to zero, and use the legacy list when the orphan file is full; removing an
ordinary inode that still has other hard links does not allocate an orphan entry. Until the last VFS reference is released, the inode bitmap and data remain allocated.

Allocated metadata/data blocks are released through the journaled bitmap helper. The inode deletion path first cuts off
directory reachability, and protects zero-link cleanup with the orphan file or the legacy fallback; if the inode has an external xattr
block, it first frees the block or decrements its refcount and clears `i_file_acl`/`i_blocks`, then truncates the
extent-backed data, cleans up the inode metadata, and finally releases the inode bitmap entry.

The runtime kext4 KVFS layer, on the last writable-file `release()` with no delayed
data reservation, only cleans initialized leftover mappings beyond `i_disksize`
(garbage from a crash-recovery scenario); unwritten preallocation persists across
close and is freed only on truncate or deletion (a real landed hole punch will also
reclaim it; the posix layer's current PUNCH_HOLE write-zero emulation does not free
blocks and instead converts unwritten to initialized). fallocate-sourced unwritten
matches Linux on-disk semantics: `ext4_release_file()` discards only mballoc's
in-memory reservations (pa), not extent entries; kext4 likewise reclaims only the
in-memory writeback speculative reservation via `release_preallocs()` on close
(`prealloc_space.rs`), so on-disk unwritten that persists across close comes only
from fallocate itself and is freed on truncate or deletion. The writeback
speculative reservation (the `normalize_write_allocation_len` surplus, capped at
`EXT4_MAX_PREALLOC_BLOCKS`, 4MB@4K) is never materialized as an on-disk unwritten
extent: it only registers in-memory space, never touches the bitmap, and leaves
zero on-disk residue across crash/unmount/close, matching Linux writeback
speculative reservation (invisible to the extent tree, returned on close), so
`st_blocks`/df do not diverge from Linux; the payoff is that fallocate's unwritten
extent is converted in place on a subsequent write after reopen, avoiding
reallocation. close does not additionally force ordinary dirty PageCache
writeback; data writeback is handled by `fsync`/`syncfs` and the generic writeback
path. When the last `VfsInode` reference goes away, the superblock hook first drops
PageCache/剩余 delalloc accounting，再对 nlink=0 inode 组合持有的 ext4 private state 调用
final eviction。KVFS 在 hook 前完成 `Live -> Freeing`，所以 cleanup 期间没有普通 VFS 能力；
三阶段 core API 始终借用该 `VfsInode` 组合持有的同一 private component；它不返回可逃逸的
eviction token 或另一种 inode handle。完成后 KVFS 精确删除旧 cache entry 并唤醒同号 `iget`。
nlink 非零的 cache eviction 不释放磁盘 inode；后续 `iget` 可从仍分配的磁盘 inode构造新的
private component。

Speculative prealloc space reclamation hangs off the file/inode lifecycle and writeback settlement. The last
writable close drops all of the file's spaces only when no delalloc reservation remains; with reservations it
keeps them and lets writeback settlement or eviction reclaim them (Linux `ext4_release_file()`'s
`i_reserved_data_blocks` gate). Writeback settlement reclaims once the reservations reach zero and no
writable open remains (Linux `ext4_da_update_reserve_space()` with `i_reserved_data_blocks == 0 &&
!inode_is_open_for_write`); the writable-open gate is the live writer count, read at the settlement point.
The VFS drops that count only after the release hook returns, so a settlement racing the decrement still
observes a writable open and keeps the spaces; a later settlement or eviction reclaims them. No per-inode
close state is kept, so a writer that reopened the inode is judged by its current count.
While the file is still open for write, the spaces this writeback produced stay for a later writeback run to
reuse. Eviction drops every space before checking nlink, including cache eviction with nonzero nlink: a
group-side twin holds a strong reference to its owner, and a missed drop would let the entry outlive the owner
object. An inode with no spaces is detected empty via the per-inode map's leaf lock, avoiding the allocator
mutex for an empty reclaim; the birth of this same writeback completes before that check, same-inode
births/consumes are serialized by the writeback lock, and close/eviction are blocked by the mount lock this
settlement holds, so an empty result here cannot miss a space registered by this call. Every reclaim point
only updates the in-memory indexes and returns blocks to the allocatable pool, without disk writes or journal
credits; a block is never marked in the bitmap before it is consumed, so there is no disk state to roll back.

Recovery does not create a resident inode table. While the mount/VFS identity has not yet been published, it obtains
temporary private state through an orphan-aware decode, and reuses the normal truncate or zero-link eviction. If cleanup fails, the recovery evidence
is preserved and the filesystem is prevented from being treated as successfully mounted.


## Quota-file storage and synchronization

`src/quota.rs` implements `kquota::QuotaIo` with a borrowed `Ext4QuotaIo`
containing a shared `Ext4SbInfo` borrow and optional mutable `JournalHandle`
write authority.
Active-handle extent, inode and xattr edits mutate their existing locked state;
mount admission, recovery and commit retain exclusive lifecycle access.
A write context supplies the caller's transaction; a read context has no handle.
This transient adapter owns no mount,
file registry or quota state. The superblock's `QuotaInfo<Arc<VfsInode>>`
retains each backing inode, and I/O borrows that inode's `Ext4Inode` component.

Quota reads map blocks directly and read the metadata cache, so pending
journaled payloads are visible before checkpoint. Reads clamp to EOF, span
filesystem blocks and zero holes/unwritten mappings. Writes must fit one
filesystem block. New mappings allocate exactly one block without speculative
preallocation; unwritten mappings are initialized to zero. Payload, extent,
bitmap and inode size changes enter the caller's journal handle. Inode size
metadata is dirtied only on growth; mapped overwrites need one payload credit. Ordinary
PageCache/ordered-data writeback does not carry active quota-file contents.
The writer admits the transaction before quota I/O and completes or aborts it
after the complete format operation. Shortage is checked before each payload
mutation using existing extent/inode credit bounds plus its payload block.
Physical admission must cover every payload/extent allocation and also
`inode_extra_isize_allocation_blocks(inode)` whenever inode metadata may be
dirtied. Extra-isize migration can allocate an external xattr block; extending
a journal handle supplies credits but does not admit physical allocations.
Callers use the existing `metadata_journal_for_allocation` boundary before
opening the handle; the storage adapter never starts nested admission.

`Ext4SuperOperations::sync_fs` takes mount write exclusion, locks the canonical
quota state, writes each dirty acquired dquot in a bounded handle, writes dirty
file information, then drains the filesystem journal and checkpoints. The
ordered-cache cursor advances past failed records and retains the first error
while attempting later items. An inactive dirty dquot reports `EIO` without
quota-file I/O; acquisition owns initialization and its larger transaction.
This adds no persistent dirty queue. Native read and write errors pass through
`fail_metadata_mutation` before conversion to VFS errors. Format corruption aborts the journal even before publication; an ordinary
error without published updates stops the handle without aborting it;
errors after publication use the existing abort boundary. Final filesystem
synchronization is still attempted. Lock order is mount state then quota state;
backing I/O uses the already borrowed state and cannot reacquire the mount.

Internal user/group/project quota files are selected by the decoded superblock inode
numbers. Activation validates user inode 3, group inode 4 and the non-reserved
project range before loading any class; malformed identities report EUCLEAN.
Their canonical VFS references live only in `SuperBlock::quota_info`.
`Ext4SbInfo` keeps a weak back-reference to that owner; inode components do not
mirror quota state or hold another filesystem reference. Namespace lookup rejects
internal quota identities before consulting the inode cache. Quota-file block
allocation is exempt by the same on-disk identity, avoiding recursive charging.

Mount enables usage accounting before orphan cleanup. Recovery acquisition
propagates the enclosing recovery policy through journal admission and handle
completion, retaining evidence until the whole cleanup finishes. A clean read-only mount
loads no quota files and performs no accounting writes. Recovery may temporarily
enable supported accounting, then releases records and files and drains the
journal before publishing a read-only mount. Read-only remount drains writeback and releases
cached records in bounded transactions before suspending accounting. After
pinning its live inode snapshot, suspension waits for earlier inode evictions
without holding mount mutation protection. Suspension
retains the canonical quota files and class information; final shutdown alone unloads them. Failed
read-write initialization restores suspension so a later remount can retry.
Resident inode construction and read-only open do not acquire owner records or
require a writable journal. Read-write remount resumes the retained quota
classes without acquiring owners for read-only resident inodes; read-only
remount admission has already excluded writable file descriptions. Writable
open initializes its owners, while namespace, metadata, xattr, truncate and
eviction mutations initialize their affected owners before journal admission.

Owner records are initialized in separate admitted transactions before ordinary
inode operations take reservation/allocator locks. The format supplies its
maximum distinct-block write bound. Each touched 1-KiB quota block can occupy
a separate filesystem block, including holes reused below EOF. Physical and
journal admission therefore cover one allocation and extent insertion per
touched quota block through the supported extent depth, plus inode extra-isize
migration. Rewriting a block already owned by the handle consumes no additional
journal credit. Namespace creation initializes the new owner before inode bitmap
mutation. Inode and physical-block policies charge cached records before
allocation, undo unsuccessful admission, reconcile partial runs, and journal
dirty usage records in the same handle as bitmap changes. Dropping a shared
external-xattr reference releases that inode owner's charge even when the
physical block remains allocated for other inodes. Fallocate uses the same
allocation policy and reserves owner-record writeback credits for each batch. Raw bitmap operations
retain their physical role. Up to four affected inodes contribute user/group/project
record credits to an ordinary mutation.

Physical allocation, reservation claims and releases update the resident inode's
allocated sectors together with its cached quota records under the inode state
lock. The block allocator refunds both counters when allocation fails or returns
a shorter run. Extent and external-xattr updates persist the resident block
count in the same transaction as their metadata changes; quota backing inodes
still maintain their own block count without charging themselves.
External-xattr detach and shared-block replacement validate the inode sector
count and bound quota records under mount mutation protection before releasing
the old reference or allocating its replacement. Eviction performs the same
validation before publishing orphan state.
Regular-file `stat` includes delayed-allocation reservations alongside allocated
sectors, while the persisted inode block count contains only allocated space.

Each resident inode binds its user, group and project records to the quota cache before
mutations. Binding imports the inode's existing delayed reservations, and
ownership changes publish the new records with the new owner. Newly allocated
directory and symlink inodes bind already acquired records during their initial
transaction. Delayed-allocation admission updates cached reservations while holding the
existing inode state lock, before allocator/delalloc aggregate locks. It performs
no quota-file I/O. Range validation and inode-counter overflow checks precede
quota and mount admission. After admission, extent and counter publication cannot
return an error. Mount admission failure refunds quota without replacing the
admission error. The common physical-allocation entry admits quota before
selecting either a preallocation hint or a free run. Both outcomes claim the
overlapping reservation
at allocation success and remove the corresponding inode/mount reservation;
later writeback settlement releases only reservations still present. Allocation
or data-I/O failure after publication follows the journal abort/recovery policy.
Allocation rollback preserves the allocation error and reports cleanup failures
separately, aborting the journal for fatal cleanup errors.
Truncation and eviction free actual usage; invalidation frees reservations.
Chown first performs optional extra-isize expansion under the original owner.
It then admits both new owners atomically using the complete allocated sectors
and outstanding reservations, journals both old/new records, and persists the
owner change. A failed expansion cannot publish a new resident owner.

Final VFS shutdown invokes `put_super` after ordinary inode writeback and dcache
eviction. Quota deactivation first drains all live inode I/O queues and flushes
their completion captures outside mount mutation protection; failed ordinary
writeback may have left another inode's completion in flight. Ext4 releases each cached record in a bounded transaction, disables one
class, and returns its retained file to the mounted owner. The mounted owner
ends mutation protection before dropping the file; the core helper is private
to quota integration. A final journal drain persists structural release changes. Writeback status and the
retained file are returned separately within that scope, so an error cannot
prevent unlocked file release or processing later classes. After journal abort,
final disable
invalidates class cache/info without I/O; the original error is returned and
disk recovery flags and committed journal evidence remain intact. No copied file registry,
dirty queue or parallel usage counter is introduced.
`usrquota`, `grpquota` and `prjquota` select limit enforcement for internal quota files.
Without the on-disk QUOTA feature, they authorize external quota-file activation
through the VFS quota operations. Accounting and limits are then loaded together.
Project quota requires the on-disk project feature. A new child inherits the
project ID when its parent has the project-inherit flag; directories also inherit
the flag. Links and renames into such directories require matching project IDs.
Changing an inode's project ID transfers its allocated space, delayed-allocation
reservations and inode charge together, before publishing the new identity.
Project-limited directories report their available resources through `statfs`.

The asynchronous allocating and mapped-completion writeback transactions reserve
owner-record credits at their existing journal admission boundary. Quota claims
share the unwritten allocation transaction, and completed overwrites reuse
existing allocation without another charge.

## Quota control and external files

`src/vfs/quota.rs` implements the shared quota operation table. The syscall
adapter owns command validation, credential checks, and ABI conversions; the
filesystem owns backing-file admission, transactions, and inode policy. No
parallel control-state object or file registry is introduced. The mount policy
mask is retained by `Ext4SbInfo`; usage, enforcement and suspension remain in
the existing `QuotaInfo` class state.

External activation requires a regular file on the same superblock and a quota
mount option. Activation excludes file-data mutation, revokes mapped access,
drains writeback, invalidates cached contents, and marks the resident inode `NO_QUOTA` before
loading its header. Direct metadata I/O uses the existing journal adapter.
Persistent immutable/no-atime flags protect the file from ordinary access.
The resident inode publishes their corresponding runtime behavior flags when
disk attributes change, preserving independently updated quota-exclusion state.
Existing writable inodes acquire their records before activation returns.

Quota synchronization drains file and quota writeback, checkpoints the journal,
and invalidates external quota-file pages so quota tools observe current disk
records. Disable drains delayed allocation, releases records in bounded
transactions, unloads the retained file, and restores external-file policy.
References are released outside native mount protection. Read-only suspension
retains files and resumes accounting on a writable remount. Final shutdown uses
the same external-file cleanup, preserving protection after journal abort.

Record acquisition first loads the canonical cache without write authority.
Existing records need no physical-growth admission; missing records retain the
format's bounded tree-growth admission before activation. Administrative queries
use the same acquisition semantics, including creation of missing records.
Grace updates overwrite the existing header with bounded journal credits and no
physical-growth admission. ABI limits and identities are checked before mutation.
Resource-capability bypass is selected from the current user caller at the
quota integration boundary; kernel writeback consumes existing reservations
without independently acquiring a user's privilege.

External quota activation first excludes buffered mutation, revokes existing
mappings, flushes their dirty pages and synchronizes the filesystem before
reading the file as quota metadata. Ordinary writes and shared write faults
participate in the superblock lifecycle admission that excludes activation,
writer initialization, reservation drain and class teardown.

Disable stops class accounting and detaches resident inode records before
release. Outstanding reservations stay on their inodes and are imported on a
subsequent activation; an eviction already in progress finishes before record
release. Read-only suspension instead requires reservations to be drained.
After the final record/tree/free-list/header updates it commits, checkpoints and
flushes the metadata domain before unloading the backing file. Under file-data
exclusion it then invalidates live mapped/cache state, restores external inode
policy and drains that policy update before admitting ordinary writes again.

### Generic selection and buffered acceptance

The existing inode submission mutex spans TOWRITE selection, scanning, queue
admission and complete rollback of unsent batches. Workers do not acquire it.
The callback borrows the same WritebackControl as the scanner; no mode copy or
throttled flag is retained in WritebackPass. A full queue under NONE accounts
the declined batch as skipped and drops its owner before returning WouldBlock.
ALL waits admission. Generic preparation waits/revalidates selected busy folios,
so the entry-wide queue drain before selection is removed. The exit still waits
the finite queue prefix and flushes completion captures, covering old clean and
detached owners. Mapped submission retains its earlier-allocation metadata drain.

write_end settles delalloc reservations and publishes accepted i_size under the
copied folio lock; its fast path takes no cache/lifecycle wait. Short-accept and
copy-failure rollback waits happen after that lock is released. Generic scans
sample live EOF per folio and charge attempted work independently of completion
prefixes. Existing inode, extent and queued-I/O lifetime owners remain unchanged.
