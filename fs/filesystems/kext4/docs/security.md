# KExt4 Security and Reliability Analysis

## Scope

This document analyzes the current KExt4 implementation at
`fs/filesystems/kext4`. It covers hostile or corrupted ext4/JBD2 bytes,
filesystem-operation inputs, concurrency, device failures, recovery, and
resource exhaustion. It does not treat a valid filesystem image as trusted
merely because it was created by ext4 tooling.

The analysis describes implemented controls separately from residual risk and
recommended hardening. A documented recommendation is not an existing
guarantee.

## Trust model

| Asset or property | Required protection | Trusted enforcer | Relevant untrusted influence |
| --- | --- | --- | --- |
| Kernel memory | Disk metadata must not cause unchecked indexing, arithmetic wrap, uninitialized-value use, or unsafe Rust execution | KExt4 checked codecs and Rust's safe type system | Every metadata block and length derived from the device |
| Filesystem metadata | Preserve ext4 ownership, allocation, checksum, journal, and lifecycle invariants | KExt4 storage algorithms, buffer layer, and mounted journal | Crafted image metadata, namespace requests, xattrs, and device failures |
| File data | Do not expose a new mapping before ordered data writeback reaches the required point | KExt4 ordered writeback plus KVFS page-cache ordering | File writes, write faults, allocation pressure, and I/O completion |
| Journal and recovery evidence | Do not erase the only recoverable copy of committed or partially checkpointed metadata | `MountedJournal` and `Ext4SbInfo::recover` | Crafted JBD2 records and failed writes or flushes |
| Resident identity | Keep one resident inode and page cache per mounted inode number | KVFS inode table and lifecycle; KExt4 uses one private component | Concurrent lookup, open-unlink, rename replacement, and inode-number reuse |
| Ownership and policy state | Preserve UID, GID, mode, immutable/append flags, and supported xattrs | KVFS authorization and KExt4 persistence | Unprivileged filesystem requests and on-disk values |
| Availability | Reject malformed input in checked paths and bound work where implemented | KExt4 validators, credit limits, and bounded cleanup batches | Extreme images, fragmentation, request ranges, and slow devices |

KExt4 trusts the following contracts, but not arbitrary values crossing them:

- KVFS supplies kernel-owned path components, dentries, resident inodes,
  address spaces, lifecycle callbacks, generic locking, read-only admission,
  and permission checks.
- The syscall and user-access layers validate user pointers and copy data
  before KExt4 sees it.
- The block layer provides the claimed device and reports transfer and flush
  failures accurately.
- Kernel synchronization primitives implement their documented mutex,
  read/write lock, and wait behavior.

The following inputs are untrusted:

- primary and backup ext4 metadata read from the block device;
- group descriptors, bitmaps, inode tables, extent nodes, directory records,
  and extended-attribute blocks;
- JBD2 superblocks, descriptors, commit records, revoke records, and log
  positions;
- file names, xattr names and values, offsets, lengths, and mount options
  received through KVFS; and
- device completion status and short or failed I/O.

KVFS authorization remains part of the security boundary. KExt4 persists and
reports supported metadata, but it is not a standalone discretionary-access
control or Linux Security Module implementation.

## External Boundaries and Attack Surface

