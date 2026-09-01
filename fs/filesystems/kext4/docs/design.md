# KExt4 Design

## Purpose

KExt4 is X-Kernel's ext4 implementation for the Kernel Virtual File System
(KVFS). It turns a block device containing a supported ext4 image into a
device-backed KVFS mount.

The crate owns the ext4-specific parts of that mount:

- decoding and validating on-disk metadata;
- block, inode, and extent allocation;
- JBD2 journal discovery, recovery, transactions, and checkpointing;
- directory, namespace, orphan, truncate, and extended-attribute updates; and
- ordered-data writeback and filesystem synchronization.

KVFS owns pathname traversal, dentries, resident inode identity, page-cache
state, generic file lifetime, and read-only operation admission. The block
layer owns device discovery and request execution.

## Scope and boundaries

| Boundary | KExt4 responsibility | Other subsystem responsibility |
| --- | --- | --- |
| KVFS | Implement filesystem, superblock, inode, file, and address-space operations | Resolve paths, cache dentries and inodes, own open files, and manage page-cache objects |
| Block layer | Translate validated ext4 filesystem blocks into device requests | Provide the canonical block device and execute reads, writes, and flushes |
| On-disk ext4 | Validate supported metadata and preserve ext4/JBD2 update rules | The image is treated as untrusted input |
| Process and syscall layers | Return typed KExt4 or KVFS failures | Validate user pointers, credentials, and syscall arguments before invoking KVFS |
| Credentials and policy | Persist ownership and expose supported inode and xattr state | Enforce generic permissions and security policy at the VFS boundary |

The crate does not implement device discovery, user-memory access, a second
inode cache, or a second page cache. It also does not promise support for every
ext4 feature accepted by Linux; unsupported formats are rejected at a checked
boundary.

## Integration

Enabling `KFEAT_FS_EXT4` links the crate into the kernel. The
`register_init` entry in `src/lib.rs` registers the static `ext4` filesystem
type. Mount requests then use the ordinary KVFS device-backed mount path:

1. KVFS selects and claims the canonical block device.
2. `Ext4SuperOperations::fill_super` prepares checked `Ext4SbInfo` state.
3. If preparation reports `NeedsRecovery`, KExt4 replays the journal and
   cleans the legacy orphan list, then prepares the mount again.
4. KVFS installs the mount-private state and initializes the root inode.
5. A writable mount persists the selected directory-hash policy before the
   root is published.

Failure before the final publication step leaves no usable KExt4 root in
KVFS. Recovery errors are returned to the mount caller.

## Architecture

| Component | Responsibility | Owned state or resource | Called by | Delegates to |
| --- | --- | --- | --- | --- |
| `Ext4SuperOperations` and KVFS adapters | Translate KVFS operations into checked ext4 operations and translate failures back to KVFS | Static operation tables; no mount identity | KVFS mount, inode, file, and address-space paths | `Ext4SbInfo`, `Ext4Inode`, and KVFS helpers |
| `Ext4SbInfo` | Validate and coordinate one mounted filesystem | Layout, frozen geometry, allocator state, delayed-allocation total, device, metadata I/O, system zones, and mounted journal | KVFS adapter and ext4 storage algorithms | Disk codecs, metadata buffers, allocator, and journal |
| `Ext4Inode` | Hold the ext4-private state of one resident inode | Checked inode metadata, extent state, delayed-allocation ranges, and writeback serialization | The resident `VfsInode` and KExt4 algorithms | Extent, directory, xattr, truncate, and writeback paths |
| Metadata buffer layer | Resolve physical metadata blocks and coordinate mutation ownership | Per-block cache slots, loaded bytes, dirty state, transaction ownership, and checkpoint snapshots | Metadata decoders and mutation paths | `Ext4MetadataIo` and `MountedJournal` |
| `MountedJournal` | Recover and serialize JBD2 metadata transactions | Journal mapping and superblock, running transaction state, abort state, and checkpoint queue | Mount recovery, metadata mutation, sync, and buffer layer | JBD2 codecs and filesystem device I/O |
| Storage algorithms | Implement ext4 layout-specific lookup and mutation | Operation-local paths, plans, and checked decoded values | KVFS adapter and `Ext4SbInfo` | Metadata buffer, allocator, journal, and block device layers |

Control normally enters through a KVFS operation, borrows the resident
`Ext4Inode` and mount `Ext4SbInfo`, and then invokes a storage algorithm.
The algorithm decodes or updates metadata through the buffer layer; mutation
also obtains a journal handle before publishing dirty metadata. Device results
and typed `Ext4Error` values return through the same path to KVFS.

### KVFS adapter

`src/vfs.rs` implements the KVFS operation tables. It translates KVFS objects
and errors at the filesystem boundary, obtains the mount-private
`RwLock<Ext4SbInfo>`, and attaches exactly one `Ext4Inode` private component to
each resident `VfsInode`.