| Input or operation | Source | Entry point | Influence | Implemented validation or isolation | Failure behavior |
| --- | --- | --- | --- | --- | --- |
| Mount options and device identity | KVFS mount request | `Ext4MountOptions::parse` and `Ext4SuperOperations::fill_super` | Mount mode, statfs policy, and selected storage | KVFS supplies a claimed canonical device; KExt4 parses only supported options | Reject the mount through a KVFS error before root publication |
| Primary superblock and group metadata | Block device | `Ext4SbInfo::open` through `Ext4DiskSuperblock::decode` and group decoders | Filesystem geometry, features, metadata addresses, and allocation state | Checked decode, feature negotiation, geometry bounds, descriptor and bitmap checksums, and system-zone construction | Return a typed bounds, checksum, corruption, or unsupported error; do not publish the mount |
| Inode, extent, directory, and xattr blocks | Block device | `Ext4MetadataIo::read_block` followed by the corresponding `disk`, `extent`, `dir`, or `xattr` decoder | File mappings, namespace results, ownership, sizes, and metadata mutation targets | Checked arithmetic and format bounds, record and tree validation, checksum checks, namespace checks, and block-ownership validation | Fail the lookup or operation; a failure after journal publication follows the abort policy |
| JBD2 records | Internal journal blocks on the device | `Ext4SbInfo::recover` and `jbd2` replay paths | Metadata replay, revoke decisions, journal head/tail state, and recovery evidence | Supported-feature checks, bounded ring mapping, checksums, sequence checks, transaction assembly, and revoke processing | Reject recovery and keep the filesystem unpublished without intentionally clearing final recovery evidence |
| Names, offsets, sizes, and xattr values | KVFS operation callers | KVFS callbacks in `src/vfs.rs` and the `namei`, `file`, `truncate`, and `xattr` modules | Namespace, file contents and mappings, inode length, and attributes | KVFS owns copying, credentials, permission checks, and conflicting-operation serialization; KExt4 checks type, range, maximum size, name, namespace, and storage invariants | Return a KVFS error; ordered writeback can report a completed prefix and retain the suffix |
| Read, write, and flush completion | Block-device implementation | `FilesystemDevice` and `Ext4MetadataIo` | Loaded bytes, transaction durability, checkpoint progress, and sync result | Exact-range request construction and explicit device-result propagation; journal paths track the current phase | Return the device error; metadata mutation may abort the journal and require recovery/remount |

Feature bits are retained by class during decode so an unknown incompatible
bit cannot disappear through a lossy conversion. `validate_mount` rejects
unknown incompatible features, BIGALLOC, and filesystems without extent-based
mapping.

## Unsafe code inventory

KExt4 declares `#![forbid(unsafe_code)]` at the crate root. There are no
permitted unsafe functions or unsafe blocks inside the crate.

This is a local memory-safety control, not proof that the filesystem is
semantically safe. Bounds checks, checksums, ownership validation, lock
contracts, and device error handling remain necessary. Unsafe device or
user-memory operations, if any, are outside this crate and are governed by
their owning subsystem.

## Memory-Safety and Integrity Invariants

### Checked representation

Disk bytes become typed state only through checked decoders. Offset, length,
block-number, byte/block conversion, and capacity calculations use checked
arithmetic or fallible conversions. A checksum is verified before use whenever
the active ext4 or JBD2 format requires one.

### Geometry and ownership

Every referenced filesystem block must be within the mounted layout. Metadata
and release paths distinguish data ownership from protected system zones.
Block and inode allocation changes must update the relevant bitmap, group
descriptor, primary aggregate, and checksum within the associated metadata
transaction.

### Transaction ownership and durability

A mutable metadata buffer has one transaction owner at a time. Journal credit
accounting bounds the metadata set. The transaction state moves forward from
running to committing, checkpoint, and finished; an abort is a separate
mount-level failure condition.

Metadata that exposes newly written file data is committed only after the
ordered data write succeeds. Committed journal evidence remains until
home-block checkpoint succeeds. Recovery markers are not intentionally
cleared before replay and orphan cleanup are durable.

### Resident identity

KVFS owns the unique resident `VfsInode` and page cache for a
`(SuperBlock, inode number)` pair. KExt4 stores one `Ext4Inode` private
component in that object. Namespace changes and final eviction operate on the
same component rather than decoding a parallel live object by inode number.

### Delayed allocation

KExt4 owns inode delayed-allocation ranges, the inode reserved-block count, and
the mount aggregate. Allocator and aggregate counters are protected by their
documented locks. Correctness also relies on KVFS serializing conflicting
inode data operations; direct concurrent use that bypasses this boundary is
outside the supported contract.

## Thread Safety

| Shared state | Concurrent accessors | Protection and scope | Ordering or context constraint | Residual concern |
| --- | --- | --- | --- | --- |
| Mount-wide ext4 state | KVFS read, mutation, writeback, and sync callbacks in task context | `RwLock<Ext4SbInfo>` protects coordinated mount state | Broad mutation takes the write guard; code may block and must not run in interrupt context | A long mutation or I/O under the write guard delays unrelated operations |
| Allocation and mutable group state | Block/inode allocation, release, statfs, orphan, and replay reload | `Mutex<AllocatorState>` protects mutable group data and aggregates | Take before the delayed-allocation mutex; do not recursively call locking accessors | Coarse locking reduces parallelism; accidental re-entry would fail |
| Delayed-allocation total | Reserve, release, truncate, writeback, eviction, and statfs | Dedicated mutex protects the mount total | Allocator first when both are required; inode-range consistency also relies on the KVFS data-operation contract | A cross-object bug can drift the aggregate |
| Per-inode ext4 state | Lookup metadata, namespace mutation, truncate, and writeback | Per-inode read/write lock protects decoded and transient inode state | Non-reentrant; current implementation prefers readers | Sustained readers can delay a writer |
| Per-inode writeback | Competing writeback and sync activity for one inode | Dedicated mutex serializes each writeback pass | Page-cache scanning releases page-cache locks before entering the storage core | One slow pass delays later writeback |
| Metadata buffer cache | Concurrent metadata readers, mutators, commit, and checkpoint | Cache-map and per-slot locks protect publication, bytes, ownership, and waiters | Wait for I/O only through the slot protocol; failed initialization is removed | Device latency keeps waiters blocked until completion or failure |
| Journal transactions and log state | Metadata mutators, sync, recovery, and checkpoint | Journal-owned locks and phase checks protect ownership, log space, abort, and progress | Phase transitions move forward; an abort rejects later mutation | One fatal journal error affects the complete mount |
| Resident inode identity and page cache | Concurrent lookup, open files, namespace changes, writeback, and eviction | KVFS owns the resident table, data locks, and page cache | KExt4 must use the existing private component and must not access user memory under its locks | Bypassing KVFS violates the caller-dependent identity and ordering guarantees |

The lock order for paths that require both counters is allocator state before
the delayed-allocation aggregate. User-memory access must not be introduced
under KExt4 mount, inode, metadata-buffer, or journal locks.

## Threat Analysis

| ID | Asset or boundary | Threat and trigger | Severity | Implemented control and evidence | Residual risk | Recommended hardening |
| --- | --- | --- | --- | --- | --- | --- |
| T1 | Kernel memory; disk codecs | A crafted image supplies a malformed length, offset, tree node, or block number that would cause out-of-bounds access or arithmetic wrap | High | Checked decoders, fallible integer conversions, checked arithmetic, layout bounds, and `#![forbid(unsafe_code)]` | Parser bugs or missing cross-field validation remain possible | Maintain malformed-image and boundary-value tests for every codec and traversal |
| T2 | Mount capability boundary | An unsupported feature is accepted and later mutated with incomplete semantics | High | Unknown incompatible bits, BIGALLOC, and missing EXTENTS are rejected before mount publication | Unknown read-only-compatible bits other than explicitly checked BIGALLOC are not comprehensively gated by current `validate_read_only` | Define a supported read-only-compatible mask and reject unknown bits on writable mounts |
| T3 | Journal and recovery evidence | A crafted JBD2 log redirects replay, wraps the ring incorrectly, or attempts to erase recovery evidence | High | Supported-feature checks, checksums, bounded ring mapping, transaction sequencing, revoke handling, and replay-before-publication | Complex valid-looking journals may expose untested state combinations | Expand adversarial replay tests across wrap, revoke, checksum, and interrupted-checkpoint states |
| T4 | Allocation integrity | A crafted extent, bitmap, or xattr pointer causes KExt4 to allocate or free a system or foreign block | High | Layout and group validation, bitmap checks, system-zone protection, mapping validation, and checked release paths | Cross-structure consistency checking is not equivalent to a full offline `e2fsck` pass | Add targeted ownership cross-checks and corruption tests around every release path |
| T5 | Metadata durability | A malicious or failing block device acknowledges, rejects, or reorders a write or flush so metadata is only partially durable | High | Ordered data-before-metadata flow, journal commit/checkpoint separation, explicit device-error propagation, journal abort, and preserved recovery state | KExt4 relies on the block layer's completion and flush contract; resident state cannot be rolled back after every failure | Add fault injection at each write and flush boundary and document remount requirements to callers |
| T6 | Namespace and authorization boundary | An unprivileged request attempts to bypass ownership, mode, immutable, append-only, or xattr namespace policy | High | KVFS performs caller-dependent generic authorization; KExt4 persists owner and flag state and exposes only selected xattr namespaces | POSIX ACL xattrs are not exposed because ACL evaluation, inheritance, and mode coupling are absent; `security.*` policy depends on higher layers | Add an end-to-end ACL and security-label design before exposing those mechanisms as authorization controls |
| T7 | Resident inode lifecycle | Concurrent open-unlink, rename replacement, and inode-number reuse are triggered so mutation or cleanup targets a stale object | High | KVFS `New/Live/Freeing` identity, one private component, orphan tracking, and final eviction after the last VFS reference | The protection is caller-dependent; a cross-layer ordering bug could leak blocks or delay cleanup | Keep concurrent open-unlink and reuse tests and assert lifecycle transitions at the boundary |
| T8 | Availability | A crafted but structurally valid image or request creates extreme fragmentation, directory/xattr work, journal pressure, or lock hold time | Medium | Format bounds, checked capacities, bounded eviction batches, journal credit limits, and explicit no-space errors | Coarse locks and synchronous checkpointing permit latency amplification; some scans scale with image content | Add measured complexity limits, safe cancellation points, and large-image stress tests |
| T9 | Delayed-allocation accounting | Concurrent write, truncate, writeback, or eviction requests attempt to drift inode and mount counters and over-admit space | High | Dedicated reservation locks, allocator-before-delalloc ordering, checked counters, and KVFS serialization of conflicting inode operations | The invariant spans multiple protected objects and depends on callers using the supported adapter | Add concurrency assertions and stress tests that compare interval totals, inode counters, and the mount aggregate |
| T10 | Directory hashing | A crafted image selects encrypted, case-folded, or keyed name semantics so a mutation would choose the wrong directory leaf | High | Supported hash versions and tree structure are validated; unsupported keyed behavior fails explicitly | Images using unsupported name semantics cannot be safely modified | Keep failure before metadata access and add feature-negotiation tests for each name mode |