The operation tables are static and contain no mount or inode identity.
Mount identity lives in the KVFS superblock private object; inode identity
lives in KVFS's resident inode table.

### Mount state

`Ext4SbInfo` in `src/superblock.rs` owns validated state for one mount:

- a frozen decoded primary superblock and filesystem layout;
- frozen block-group geometry;
- mutable allocation counters, group state, and orphan state;
- the mount-wide delayed-allocation reservation total;
- the filesystem device and metadata I/O layer;
- protected system zones; and
- an optional mounted journal.

Frozen geometry is the sole source for bitmap and inode-table addresses during
normal operation. Mutable counters and flags are rebuilt and revalidated after
journal replay.

### Inode state

`Ext4Inode` in `src/inode.rs` is the ext4-private part of a resident KVFS
inode. It owns the inode number, decoded and transient ext4 state, delayed
allocation ranges, and a per-inode writeback lock. It does not own resident
identity or the final reference count.

`RawInode` and the structures under `src/disk/` are checked representations of
disk bytes. They are values, not runtime identity objects.

### Metadata buffer layer

`src/buffer/` caches metadata by physical filesystem block. A cache slot moves
through loading, ready, failed, dirty, journaled, and writeback-related states.
The layer coordinates concurrent loaders, associates mutable buffers with
journal transactions, and retains checkpoint snapshots until home-block
writeback completes.

A failed initial load wakes waiters and removes the unusable slot so a later
caller can retry instead of observing partially initialized bytes.

### Journal

`MountedJournal` owns the mounted JBD2 state, transaction coordinator, journal
mapping, and persistent journal-superblock state. One transaction progresses
through:

`Running -> Committing -> Checkpoint -> Finished`

Commit makes the journal record durable. Checkpoint copies committed metadata
to its home blocks and advances reusable journal space. These are distinct
durability phases. The current checkpoint worker is invoked synchronously by
journal operations; KExt4 does not create a background checkpoint thread.

### Storage algorithms

The remaining modules implement cohesive ext4 mechanisms:

- `disk` provides checked on-disk codecs, feature flags, and checksums;
- `extent` and `file` map blocks and perform ordered writeback;
- `balloc`, `ialloc`, and `mballoc` manage block and inode allocation;
- `dir`, `dirhash`, and `namei` implement lookup and namespace changes;
- `truncate` and `orphan` implement shrink and final cleanup; and
- `xattr` manages inline and external extended attributes.

## Execution Context and Calling Constraints

| Operation | Context and prerequisite | Blocking or callback behavior | Enforcement |
| --- | --- | --- | --- |
| Filesystem registration | Kernel initialization must have reached the initcall that links `KFEAT_FS_EXT4` | Registration itself does not perform disk I/O | Build configuration and initcall ordering |
| Mount and recovery | Sleepable task context with a KVFS-claimed block device | May allocate, wait for locks, read and write metadata, checkpoint, and flush | KVFS provides the device and call context; KExt4 validates mount state |
| Lookup and metadata reads | Published mount and resident KVFS objects | A cache miss may wait for metadata I/O | KVFS calls the adapter from task context; KExt4 does not test interrupt state |
| Allocation and namespace or xattr mutation | Writable KVFS operation with the required inode and namespace serialization | May allocate, wait, start or extend a transaction, and issue metadata I/O | Read-only and permission admission are caller-dependent KVFS guarantees; KExt4 enforces format and transaction invariants |
| Ordered writeback and truncate | Resident inode and address space under the KVFS data-operation contract | May block on the inode writeback lock, device I/O, journal commit, or checkpoint | KVFS provides page-cache ordering; KExt4 serializes inode writeback and storage mutation |
| `sync_inode` and filesystem sync | Published mount in task context | May commit the running transaction, checkpoint metadata, and flush the device | The caller selects the required sync point; KExt4 returns durability failures |

`Ext4MetadataIo::read_block` may sleep. It must not be moved into a lock
region whose contract excludes blocking I/O. KExt4 locks are non-reentrant;
a path holding allocator or inode state must use guard-local helpers instead
of recursively acquiring the same lock. The crate has no interrupt-context
entry point and does not enforce task context itself.

## Core flows

### Mount and recovery

`Ext4SbInfo::open` reads and validates the primary superblock before deriving
layout. It validates the feature set, group descriptors, checksums, geometry,
system zones, and journal location before producing mount state.

When recovery is required, `Ext4SbInfo::recover` scans and replays committed
JBD2 transactions, reloads mutable ext4 metadata, cleans the legacy orphan
chain, checkpoints the resulting updates, marks the journal empty, and clears
recovery evidence only after the associated work is durable. Mount preparation
then starts again from disk state.

### Lookup and inode publication

A lookup validates the directory entry and asks KVFS
`get_or_try_init_inode` for the inode number. KVFS reserves the resident slot,
KExt4 decodes one `Ext4Inode` private component, and KVFS publishes the
`VfsInode`. Concurrent lookups reuse or wait for that identity rather than
creating a KExt4-side cache.

### Namespace and metadata mutation

Create, link, unlink, rename, and xattr updates calculate required journal
credits, obtain transaction-owned metadata buffers, stage checked byte
changes, and publish the corresponding in-memory state. An error after a
transaction has published metadata invalidates or aborts that transaction
according to the journal failure policy.

An unlinked inode remains resident while KVFS references exist. Final eviction
releases delayed-allocation reservations, prepares orphan cleanup, releases
data and metadata in bounded batches, and finally releases the inode number.

### Ordered writeback

KVFS scans dirty page-cache ranges without holding a KExt4 mount lock.
`Ext4AddressSpaceOperations::writepages` serializes a pass with the inode
writeback lock. For each batch, it obtains the mount write lock and calls
`writeback_ordered_at`.

The storage core rechecks mappings, allocates holes from existing reservations,
writes data before committing the metadata that exposes it, converts unwritten
extents where required, and returns a completed byte prefix if a later batch
fails. KVFS completes only the successfully written prefix and leaves the
remaining pages dirty for retry.

### Truncate

`set_len` first validates the requested size and prepares the ext4 metadata
transition. KVFS then calls `truncate_setsize` at the page-cache ordering point.
For a shrink, KExt4 removes delayed-allocation ranges and finishes releasing
disk mappings and orphan state. Keeping page-cache size publication in KVFS
prevents a second, divergent inode-size owner.

### Synchronization

Inode synchronization commits the running transaction conservatively when it
cannot prove that the transaction is unrelated to the inode. Filesystem
synchronization completes journal commit/checkpoint work and flushes the
device. This is intentionally stronger than a per-inode transaction-ID design.

## State and lifecycle

### Mount lifecycle

| Current state | Event or entry point | Preconditions | Next state | Observable effect or rejection |
| --- | --- | --- | --- | --- |
| Unpublished | `fill_super` prepares the device | KVFS owns the device claim; superblock, features, geometry, and journal validate | Prepared | Checked `Ext4SbInfo` exists but no root is visible |
| Unpublished | Preparation returns `NeedsRecovery` | Recovery feature or orphan evidence is present | Recovering | Journal replay and orphan cleanup run before preparation is retried |
| Recovering | `Ext4SbInfo::recover` completes durably | Replay, cleanup, checkpoint, and recovery-marker updates succeed | Unpublished | Mount preparation restarts from disk |
| Prepared | KVFS installs private state and initializes the root inode | Root inode and writable hash-policy update succeed | Published | KVFS may return the mounted root |
| Published | KVFS tears down the superblock after references are released | VFS lifecycle obligations are complete | Dropped | The mount-private device, cache, allocator, and journal owners are released |

Any validation, recovery, root-initialization, or policy-persistence failure
rejects publication and returns an error through KVFS.

A journal abort is not a normal transaction phase. It is a mount-level failure
condition: later metadata mutations and synchronization return the stored
failure until recovery and remount establish new state.

### Resident inode lifecycle

| Current state | Event or entry point | Preconditions | Next state | Observable effect or rejection |
| --- | --- | --- | --- | --- |
| Absent | `get_or_try_init_inode` reserves an inode number | No resident slot exists | New | One caller becomes the initializer; competitors wait |
| New | KExt4 decodes and constructs `Ext4Inode` | Disk inode and kind validate | Live | KVFS publishes one resident `VfsInode` |
| New | Initialization fails | Decode, allocation, or publication fails | Absent | Reservation is removed, waiters wake, and the error is returned |
| Live | Last resident reference begins final eviction | KVFS has published `Freeing` | Freeing | New lookups wait; KExt4 cleans the same private component |
| Freeing | `evict_inode` and KVFS teardown complete | Delayed allocation and zero-link cleanup have finished or returned an error | Absent | The old slot is removed before inode-number reuse |

### Metadata buffer lifecycle

A metadata block has one cache slot per mount. Loading is single-owner with
waiters. Ready buffers may become transaction-owned and dirty, then retain a
checkpoint snapshot until their home-block write completes. Load or write
failures remain explicit errors and do not turn unvalidated bytes into ready
metadata.

## Concurrency model