Severity expresses worst credible impact for the stated boundary, not the
likelihood of exploitation.

## Failure Modes and Effects Analysis

| ID | Failure mode | Cause | Local effect | System effect | Detection | Handling | Severity |
| --- | --- | --- | --- | --- | --- | --- | --- |
| F1 | Metadata decode or checksum fails | Corruption, damaged media, or a short block | The current object is not constructed or updated | Mount or operation fails without accepting the metadata | Bounds, format, and checksum validation | Return a typed corruption, bounds, or checksum error | 2 |
| F2 | Journal commit, checkpoint, or device flush fails | Device error or injected I/O fault | Transaction cannot reach the requested durability phase | Later mutations and sync may fail for the entire mount | Device result and journal phase checks | Abort the journal, preserve recovery evidence, and require recovery/remount | 2 |
| F3 | Allocation or journal credit is exhausted | Full or fragmented filesystem, allocation failure, or transaction limit | Current run cannot continue | Operation returns no-space or a completed writeback prefix | Checked admission, allocator result, and credit accounting | Commit safe prefixes where supported; retain dirty suffix and reservations for retry | 3 |
| F4 | Recovery or orphan cleanup fails | Invalid log, invalid orphan link, unsupported cleanup, or I/O failure | Recovery does not complete | Mount is not published | Replay validation, orphan checks, and device errors | Return failure and avoid clearing final recovery evidence prematurely | 2 |
| F5 | Unsupported feature or format is encountered | Image requires unimplemented semantics | Checked construction or mutation stops | Mount or operation is unavailable | Feature and per-object format checks | Return `Unsupported` before metadata mutation | 3 |
| F6 | Writer is delayed or operation stalls | Reader-preferring lock, coarse mount lock, synchronous checkpoint, or slow device | Lock wait or long operation latency | Filesystem throughput and responsiveness degrade | Timing, watchdog, and workload observation outside KExt4 | Bound batches where implemented; investigate lock splitting without weakening invariants | 3 |
| F7 | Metadata-buffer load fails | Read error, checksum error, or invalid block | Waiting callers cannot obtain a ready buffer | Dependent operation fails | Slot state and loader result | Wake waiters, remove the failed slot, and allow a later retry | 3 |
| F8 | Partial ordered writeback succeeds | A later allocation, data write, metadata update, or commit fails | A byte prefix is complete and the suffix remains dirty | Caller sees partial progress and an error | `Ext4WritebackResult` completed-byte accounting | Complete only whole successful page-cache ranges and retain the suffix for retry | 3 |