| State | Synchronization | Contract |
| --- | --- | --- |
| Mount-private state | `RwLock<Ext4SbInfo>` | Reads may share; mutations that coordinate filesystem-wide metadata take the write guard |
| Allocator state | `Mutex<AllocatorState>` | Protects mutable group descriptors, allocation counters, orphan state, and free-extent caches |
| Delayed-allocation total | `Mutex<u64>` | Protects the mount aggregate independently of block-group allocation |
| Inode state | Per-inode `RwLock<Ext4InodeState>` | Readers share checked inode state; mutation takes the exclusive guard |
| Inode writeback | Per-inode mutex | Prevents overlapping writeback passes for one inode |
| Metadata cache | Cache-map and per-slot locks | Coordinates one loader, transaction ownership, dirty state, and waiters |
| Journal | Transaction and mounted-journal locks | Serializes transaction phase changes, log-space accounting, abort, and checkpoint state |
| Resident identity and page cache | KVFS locks | KExt4 must not create a competing cache or hold its locks while accessing user memory |

Where both allocator and delayed-allocation state are needed, the order is
allocator first, delayed-allocation second. Code holding an allocator guard
must use guard-local state rather than call an accessor that reacquires the
same non-reentrant lock.

The per-inode read/write lock is reader-preferring in the current kernel
synchronization implementation. A sustained read workload can therefore delay
a writer. The coarse mount write lock also limits parallel metadata mutation
across independent block groups.

Delayed-allocation range updates combine inode-local ranges with a
mount-aggregate counter. Correctness depends on the KExt4 locks and the KVFS
serialization contract for conflicting inode data operations; callers must not
bypass the KVFS adapter and perform conflicting updates concurrently.

## Resource ownership and cleanup

- KVFS releases dentries, open files, resident inode slots, and page-cache
  pages.
- KExt4 final inode eviction releases delayed reservations, data mappings,
  extended-attribute blocks, orphan state, and the inode allocation.
- `Ext4SbInfo` owns the device reference, metadata cache, allocator state, and
  mounted journal for the lifetime of the KVFS superblock.
- The journal retains committed metadata evidence until checkpoint succeeds.
- Failed metadata-buffer loads wake waiters and discard the failed cache
  entry.
- Device and journal failures are returned; cleanup must preserve recovery
  evidence rather than report a clean filesystem prematurely.

## Design decisions and trade-offs

### Checked Rust only

- **Context:** The crate interprets attacker-controlled disk bytes and performs
  complex metadata mutation.
- **Decision:** Enforce `#![forbid(unsafe_code)]` for the complete crate.
- **Rationale:** Parsing and update code cannot introduce a local raw-pointer
  escape around checked representations.
- **Trade-off:** Logical, arithmetic, ownership, and concurrency invariants
  still require explicit checks and tests.

### Fail closed on unsupported formats

- **Context:** KExt4 exposes mutation APIs, while several ext4 formats require
  allocation or recovery semantics that are not implemented.
- **Decision:** Require extent-based mapping and reject BIGALLOC, external
  journals, orphan-file format, and other unsupported paths explicitly.
- **Rationale:** A format is not published when later mutation could not
  preserve its invariants.
- **Trade-off:** Some images accepted by Linux cannot be mounted or modified.

### One identity owner per layer

- **Context:** Open-unlink, concurrent lookup, and inode-number reuse require
  all users to observe one resident object.
- **Decision:** KVFS exclusively owns resident inode and page-cache identity;
  KExt4 stores one ext4-private component in that object.
- **Rationale:** Namespace mutation, open files, writeback, and eviction update
  the same state.
- **Trade-off:** Direct use of storage-core mutation outside the KVFS lifecycle
  is unsupported.

### Commit and checkpoint remain separate

- **Context:** A durable journal commit may still be the only recoverable copy
  of metadata that has not reached its home block.
- **Decision:** Model commit and checkpoint as separate transaction phases.
- **Rationale:** Recovery evidence and journal space remain owned until
  home-block writeback finishes.
- **Trade-off:** Synchronization may wait for both phases and checkpoint state
  must remain resident longer.

### Coarse locking before speculative parallelism

- **Context:** Allocation, journaling, inode state, and metadata buffers carry
  cross-structure invariants.
- **Decision:** Serialize broad mutation with the mount write lock and protect
  allocator state with one mutex.
- **Rationale:** The current ownership and failure paths have an explicit,
  reviewable order.
- **Alternative:** Per-group or per-operation locks could permit more unrelated
  mutation in parallel.
- **Trade-off:** Independent operations contend and blocking I/O can amplify
  latency. Any future split must preserve revalidation, transaction ownership,
  lock order, and the page-cache boundary.

## Non-goals and current limitations

This design does not claim full Linux ext4 feature parity. Notable limitations
include unsupported external journals, BIGALLOC, orphan-file format, encrypted
or case-folded directory names, inline data, and several extended-attribute
storage variants. Legacy non-extent filesystems cannot be mounted for normal
operation.

The current implementation uses synchronous checkpoint work, a coarse
mount-wide mutation lock, reader-preferring inode locks, and conservative
whole-running-transaction inode synchronization. These are current behavior,
not requirements for future implementations. Security consequences and
additional format limitations are recorded in `docs/security.md`.