Severity uses the X-Kernel FMEA scale: `1` is fatal or unrecoverable, `2` is
serious, `3` is moderate and recoverable, and `4` is minor.

## Failure Handling

Failures before metadata publication are returned without intentionally
aborting an otherwise healthy transaction. Once a metadata mutation has been
published, failures that can compromise transaction consistency invalidate or
abort the affected journal state. Corruption, checksum, device, credit,
ownership, and illegal-state failures are not converted into success.

An aborted journal rejects later metadata mutation and synchronization on that
mount. KExt4 does not promise in-place rollback of all resident state after
such a failure. Recovery and a new mount are the supported route back to a
trusted state.

Ordered writeback reports the completed byte prefix. KVFS must not mark the
uncompleted suffix clean. Mount recovery is fail-closed: if replay, mutable
metadata reload, or orphan cleanup cannot establish a checked state, the root
is not published.

## Privacy

KExt4 processes file contents, file names, ownership metadata, timestamps, and
extended-attribute values. These may be sensitive even when the filesystem
structure is valid.

The current log sites in `src/vfs.rs` report error categories and operational
metadata such as inode numbers, offsets, lengths, and size values. They do not
intentionally print file contents or xattr values. Directory iteration also
logs the inode number when it skips a non-UTF-8 name, not the name bytes.
Operators should still treat the reported identifiers and ranges as sensitive
metadata.

File and xattr bytes remain in the KVFS page cache, KExt4 metadata buffers, and
the underlying device according to those owners' normal lifetimes. KExt4 does
not provide disk encryption, secure deletion, traffic confidentiality, or
protection against an attacker who can read the device.

## Known Limitations

| Missing or incomplete property | Affected boundary | Current consequence or residual risk | Workaround or caller obligation | Recommended hardening |
| --- | --- | --- | --- | --- |
| Complete writable gating for read-only-compatible feature bits | `FeatureSet::validate_read_only` during mount | An unknown bit may require a restriction that KExt4 does not derive | Mount only images whose enabled features are known to be supported and verify unfamiliar images offline | Define and enforce a supported read-only-compatible mask before writable publication |
| Full ext4 format coverage | Mount, directory, inode, journal, and xattr formats | External journals, BIGALLOC, orphan-file format, inline data, encrypted or case-folded names, and several xattr variants are rejected | Use an image created with the supported feature set | Implement and test each missing format before advertising it |
| POSIX ACL enforcement | KVFS xattr and permission boundary | Stored ACL semantics cannot be used for access decisions through this adapter | Rely on the currently supported mode and higher-layer policy; do not treat hidden ACL bytes as enforced | Design ACL evaluation, inheritance, mode coupling, and VFS exposure end to end |
| Security-label enforcement | `security.*` xattr boundary | Storing a label does not make it an active authorization control | The higher security layer must interpret and enforce the label | Connect xattr storage to an explicit label-policy hook and test denial paths |
| Per-inode journal synchronization | `sync_inode` | Syncing one inode may commit unrelated work in the running transaction, increasing latency and coupling failures | Callers must tolerate a stronger mount-wide commit point | Track per-inode data and metadata transaction IDs |
| Fine-grained progress under contention | Allocation, metadata mutation, inode state, and checkpoint | Coarse locks, reader preference, and synchronous checkpointing permit latency amplification | Run these paths only in sleepable task context and treat long waits as filesystem-level stalls | Split locks and add asynchronous checkpointing only with preserved revalidation and lock-order invariants |
| Standalone storage-core concurrency and authorization | Direct calls that bypass `src/vfs.rs` | Identity, permission, read-only, page-cache, and conflicting-operation guarantees are absent | Use KExt4 through KVFS | Encode more boundary requirements in types if direct use becomes a supported interface |
| Full offline consistency checking | Mount validation | A globally inconsistent image may pass the structures examined by mount and fail later | Run `e2fsck` on untrusted or damaged media before use | Add targeted cross-structure validation where online cost is justified |
