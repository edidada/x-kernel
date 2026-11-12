# kvfs - Security and Reliability Analysis

## Dynamic directory disappearance

An exited procfs child can vanish after `child_names` returns its snapshot.
SimpleDir skips that child's `NotFound` while enumerating subsequent live
entries. It does not suppress permission, allocation or I/O errors, and direct
lookup of the missing child retains its normal failure behavior.

## Parallel lookup result publication

The parent children mutex protects both candidate membership and the decision
to expose a completed cache hit. Checking `PAR_LOOKUP` only after releasing
that mutex could expose an obsolete negative candidate after successful
replacement or non-cacheable lookup, causing spurious `NotFound` errors for
existing names. Pending candidates stay discoverable for lookup serialization,
but neither fast nor slow lookup consumes them as results. Tests cover candidate
retraction, replacement, and retention of genuine completed negative entries.

## Prepared mount-state ownership

Private-owner allocation errors occur before the SimpleFs root factory runs and
before a superblock is published. Once transferred, a prepared owner's address
is stable; the superblock drops it exactly once, including after root initialization
failure. Block-backed initialization still uses the registry's existing failure
transition to wake waiters and release the device claim. Tests cover pointee
address preservation, teardown, failed root initialization, and rejection of
invalid owner flags before root construction; they do not force global OOM.
Nested Arc/Vec/String allocations and root construction remain infallible where
they were infallible before. This is not whole-mount OOM safety.

## Fallible directory iterators

Iterator allocation errors propagate before directory entries are emitted, so
this failure leaves the directory position unchanged. Child-iterator errors and
outer-owner allocation errors release already-created iterators. Vec/String
snapshots and lazy name formatting remain infallible allocations: migrating the
iterator owner is not a guarantee of end-to-end OOM-safe directory enumeration.

## Mount-transaction private-state allocation failure

Both FsContext setters allocate fallibly before changing their slot. An OOM
leaves the old state intact; initialization errors destroy already installed
state without publishing the failed transaction. tmpfs/ext4 propagate errors
instead of converting them to fatal OOM. Tests cover erased-state replacement
and cleanup after initializer failure; they do not force global allocator OOM.
Default KBox allocation requires sleepable task context and reclaim-compatible
locking, even though the backend does not currently reclaim. This does not
make parsers, nested Vec/String/Arc allocations or superblock creation fallible.

## File-lock context allocation failure

The inode's first advisory-lock context allocation can return `NoMemory`.
`Once::try_call_once` publishes only a fully initialized `KBox`; on error the
cell stays uninitialized and no lock request reaches `apply`. Later attempts
may retry. Both record-lock and BSD-lock entry points propagate the error.
`KBox::new_nowait` prevents this spinning initialization path from opting into future
sleeping reclaim. This change does not make subsequent lock-record, waiter,
or other VFS allocations fallible.

## Test Fixture Scope

Demand-read cancellation releases only the wait registration, leaving submitted
initialization owned by `FolioRead`. Writes reject invalid sleeping context
before `write_begin`, and truncation does so before publishing size. Filesystem
callbacks must restore the admitted context before returning, allowing mandatory
rollback and writeback cleanup to complete without a new admission failure.

The audit scope includes guest fixtures under `ut-tests/` and the host-only
path fixture under `tests/host/`. Guest modules remain private children of
their original production parents and require `cfg(unittest)`; the host
module retains `cfg(test)`. Address-space fixture nesting, readahead
observation, and test-only production hooks remain unchanged.
All 17 serial flags are preserved. Moving the fixtures adds no production
entry point and changes no unsafe, registration, locking, or cleanup contract.

## Named cdev identities and projection

Named registration validates class/name path components and rejects overlapping
ranges or duplicate class/name identities before publishing any identity. Handles expose no unregister authority;
only the range owner may call cdev_del. Retired snapshots cannot supply backend
operations or claim availability, even if the number is later reused. Callbacks
run in sleepable task context outside registry locks, and consumers must
serialize snapshot installation with their callback handling. No registry lock
is held across filesystem invalidation or device callbacks.


## Captured Poll Source Boundary

Backends choose the captured wait source; KVFS does not infer identity from
readiness or retain a file/process owner through `FilePollSource`. Disjoint
events and expired sources register nothing and allocate nothing. Capture
errors propagate without publishing a registration. Consumers must retain
successful registrations, recheck readiness, and keep an expired binding
distinct from an absent binding. Registration may run waker callbacks when
its temporary source owner is released, so conflicting outer locks are forbidden.

## Trust Model

A plain open of a magic link follows the Path it holds, rather than re-resolving the
display name used for readlink. The target is still subject to the magic-link follow policy, the link count limit, and the may_open permission check;
this guarantees that a retained old path is not replaced because an object with the same name was re-created.

Character-device ranges are accepted only from kernel registration; empty ranges, minor overflow, and any overlap within the same major are
rejected at the registration boundary. Unregistration accepts only a complete range and does not allow splitting other registrations. The retirement
marker of a registered object only transitions from valid to invalid; the inode weak cache cannot keep an old registration or backend alive indefinitely.
When a cache hit races with unregistration, the already selected backend may continue to execute open; unregistration is not a synchronous drain,
so the backend must check instance admission on its own. Callbacks, failed registrations, and the final backend destruction after unregistration all happen outside the table lock.

Access by device number (`DeviceId`) is constrained by the global
device-access policy (see design.md, "Device-access policy (reverse
injection)"): when a registered policy denies the access, the
`open_character_device`, block-device open and `mknod` enforcement points
reject with `VfsError::OperationNotPermitted` (`EPERM`). The protected assets
are device-number access itself and the policy slot. With no policy
registered, access is unrestricted.

User-provided paths, open flags, rename flags, mount flags, and mount data are untrusted. The POSIX syscall layer
is responsible for copying user memory and completing the initial ABI validation; `kvfs` receives only kernel-owned path strings, opaque mount-data
byte slices, and typed flags.
Directory entries, extents, and metadata returned by a concrete filesystem must also be treated as external input that may fail. `kvfs` validates name, mount relationship, type, topology, and operation flags before committing
namespace state.

`Cred` comes from trusted task state, but its UID/GID is not a "privilege guarantee" and can only serve as DAC input.
The caller is responsible for obtaining a stable `Arc<Cred>` at the operation entry point; `kvfs` is responsible for making the entire path walk, the final check,
and the namespace mutation use the same passed-in object.

`CommandFile` does not trust the read callback to always return content; `Ok(None)` returns
`VfsError::InvalidData` instead of triggering a kernel panic.

## External Boundaries / Attack Surface

- `Filename::open_with_flags_at` and `dentry_open` are compatibility entry points that preserve raw `O_*` bits, and they normalize the
  raw bits into `OpenParams` and `OpenFlags`.
- `Filename::open_with_openat2_at` receives the raw `struct open_how` triple
  (flags/mode/resolve) copied from user space by the POSIX layer, which checks
  the size and reserved bytes; kvfs validates the triple through
  `OpenHow::from_openat2` before it reaches namei.
- A device open callback can inspect the transient `O_NOCTTY` only through the narrow interface of `VfsFileBuilder`, which avoids
  exposing or misusing the full set of creation-time flags.
- `sys_renameat2` converts the raw rename bits into `RenameFlags` before entering the VFS.
- Filesystem operation traits may return errors and metadata produced by disk, network, or device backends.
- The optional `InodeOperations::fiemap_operations()` can output the backend block mapping, but it may pass checked fields only through the borrowed
  `FiemapExtentInfo` and a safe writer; it cannot retain the writer or touch user pointers;
  the callback runs inside the inode shared lock.
- The `&Cred` of every pathname and namespace mutation API is a permission boundary; omitting or replacing it changes
  the authorization subject of the current operation.
- `VfsFile::cred()` exposes the immutable credential captured at open time, for file operations that need Linux `f_cred`
  semantics to read.
- The superblock default dentry operations accept only kernel-constructed static tables; once a dentry is bound, the operation identity cannot be
  installed or replaced from user input.
- `AnonInodeFs::global()` is the runtime entry point for creating anonymous files, but it accepts only a singleton already
  initialized during the boot phase; it does not trigger global VFS initialization directly from user input.
- `Path::render_from()` explicitly distinguishes whether the requested root is reachable or unreachable. When crossing mounts, the
  mountpoint itself must be included; the empty dentry name of a mounted root must not be treated as a path component, and the `..` semantics of an ordinary
  `Path::parent()` must not be used to skip the mountpoint.
- The filesystem type registry accepts only kernel-constructed static descriptors; a user-provided type name can only perform an exact lookup
  and cannot install or replace registry entries.
- `FsContext::data()` exposes only the opaque mount-data slice that the syscall layer has already copied with bounds; the filesystem
  type must interpret its own text or binary representation within the synchronous `get_tree()`, and copy any
  typed state that needs to be kept long-term, and must not retain user pointers or the borrowed slice.
- Filesystem mutation results enter the VFS cached inode attributes; an incorrect identity or immutable
  geometry is untrusted.
- Xattr names and values come from the syscall boundary; KVFS accepts only kernel-owned `XattrName` and value slices, and
  namespace permissions must be checked before the filesystem callback is invoked.

`kvfs` does not directly dereference user pointers and does not directly access MMIO, PIO, DMA, or architecture FFI.

## unsafe code inventory

| Location | Call path | Purpose | Invariant / safety argument |
|---|---|---|---|

The `unsafe` encapsulation of the file-lock shared node lives in the private `locks/node.rs`: range access relies on unique ownership,
and the wait relationships and cancellation unlinking are protected by a global wait lock. The intrusive list uses the `intrusive-collections`
typed adapter and the safe `clone_pointer()`, and does not adjust Arc reference counts by hand. The rest of the namespace
model maintains memory safety through safe Rust locks, `Arc` ownership, and operation traits.

## Memory-safety invariants

- 写回错误返回前保留已完成页的预算更新，并结束所有已开始的 folio writeback；
- `LockedDentry` 的 location guard 限定借用的 dentry name 生命周期。
- `parent` 和 `name` 在同一个 location write lock 下同时替换。
- rename 期间 source dentry 和对应 `VfsInode` 保持存活。
- 目录 inode 至多关联一个 live dentry；重复 lookup 必须复用该 alias，不能建立第二套
  child cache 和 topology location。
- create-like callback 成功前必须实例化 VFS 提供的 negative dentry，不能返回另一个
  未经事务校验的对象。
- lookup miss 在 filesystem callback 前发布带 parallel-lookup 状态的 hashed negative
  dentry；同名 lookup 必须等待 owner 完成，不能并行建立第二个 candidate。
- filesystem callback 不能直接修改 VFS cache internals。
- 每个 raw flags 家族必须在边界转换为对应 bitflags 类型。
- 未知 open/rename 位不得进入内部 namespace 或 open 算法。
- `AtomicU32` 中的 `f_flags` 只通过 `OpenFlags` API 读写。
- 不同 flags 类型不得通过 `.bits()` 在 VFS 内互相转换。
- `FiemapExtentInfo::fill_next_extent()` 必须拒绝零长度及 logical/physical 末端溢出；
  delayed、encrypted 和 inline/tail 状态必须补齐 Linux FIEMAP 规定的隐含 flags。
- FIEMAP `SYNC` 写回和 extent 遍历必须由同一次 inode data read lock 覆盖，不能在两阶段
  之间允许 buffered write 建立新的 delayed allocation；shared write fault 通过文件系统
  mapping lock 和 delayed-state lock 发布 reservation，不依赖 inode data lock。
- live child dentry 强持有 parent，parent 只保存 child 的弱索引；superblock dcache
  强持有 hashed dentry，驱逐时必须同时移除弱索引和 dcache 所有权。
- dentry 继承的 operation table 必须与其唯一 superblock 的默认 `s_d_op` 对应物一致；
  绑定后不得更换 table 或重绑 superblock。
- pseudo filesystem 的 magic、block size、最大文件大小和 root inode 由通用 helper 一次
  安装；具体实现不得在 private state 复制另一组 geometry/identity。
- `Nameidata` 不持有 credential；一次操作的调用者必须在整个方法链中传递同一个 `&Cred`。
- The `openat2(2)` `RESOLVE_*` constraints must be enforced uniformly by namei,
  not by replacing the root in the syscall layer: every `BENEATH`/`IN_ROOT` path
  transition must verify the result stays inside the anchored subtree
  (`Path::render_from`); `NO_XDEV` must pin the starting mount and cover descent
  into a mount, an upward `..`, an absolute symlink target and a magic-link jump;
  `NO_SYMLINKS` must cover the final component and magic links.
- An anchored lookup must refuse to deliver an outside object (`EXDEV`) when a
  concurrent rename changes an intermediate directory's topology; it must not
  trust later parent traversal just because `path == root` held earlier.
- `RESOLVE_CACHED` must return `EAGAIN` before a slow lookup or a side effect such
  as create or truncate.
- 路径中间目录在 lookup 下一组件前必须通过 `MAY_EXEC`。
- 尾随 `/` 必须在最终组件 lookup 前强制 `FOLLOW_FINAL | DIRECTORY`；最终
  `LOOKUP_DIRECTORY` 必须由通用 namei 用 `Dentry::can_lookup()` 消费，不能由 open 层用
  接受 autodir 的 `d_is_dir()` 代替。
- 最终 open 必须按 access mode 检查目标 inode，不能只检查路径是否存在。
- namespace mutation 必须检查相关父目录 `MAY_WRITE | MAY_EXEC`；unlink、rmdir 和 rename
  的 sticky 与 mountpoint policy 必须针对父目录锁内最终 lookup 得到的 victim 执行，不能
  复用锁外预查对象。
- 创建 inode 的初始 UID/GID 必须由 `inode_init_owner()` 基于 `fsuid/fsgid` 和父目录
  setgid 状态导出，不能使用固定 root owner。
- `mkdir` 必须先清除用户 mode 中的 set-user-ID/set-group-ID 位；只有 setgid 父目录可由
  `inode_init_owner()` 给新子目录重新添加 set-group-ID。
- `Path::chown` authorization includes Linux `chown_common()` set-ID cleanup:
  non-directory set-UID is always removed and set-GID is removed when group
  execute is set, or when the caller is not in the pre-chown GID and lacks
  effective `CAP_FSETID`. When cleanup clears a bit and therefore generates a
  mode update, set-GID is also removed unless the caller belongs to the new GID
  or has effective `CAP_FSETID`; a GID-only change that clears no set-ID bit does
  not trigger that extra check. A generated mode change requires inode
  ownership or effective `CAP_FOWNER`; `CAP_CHOWN`/`CAP_FSETID` alone do not
  authorize it. Directories preserve their set-ID bits, and a `chown(-1, -1)`
  that changes no mode bit remains an authorized no-op.
- metadata 更新必须先通过 `Path::chown/chmod/set_times` 授权；后端 `setattr` callback
  只执行已授权的 mutation，不能作为 syscall 层的公开绕过入口。默认实现是公共回退层，
  只接受 owner/mode/时间并原样发布，size 一律拒绝：未安装专属表的 inode 不得通过回退
  路径伪造 `i_size` 或获得任何超出 `Path` 授权的变更能力。回退层只适用于 identity 与
  metadata 均为自身持有的 inode；跨文件共享 identity 的 inode（anon_inodefs singleton）
  必须显式覆写 `setattr` 保留拒绝语义，防止一次授权更新被所有持有者观察到。
- timestamp 授权必须区分 touch 与显式 times 数组；单个 `UTIME_NOW` 与
  `UTIME_OMIT` 的组合仍属于显式请求。
- `TimestampLimits` 构造时必须拒绝零值、超过一秒的粒度和反向范围；通用 superblock
  构造路径使用整秒保守默认，文件系统必须显式提升能力。已授权的显式时间由
  `VfsInode::truncate_timestamp()` 准备，自动 I/O 与 namespace 时间由 `current_time()` 在比较
  前准备；filesystem callback 必须通过返回的 `MetadataUpdate` 报告实际生效值并省略未应用字段，
  resident inode 只发布该返回值，raw setter 不再重复应用 policy 或恢复原始精度。
- descriptor truncate 必须验证 `FMode::WRITE`；pathname truncate 必须使用调用时凭据
  检查 inode write permission。
- dentry backing metadata refresh 必须先确认 positive state，再匹配 inode number、node
  type、block size 和 `rdev`；外部 filesystem bridge 不直接取得内部 `Arc<VfsInode>`。
- split truncate 必须在 inode data lock 与 address-space invalidate exclusive lock 下恰好
  调用一次 `truncate_setsize()`；该入口先发布 `i_size`，并在 cache truncate 前后各执行
  一次 mmap invalidation。有独立 backing 的文件系统必须覆盖默认 `set_len()`，并负责维护
  backing prepare 后失败的磁盘恢复协议。
- VFS-wide identity table 按 `(SuperBlock, inode number)` 联合索引，是唯一的 hashed resident inode
  identity table；`SuperBlock`、filesystem 和 bridge 不得再保存第二个 hashed cache 字段。Linux 风格的
  pseudo/unhashed inode 可以不进入表，但同一 filesystem identity 不得混用直接构造和 hashed initializer。
  缺失 entry 必须先发布
  `New` 再运行 fallible initializer；局部 reservation guard 必须在错误或 panic unwind 时删除
  未发布的 `New` 并唤醒等待者；新 inode 必须在发布 `Live` 前绑定所属 superblock；`New` 和
  `Freeing` 只能等待，不能返回普通 inode。key 自身持有 `Weak<SuperBlock>` 分配身份并按指针
  比较；这不保活 superblock 值，却保证 hashed slot 删除前地址不能复用。禁止用可复用裸整数地址
  或额外的 superblock serial 建立平行身份。
- 使用 `InodeAttributeOperations` 的 filesystem 必须让 operations 描述待构造的同号、同类型
  inode；KVFS 只保留该共享后端，不能同时分配另一份 generic attribute storage。
- `VfsInode` final drop 必须在 filesystem eviction hook 前把精确匹配的 cache entry 从 `Live`
  转为 `Freeing`，hook 返回后只删除该对象的 entry 并唤醒等待者；后端不得另建 resident cache。
- dentry 和 inode 的 superblock identity 首次绑定后不可改变；即使旧 `Weak<SuperBlock>` 已无法
  upgrade，也不得把同一对象绑定到另一个 superblock。合法 pathname callback 由 `Path/Mount`
  保活 superblock；绕过该 owner 调用返回 `ESTALE`。
- `VfsInode` 只拥有一个 `AddressSpace`，MM/filemap 只经 `VfsFile::mapping()` 获取它；
  address-space operation table 必须是所有 inode 共享的静态表，callback 只使用传入 mapping
  的唯一 host inode/superblock，不得把 filesystem inode 同时作为 aops object，也不得保存平行
  owner 回指。通用 block size 和 max file size 只由 `SuperBlock` 保存。
  `MappingPages` 由 AddressSpace 直接持有，不能作为可独立 clone 的缓存 owner 暴露给
  filemap 或文件系统。entries 与派生 dirty marks 由同一 mutex 维护，Folio 的引用
  继续独立保活页数据；该调整不改变 invalidate lock 或 mapping identity 校验。
  Filesystem-owned `CacheResidency` is installed once during inode construction.
  Admission runs under folio and mapping index locks; final physical-page
  release returns capacity outside both locks and must not sleep, including
  final provider destruction. Callbacks must not reenter a
  mapping, lock folios, perform I/O, or invert the folio-to-index-to-provider
  lock order. Detachment alone never releases a live allocation's charge.
- The operation table of a `SuperBlock` is separate from the filesystem-private state, corresponding to Linux `s_op` and
  `s_fs_info`. The construction API accepts only a static shared operation table; a filesystem callback must recover the private state through a typed borrow bound to
  `&SuperBlock`, and must not obtain an escapable private owner, store the mount owner in the
  operation object, or create a second filesystem identity.
- The active `PipeObject` of a pathname FIFO must be held by a typed slot of the `VfsInode`; the shared
  `FifoFileOperations` must not hold session state.
- The pipe access direction can come only from `VfsFile::mode`, the HUP generation snapshot can come only from
  `VfsFile::pipe_generation`, and the private data holds only the `PipeObject`.
- A non-blocking reader with no writer must not report HUP until the writer generation changes; a poll waiter
  must register on the queue according to the file mode, and the user event mask must not remove the wakeup source of HUP/ERR.
- `PIPE_BUF` atomicity must be decided on the complete user write request, and must not turn
  a large write into an atomic write because of the iterator's internal staging chunks; after a partial write, the uncommitted source iterator progress must be rolled back.
- A writable control file consumes only the current write buffer; a write command must not infer the persistent content length from `i_size`,
  and the size of a regular file may be published only after the backing store has accepted the write.
- The automatic atime/mtime/ctime updates of a pathname FIFO must go through the VFS mount/time policy and the
  filesystem `update_time` callback; an anonymous pipe must not update pathname inode metadata.
- The target of an immutable simple symlink is held only by the `VfsInode` cached-link state;
  `SimpleFsNode::i_size` must equal the byte length of the target, and a copy must not also be kept by a closure or an extra field.

## Dynamic directory metadata callback

`SimpleDirOps::link_count()` is a trusted filesystem callback, executed in a sleepable task context,
and invoked without holding the `SimpleFsNode` metadata lock. The implementation is responsible for its backend synchronization and for lifetime errors; errors
are propagated to `getattr()`. The interface's return value covers only the nlink of this metadata copy, and does not modify the inode identity,
permissions, owner, or the node's stored metadata. The return value is a single observation and must not be used as a basis for member synchronization or authorization.

## Thread Safety

The character-device adapter holds an `Arc` to the selected backend and takes no lock outside the open/release or poll callbacks.
Replacing the backend in open must complete before the file is published; on failure the backend rolls back, and the release of a successful file is not called.
Only the final operation table determines the capabilities and release callback of an opened file; the multiplexer does not additionally release the replaced table.

The interrupt-source marker of a file wait does not imply zero completion, and does not authorize replaying the entire transfer. The caller must retain
the completed amount of the object and the iterator; an ordinary operation error or a registration failure must not be disguised as a signal interruption. The wait
future owns the subscription and cleans it up on cancellation; the operation closure must not carry the object lock into the wait phase.

Shared dentry, inode, mount and file state is protected by mutexes, atomics, `Arc` and `Weak`.
The VFS inode-cache mutex protects only the `New/Live/Freeing` entries; the initializer, the eviction hook and waiting never hold
that mutex. Each slot has its own wait queue; both the drop of an unpublished reservation and normal state transitions wake only that slot after releasing the mutex,
and waiters also validate the slot generation, which avoids missed wakeups, cross-inode thundering herds, or treating a `Live` entry that has entered
final drop, where the Weak upgrade fails, as a cache miss and rebuilding it in parallel.
Mutable per-mount flags are stored in `VfsMount::mnt_flags` and are wrapped by `flags()` and `set_flags()` as the
strongly typed `MountFlags`, using relaxed ordering because the flags neither publish nor protect other mount state.
remount/reconfigure and make-private validate, before the operation, that the target is the root path of a mount registered in the current mount namespace.
make-private relies on the global design invariant that no shared propagation group currently exists,
so it never silently treats a truly shared mount as private. An ordinary remount receives the superblock
flags and the per-mount flags independently, and synchronously updates the read-only policy of the shared
superblock; `MS_REMOUNT|MS_BIND` updates only the per-mount flags of the target mount.
The superblock flags are wrapped by `AtomicSuperBlockFlags` as the strongly typed `SuperBlockFlags`; an ordinary remount
constructs an `FsContext` whose purpose is reconfigure, calls the static
`FsContextOperations::reconfigure` before publishing the proposed flags, and serializes the hook, the publication and the final shutdown with each superblock's
umount lock. The last active reference is first
decided under the lifecycle lock, then re-validated after the umount lock is taken and switched to dying; the shutdown callback does not hold the lifecycle
lock, and the dying/dead state rejects new mount activations. The context is bound to a unique target superblock, and the callback
recovers the owner only from the context; the VFS verifies the identity before the call. The context separately owns the parsed `fs_private`
and the proposed `s_fs_info`, and carries `sb_flags_mask`, so a failed transaction does not pollute the existing `s_fs_info`.
The default hook accepts pure VFS-flags changes; a block-backed superblock
is validated by the VFS in two phases — pre-hook against the caller's
proposed flags (gating hook execution) and post-hook against the
data-adjusted final flags (gating publication, covering data-level
`ro`/`rw` adjustments).
Explicit filesystem
sync and the global Weak registry sync snapshot
are also serialized with the final shutdown through the same umount lock, and skip dying/dead superblocks.
`get_tree_bdev` rejects a
writable mount of a canonical read-only block device before calling the filesystem fill-super, so that a read-only constraint of the medium is not deferred into a later filesystem I/O error. Before fill it establishes a nascent reservation keyed by the canonical
filesystem-type pointer and the canonical block-device object and holds an exclusive
claim; concurrent requests with the same identity wait for the same fill, and on failure or final shutdown the registry identity is removed and the claim
released before waking them to retry. Different filesystem types cannot own the same device at the same time.
mount attach/detach modifies the topology under the mountpoint inode namespace lock. Each child mount
protects its complete parent `Path` with a single mutex, the parent's child map stores `Weak<Mount>`, and the namespace
registry stores the `Arc<Mount>` of visible mounts; detach clears the parent path before removing the registry reference.
attach, bind, remount and detach all first use the existing registry to validate that the mounts involved belong to the current
`MntNamespace`, and make the registry mutex cover validation, topology modification and registry commit, which avoids a
membership TOCTOU. Recursive detach also collects the overmount stack of each child mountpoint,
and validates the complete set before any topology modification; the commit phase performs no more fallible validation. The registry mutex
is explicitly released before anything that may trigger the release of the last mount reference. The dentry children map and the superblock dcache
are not locked in a nested fashion; namespace operations first
update the parent's weak index and then update the dcache's strong ownership. Typed flags are immutable value snapshots and provide no shared
mutable state.

A regular inode's data lock covers generic buffered writes and the entire
set-length callback. Invalidate shared protection covers `page_mkwrite` and the
writepages call; exclusive protection covers truncate, mapped-view invalidation,
and cache truncation. The target folio lock serializes fault preparation, dirtying,
and PTE publication. Filesystem mapping/reservation locks are acquired within these
boundaries. Writeback callbacks must not upgrade to invalidate exclusive protection.

All namespace locks are sleepable locks. Mount tree operations first take the mount-namespace registry
mutex; the global order after that is superblock topology, parent directory, child directory, non-directory inode, and only last
dentry cache/location. Cross-directory ordering is decided by parent dentry identity: parent locks are taken ancestor-first, and unrelated parents are ordered by `Dentry::namespace_lock_key()` (ancestor inode pointer chain, then pointer value).
Directory
inodes rely on the single-alias invariant; the inode locks corresponding to non-directory aliases are deduplicated by identity and sorted by pointer value.
The mount topology lock is always acquired after the related inode namespace locks.

The default dentry operation table is published when the nascent superblock is initialized. Root/child dentries inherit only
a static reference; concurrent paths neither allocate nor replace the operation object.

The FIFO session lifecycle uses the inode pipe-slot lock to serialize the get/create/`files` transition with
release/clear, and the pipe state lock may be acquired only after the slot lock. reader/writer close first releases the
pipe state lock and then enters the slot lifecycle path, which avoids reverse nesting. The same inode therefore cannot publish
two active pipe sessions at the same time.

Pipe duplication must serialize destination reception with source consumption; it must not consume first and then discard a failed suffix. A two-pipe
operation takes the locks in object-address order, and both waiting and notification happen after unlocking. The single-pipe duplication callback runs under a sleepable
mutex and permits only non-pipe backends; it must not re-enter any pipe. The pipe lock precedes the non-pipe file-position lock, and
entering pipe duplication while holding a file-position lock is forbidden, so that waiting for space does not block other users of that file position.
The number of bytes reported as successful by the callback must not exceed the length of the passed slice; if it does, EIO is returned, the source pipe is not consumed and no destination
pipe data is published, and the count is not truncated to pretend success. Side effects that the backend itself has already performed cannot be rolled back.
The transfer position locks for file-to-file transfers are ordered by open description address, and the same object is acquired only once;
the staged read position must not be committed by the readahead length. The transfer entry point updates only the kernel-side offset, and the write-back to the user pointer is completed by the
syscall after the position lock is released; a write-back failure cannot revoke data that has already been accepted.
Copying between files validates the returned read and write counts before slicing, accumulation and position commit: a read must not exceed this round's request,
and a write must not exceed the amount read in this round. An excess is handled as EIO and this round's staged position is not committed; the legitimately completed amount from before
is still returned first, and data side effects that the backend has already performed are not promised to be rolled back.

The filesystem-type registry is protected by its own mutex. Boot registers serially, lookup returns a static descriptor copied by value,
and enumeration returns a snapshot; the mount factory is not called under the registry lock, so that filesystem initialization is not brought into the global critical section.

A Credential itself is an immutable `Arc` snapshot. The task credential lock is not held during a permission check, and the current task is not
re-read, so a concurrent `commit_creds()` can affect only the next operation.

A backend may sleep while holding a VFS lock. Re-entering a VFS namespace
operation on the same namespace object is not supported and may cause a deadlock. A layered filesystem may acquire a
lower filesystem lock only after an upper VFS lock; before such nesting is generalized, an explicit cross-filesystem lock rank is also needed.

## Threat Analysis

Aligned direct passthrough for block device files uses only the capacity-trimmed buffer sub-range, and the general block layer still performs the
range, partition-offset and read-only checks. The buffer is only borrowed for the synchronous call and must not be returned until the driver completes; no raw pointers,
driver-entry bypasses or copies are added. A failed multi-block write does not guarantee that the medium was left unmodified; when the backend provides no partial-completion length,
KVFS returns the mapped error, does not fabricate a successful byte count, and does not retry regions that may already have been written.

| ID | Threat Description | Impact Level | Trigger Condition | Mitigation |
|------|----------|----------|----------|----------|
| T-01 | 未知 flags 改变控制流 | 中 | 用户传入未支持位 | 边界使用 `from_bits` 并返回 `EINVAL` |
| T-02 | rename 模式冲突 | 中 | `EXCHANGE` 与 `NOREPLACE/WHITEOUT` 组合 | syscall 与 VFS 入口双重校验 |
| T-03 | flags 家族误传 | 中 | 内部 API 使用裸整数 | 独立 bitflags 类型形成编译期隔离 |
| T-04 | dentry 驱逐遗漏导致目录状态或资源生命周期错误 | 中 | namespace 更新只修改弱索引或只修改 dcache，或动态文件系统带外更新后未失效缓存 | insert/remove/forget 路径成对更新两层缓存；`invalidate_cached_child` 为动态文件系统在 namespace exclusive lock 下驱逐 positive/negative child，并以行为测试覆盖 |
| T-05 | dentry operation 与所属 superblock 不一致 | 高 | 具体文件系统在 dentry 创建后逐个补装或替换 operation object | nascent superblock 安装静态默认 `s_d_op` 对应物，root/child 在绑定时继承且不可替换 |
| T-06 | 路径只检查最终 inode，绕过不可搜索目录或错误接受不可 lookup 的最终对象 | 高 | namei 未检查中间目录 execute/search，或把尾随 `/`、`LOOKUP_DIRECTORY` 留给 open 层处理 | 每次 lookup 下一组件前调用 `Path::permission(MAY_EXEC, cred)`；通用 `path_lookupat()` 在最终 walk 前转换尾随 `/`，并用 `Dentry::can_lookup()` 消费目录约束 |
| T-07 | owner class 缺位后错误退回 group/other | 高 | DAC 把三类权限当作可任选集合 | `generic_permission` 按 owner、group、other 互斥顺序只选择一类 |
| T-08 | namespace 修改绕过父目录权限 | 高 | 后端 callback 被直接调用或 VFS wrapper 漏检 | `Path` mutation API 在最终对象仍受 namespace lock 保护时执行父目录 `MAY_WRITE | MAY_EXEC` |
| T-09 | sticky 目录删除其它用户文件 | 高 | 锁外检查的名称在 callback 前被替换 | unlink/rmdir/rename 的 validator 对锁内最终 victim 执行 sticky owner 和 mountpoint policy |
| T-10 | 创建对象固定为 root 或错误组 | 高 | 后端自行填写 UID/GID | 创建 callback 接收 `&Cred` 并使用 `inode_init_owner()` |
| T-11 | 一次 namei 混用 credential | 高 | 每个组件反向调用 current helper | syscall 捕获一次 `Arc<Cred>`，VFS 显式传递引用 |
| T-12 | `ftruncate` 因当前 pathname DAC 被错误拒绝或绕过写模式 | 中 | fd 操作复用 pathname truncate | `VfsFile::truncate` 先验证 `FMode::WRITE`，再走 opened truncate |
| T-13 | 非 owner 直接修改 inode owner、mode 或显式时间 | 高 | syscall 或调用者直接进入后端 `setattr` | `Path` metadata API 在 mount write check 后统一执行 Linux owner/group/write policy |
| T-14 | pathname socket 或其它 special inode 固定为 root | 高 | simple filesystem 动态 mknod 绕过 owner helper | `SimpleDir` mknod 使用 `inode_init_owner()`，仅支持持久插入的目录实现开放创建 |
| T-25 | 一个 mount 卸载破坏同一 superblock 的其它 mount | 高 | bind root 与源路径共享 dentry，却按 mount 创建来源选择性清理 dcache | mount 不保存 bind 来源标记；每个 `VfsMount` 持有 active 引用，只有计数归零才执行一次 superblock shutdown |
| T-15 | core mutation result 污染错误的 live inode identity | 高 | bridge 把一个 inode 的 metadata 写入另一个 `VfsInode` | cached metadata refresh 校验 identity、node type、block geometry 和 `rdev` |
| T-16 | truncate 窗口内 private mmap 在新 EOF 后重新 fault，或 shrink-regrow 暴露旧缓存数据 | 高 | i_size 晚于 cache truncate 发布、backing prepare 提前覆盖旧 i_size，或只执行一次 unmap | inode data lock 串行化 write/truncate；backing prepare 不修改 i_size；`truncate_setsize()` 执行 i_size publish -> unmap -> cache truncate/EOF zero -> unmap |
| T-17 | unlink 时过早触发磁盘 inode 回收 | 高 | dentry removal 与最后 open-file 引用混为一谈 | `Arc` inode identity 延迟 final teardown，磁盘回收只在 superblock `evict_inode()` hook 中执行 |
| T-18 | 并发 rename 创建目录环 | 高 | cross-directory rename 未序列化 topology 或未在锁内检查祖先关系 | topology mutex、稳定 parent lock 和 ancestry check 拒绝该操作 |
| T-19 | create、link 或 symlink 与 lookup、删除或 replacement 竞争 | 高 | final lookup、对象校验和 mutation 分离持锁 | final lookup、validator、participant lock 和 callback 位于同一父目录 exclusive transaction；create-like callback 复用最终 negative dentry，create-only 错误仅在该对象仍为 negative 时返回 |
| T-26 | 设备节点授权在锁外检查导致错误优先级错误或绕过 | 高 | syscall 先检查特权，或其它 `Path::mknod` 调用者不检查 | `Filename::mknod_at` 与 `Path::mknod` 都在锁内 negative-dentry callback 中进入唯一的 `Path::vfs_mknod`；现有目标先返回 `AlreadyExists` |
| T-27 | open、mkdir 与 mknodat 产生不同 callback mode | 高 | 各入口各自准备 mode/type，或直接调用 inode callback | open、mkdir、regular mknod 和 special mknod 都通过对应的 `Path::vfs_*` 能力执行同一套 DAC、setgid、umask、allowed-permission 与 node-type 策略 |
| T-28 | unlink/rmdir 删除 pathname 初次解析后的同名替代对象 | 高 | syscall 先解析完整目标，再按 parent/name 执行第二次 lookup | `Filename::unlink_at/rmdir_at` 只解析 parent；Dentry 在 parent exclusive lock 下唯一一次解析并操作最终 victim |
| T-29 | pathname FIFO 在授权后重新 lookup 并打开替代 inode | 高 | syscall 根据第一次 open 错误执行第二次路径解析 | FIFO file operations 在 namei 已授权的同一个 `Path` 和 `VfsFileBuilder` 上完成 open，不存在 fallback lookup |
| T-30 | pathname FIFO fd 丢失原 inode 身份 | 中 | special open 创建 anonymous-inode-backed file | 共享 FIFO operations 只修改原 builder 的 stream/private state，保留原 path、inode 和 filesystem |
| T-31 | 同一 FIFO inode 同时发布两个 pipe session | 高 | 最后 release 与新 open 分别修改 slot 和 file count | get/create/increment 与 decrement/clear 都在 inode pipe-slot lock 域内完成 |
| T-32 | 从未连接 writer 的非阻塞 reader 立即得到 HUP | 中 | poll 仅检查 `writers == 0` | file 保存 open 时的 `w_counter` snapshot，HUP 同时检查 generation 已变化 |
| T-33 | 只等待 HUP/ERR 的 poll 永不被 peer close 唤醒 | 中 | 按用户 event mask 选择 wait queue | 按 `f_mode` 注册 reader/writer wait queue，event mask 只过滤 readiness |
| T-20 | 反向 cross-directory rename 死锁 | 高 | 两个线程按相反顺序锁父目录 | 一个 topology mutex 串行化 topology mutation，父目录按拓扑顺序加锁 |
| T-21 | dentry name 和 parent 不一致 | 中 | parent/name 分开更新或读者观察中间状态 | 两个字段在同一个 location write lock 下替换 |
| T-22 | 目录 alias 形成两套 child cache 或绕过 topology 序列化 | 高 | 同一目录 inode 建立多个 live dentry | inode alias 表强制目录单 alias；lookup 复用已有 alias，rename topology 按 parent dentry identity 判定 |
| T-23 | filesystem callback 替换 VFS 已检查的创建对象 | 高 | callback 返回另一个 parent/name 下的 dentry | create-like callback 只能实例化事务 negative dentry；lookup alternate result 校验 positive state、parent 和 name |
| T-24 | 并发 slow lookup 建立重复 dentry | 高 | cache miss 检查与 candidate 发布分离，或等待者提前观察未完成对象 | candidate 在 callback 前原子加入 dcache；owner 保持 parallel-lookup 状态和 lookup mutex，等待者只在 lookup done 后读取结果 |
| T-29 | detached mount 仍可沿旧 parent 返回原 namespace | 高 | 卸载只删除 parent child map，却保留 mount 中的 parent/mountpoint 状态 | parent 和 mountpoint 合并为一个受 mutex 保护的 `Option<Path>`；detach 在 mountpoint namespace lock 下移除 topology 并清空该 location |
| T-30 | mount 支持集合与 `/proc/filesystems` 漂移 | 中 | syscall 和 procfs 各自维护文件系统名称或构造分支 | 两者都读取同一个 `FileSystemType` 注册表；重复名称注册失败 |
| T-31 | immutable symlink target 出现两份可分离状态 | 中 | simple-file closure 和 inode cache 分别保存目标 | 创建时直接构造 cached-link inode；目标只存于 inode，node 只保存长度等元数据 |
| T-34 | 最后一个 mount 释放时丢失 dirty page、inode metadata 或 journal checkpoint | 高 | topology detach 后 superblock 没有 active 生命周期，Weak registry 也无法保活 | `VfsMount` 获取/释放 superblock active 引用；最后一个引用在 umount lock 下执行 writeback、dcache eviction 和最终 filesystem/device sync；错误记录后仍完成 teardown |
| T-35 | 递归卸载 overmount 子树留下 registry 中不可达 mount | 高 | 只遍历 visible children，或边遍历边修改 topology | 收集每个 child mountpoint 的完整 covers 链，先验证全部 registry membership，再执行无失败分支的 topology commit |
| T-36 | 文件系统输出畸形 extent 导致范围溢出或 ABI 层错误解释 | 中 | 后端返回零长度或 logical/physical 末端越界 | `FiemapExtentInfo::fill_next_extent()` 集中检查非零长度和两个范围的 `checked_add`；ABI writer 只接收校验后的字段 |
| T-37 | Unprivileged caller forges a `security.*` xattr | High | VFS delegates authorization to an unimplemented LSM hook, and the backend accepts that namespace directly | KVFS requires effective `CAP_SYS_ADMIN` to set/remove `security.*` until concrete LSM hooks are connected; reads retain the delegation semantics, and the residual risk is the missing concrete LSM read policy |
| T-38 | `listxattr` 为不可见名称或属性值建立中间副本，造成不必要的内存放大 | 中 | 后端先返回拥有 value 的属性向量，KVFS 再过滤 `trusted.*` | `InodeOperations::list_xattrs` 通过 borrowed name sink 输出；`Path` 在流中先过滤 `trusted.*`，调用者只接收可见名称 |
| T-39 | immutable 或 append-only inode 的 xattr 仍可被修改 | 中 | namespace 特例在通用 inode 状态检查前直接授权，或文件系统未把磁盘 inode flags 映射到 KVFS | `check_xattr_permission()` 在所有 namespace 分支前检查 `NodeFlags::IMMUTABLE/APPEND_ONLY`，set/remove 统一返回 `EPERM`；具体 bridge 必须在建立 VFS inode identity 时映射后端 flags |
| T-40 | inode 初始化或驱逐竞争产生第二个 resident identity，或全 cache 唤醒形成惊群 | 高 | cache miss 在构造后才占 slot、Weak upgrade 失败立即重建，或所有 inode 共用一个等待队列 | cache 先发布 `New`；并发 initializer 在该 slot 等待；最后引用 drop 发布 `Freeing` 后再调用 hook；同号 lookup 等待 entry 删除并重试；每 slot 队列只唤醒同号等待者，后端不接收 `EINVAL` 风格的竞争错误 |
| T-41 | 同一 block device 出现两个 mutable superblock，或 mount 到正在 teardown 的实例 | 高 | filesystem 每次 mount 自行分配 state，或 `get_tree` 与 active acquisition 间无状态校验 | KVFS 按 canonical `(s_type, BlockDevice)` 建立 nascent reservation 和独占 claim；同 identity 等待/复用，RO/RW 不一致或跨类型冲突返回 `EBUSY`；`mount_new` 在 dying/dead activation race 后重试 |
| T-42 | filesystem 在 transaction 完成后继续使用失效的 mount data borrow | 高 | filesystem 把 `FsContext::data()` byte-slice 引用直接存入 superblock state | opaque data 只作为同步 context 输入；filesystem 必须解析并保存 owned/typed state，generic fill closure 为 `FnOnce` |
| T-43 | resident inode 暴露磁盘格式无法保存的时间精度，或字段联动关系与磁盘不一致 | 中 | VFS 在 filesystem callback 后重新发布原请求，掩盖后端的范围钳制、字段专有粒度或忽略行为 | `TimestampLimits` 只处理 superblock 级公共能力；callback 复用 `MetadataUpdate` 返回实际生效字段，KVFS 只发布返回值；FAT 日期级 atime、2 秒 mtime/ctime 联动及 KVFS callback 回传测试约束该合同 |
| T-44 | command-style pseudo file 把多次 write 按 offset 合并，或把一次 write_iter 拆成多次命令 | 高 | 默认标量写入算法处理 command 文件 | `CommandFile` 自有 `write_iter` 完整复制最多 4096 字节的一次请求，忽略 offset，并把 `VfsFile::f_cred` 与完整缓冲区一次性交给 callback |
| T-45 | 动态目录后端按名称重新解析 VFS 已锁定的 victim | 高 | `rmdir` callback 只收到字符串，replacement 竞争使后端删除不同 identity | `SimpleDirOps::rmdir` 接收 `LockedDentry`；后端可校验 `inode_ref()`，`mkdir` 同时接收 parent inode、prepared mode 和 credential |

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure Mode | Failure Cause | Local Effect | System Effect | Severity | Mitigation |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | Invalid open arguments | Unknown bits or illegal combination | open fails | No state change | 3 | Return `InvalidInput` |
| F-02 | Invalid rename arguments | Mode conflict or unsupported by the helper | rename fails | The namespace remains unchanged | 2 | Validate before the operation |
| F-03 | Filesystem callback failure | Backend I/O or metadata error | The current operation fails | May degrade to an I/O error | 2 | Propagate through `VfsResult` |
| F-04 | Hashed dentry not reclaimed in time | There is no Linux shrinker/LRU at the current stage | dcache usage grows | Long-running systems may see increased memory pressure | 3 | Explicit eviction on unlink, rename, and forget; a global reclaim policy will be integrated later |
| F-05 | dentry does not inherit the default operations | The superblock construction or binding path fails to pass them on | Dynamic name/lookup policy is missing | Incorrect behavior of the specific pseudo filesystem | 3 | Superblock bind inherits them uniformly; specific filesystem tests cover root/per-file dentry behavior |
| F-06 | Permission check failure | The mode, owner, or group does not allow the request | The current VFS operation returns `PermissionDenied` | The namespace and inode state remain unchanged | 3 | Complete the generic checks before backend mutation and propagate the error |
| F-07 | Backend cannot store the Unix owner | The on-disk format has no UID/GID | getattr cannot fully reflect the creating identity | DAC semantics are limited by filesystem capabilities | 3 | Document the backend limitation explicitly; backends that support owners must persist the helper result |
| F-08 | Cache invalidation fails after backing prepare in split truncate | Allocation or mapping invalidation error | The current truncate returns failure | The backing inode may remain in an orphan/recovery state | 2 | Converged by the specific filesystem's persistent recovery protocol; silently executing finish is forbidden |
| F-09 | Duplicate filesystem type registration | Duplicate startup wiring or name conflict | Registration returns `ResourceBusy` | boot stops before userspace starts | 2 | boot registers each built-in type exactly once and treats failure as an initialization error |
| F-10 | FIEMAP writer capacity exhausted | The user output capacity is smaller than the number of mappings | Returns the extents already filled in, and traversal stops | The caller can retry with a larger capacity | 4 | `fill_next_extent()` expresses capacity with a boolean return value and does not treat normal truncation as a backend error |
| F-11 | The filesystem rejects mount data or an option | Binary/text representation mismatch, syntax error, or the type has not yet implemented the corresponding semantics | mount fails before superblock/topology publication | The user gets a clear error and does not mistakenly believe the option took effect | 3 | The generic VFS is only responsible for safe handoff; the filesystem returns an error for unknown and unimplemented options, and only explicitly modeled compatibility no-ops are acceptable |

Validation failures return a typed VFS error before the filesystem callback. When the backend fails, the dentry cache
location remains unchanged, because the cache commit is performed only after the callback succeeds; the key and cache
slot required by rename already exist before the callback, and the commit only swaps the location and replaces the slot in place, without performing
allocation that could fail. Rollback and logging of persistent operations remain the backend's responsibility.

File locks use the lock order inode context → blocked owner index; sleeping and waking both take place after
releasing these two lock levels. UnsafeCell and intrusive nodes exist only in the private `locks/node.rs`;
modification of wait relations and the global guard are not exposed to the algorithm layer. Installed ranges are owned exclusively by `LockList`,
and a borrowed list can only obtain value copies; once detached, it is modified exclusively by a non-cloneable `OwnedLock` and then handed back to the list.
The shared `Blocker` can neither read the range nor obtain the intrusive node; a `Waiter` cannot be converted into an install owner.
This makes range safety independent of arbitrary same-type inode guard arguments. The public `FileLock` contains no shared mutable state.
A detached installed lock checks for an empty list through get_mut only when the Arc exclusivity check succeeds, avoiding taking the global lock when there is no waiting;
the parent relation uses Weak; the inode algorithm still holds its mutex to complete the update, and deadlock tracking does not read installed ranges.
Signal cancellation unlinks directly and releases the successor; pending notification requests and registrations are handed off from the original node, and a retry is allowed only after dequeue,
waker invocation and destruction both take place outside the lock, and close cleanup needs no notification allocation. Close cleanup precedes owner identity invalidation.
Locks constrain only cooperating callers; file reads and writes without an advisory lock are not blocked by them.

## Known Limitations

- The pipe, FIFO, socket, and generic character device operation tables explicitly reject sync and return `EINVAL`.
  `O_PATH` sync returns `EBADF` before calling the backend, and backend sync errors are propagated unchanged.
  The generic default sync is still a compatibility successful no-op; a successful return from backends such as FAT and 9p does not constitute a persistence guarantee,
  which must be filled in by subsequent backend sync implementations and persistence verification.

- Effective process capabilities now cover file permissions, sticky directories,
  xattr, chown/chmod, and related checks; LSM, POSIX ACL, user-namespace ID
  mapping, idmapped mounts, and file capabilities are not supported. Ambient
  capabilities are not a separate filesystem input; they affect a check only
  after becoming effective. `trusted.*` access and `security.*` mutation require
  effective `CAP_SYS_ADMIN`; user xattr modification in a sticky directory
  requires directory ownership or effective `CAP_FOWNER`.
- FAT 等后端不能完整表达 Unix UID/GID owner。
- 当前 POSIX rename 路径不支持 `RENAME_WHITEOUT`。inode lookup 和 getattr 的类型已
  建立，但尚未定义额外语义位；当前调用使用 empty flags。
- superblock dentry cache 尚未实现 Linux 风格的 LRU/shrinker，当前依赖 unlink、rename
  等 namespace 删除路径和最后 active mount shutdown 主动驱逐。
- KVFS 提供 AddressSpace view invalidation 通知，但各文件系统仍需用 live mmap/truncate
  case 验证自身接线。
- fast lookup 仍是 mutex-based，没有 RCU 或 rename sequence validation；同名 slow lookup
  通过 hashed candidate 的 owner/waiter 协议合并。
- layered filesystem 的 lock ordering 尚未建模。
- mount topology synchronization 与 superblock rename mutex 是不同机制，但均遵循已记录的
  inode-then-topology 嵌套顺序。
- 文件系统类型当前不支持运行时卸载；注册项及其函数入口必须具有整个内核生命周期。
- `FsContext` 能承载 binary mountdata bytes，但不负责具体 representation 的解析或
  filesystem-specific remount parsing。

### Mount propagation limits

- Mount propagation covers only `MS_PRIVATE`/`MS_SLAVE` (there are no shared propagation groups, so the two behave equivalently) and non-recursive
  bind; `MS_SHARED`/`MS_UNBINDABLE` and copying of recursively bind-mounted subtrees still return `EINVAL`,
  and the mount tree is not silently flattened. A recursive bind is accepted only when the source subtree has no mounts, in which case the result is the same as for a non-recursive one.

### Validity of buffered read window hints

A batch reference only keeps the allocation alive and does not prove residency or initialization; under the page lock, invalidated and non-uptodate candidates are rejected,
falling back to ordinary lookup and initialization. The copy is still limited by the current EOF under the page lock, and a short copy stops immediately.

The synchronous readahead control object is borrowed only within the callback covered by the invalidate shared lock; complete data is published
sequentially through a cursor, insufficient length does not produce a valid cache page, and existing residents are not overwritten by backing data.
A page-fault read likewise checks EOF and completes publication under this lock, and truncate exclusive excludes late stale data.
The generic buffered user copy does not hold the invalidate lock and rechecks EOF after locking each page; `prepared_end` only suppresses repeated
readahead and does not authorize skipping the cache lookup. A readahead failure can be retried by an on-demand read; when an on-demand read or copy fails, the
completed prefix is returned preferentially, and an error is reported only when there is no prefix.

## Audit Checklist

- Whether new syscall flags are converted to their types at the ABI boundary.
- Whether internal code uses `contains`/`intersects` instead of re-parsing integers bitwise.
- Whether `.bits()` is used only for ABI output, atomic storage, or explicit low-level interfaces.
- Whether new flag combinations come with conflict validation and behavior tests.
- Whether dentry cache insert, rename, unlink, and forget keep the parent weak index and the superblock
  strong ownership in sync.
- Whether mount attach/detach validates current namespace membership, holds the mountpoint inode
  namespace lock, and clears the parent location after detach.
- Whether a hashed child can still take part in the directory non-empty check when there is no external `Dentry` reference.
- Whether the default dentry operations of a new filesystem are installed on the superblock, rather than allocated and patched in per dentry?
- Whether new pathname entry points explicitly accept and pass down the same `&Cred` at every level.
- Whether search permission on intermediate directories, permission on the final inode, and parent directory mutation permission are each checked at the correct stage.
- Whether a trailing `/` forces following of the final symlink, and whether every `LOOKUP_DIRECTORY` uniformly requires
  `Dentry::can_lookup()` rather than merely checking for a directory-like type.
- Whether newly created inodes use `inode_init_owner()`, and whether the backend persists UID/GID.
- Whether fd-based operations use the open file mode/`f_cred`, rather than re-running pathname authorization.
- Whether a FIFO session is owned only by the inode pipe slot, and whether fops remain shared and stateless.
- Whether pipe private data stores only `PipeObject`, and whether direction and the HUP snapshot come only from
  `f_mode` and `pipe_generation` respectively.
- Whether FIFO poll registration selects the wait queue by file mode, rather than by the user event mask.
- Whether inodes of anonymous objects (pipes, etc.) use `metadata_only_inode_operations`, so that
  `fchown`/`fchmod` owner/mode updates are published on the KVFS-resident inode instead of being rejected with `EOPNOTSUPP`.
- Whether pivot_root completes all fallible validation before any mutation (new_root is a registered mount root,
  put_old is reachable from new_root and not inside the current root filesystem, `put_old` is a directory),
  and, once all inode namespace locks (totally ordered by `namespace_lock_key`, deduplicated by Arc) are held,
  rechecks that the old root is still attached to its own parent mountpoint before changing the topology.
- Whether pathname FIFO time updates are performed through VFS policy only after a successful external I/O.
- Whether xattr raw flags are first converted to `XattrSetFlags`, whether set/remove check for a read-only mount and hold the
  inode data lock, whether `trusted.*` hides names and existence from unprivileged list/get, whether `security.*`
  mutation rejects unprivileged credentials, whether immutable/append-only rejects
  all mutations before the namespace branch, and whether the list backend borrows names only for the duration of the sink callback?
- Whether, after a backing mutation, the shared attribute backend has been updated in place; whether a
  filesystem that uses KVFS-owned attributes refreshes all affected live inodes, rather than only returning a new dentry.
- Whether truncate keeps the order of backing prepare, i_size publish,
  two rounds of AddressSpace invalidation, cache truncate, and backing finish through `set_len()` and `truncate_setsize()`.
- Whether the file-backed MM runtime stores only `VfsFile`, and accesses the unique
  inode `AddressSpace` through `VfsFile::mapping()`.
- Whether `release()` and the final `evict_inode()` remain two distinct lifecycle stages.
- Whether the filesystem constructs private state through the fallible inode initializer of the owning `SuperBlock`, and
  has no backend inode-number resident cache?
- Whether `New/Freeing` waits and retries inside KVFS, and whether eviction finish deletes only the exactly matching old entry?
- Whether each `VfsMount` acquires and releases the superblock active reference exactly once, whether a non-final mount avoids
  teardown, and whether the last reference performs shutdown only once.
- Whether block-backed fill initializes only a nascent superblock already reserved by KVFS; whether both failure and final
  shutdown release the claim, delete the identity, and wake waiters; whether an RO/RW mismatch returns `EBUSY`.
- Whether recursive detach includes the child overmount stack, and completes full registry validation before modifying the topology.
- Whether every namespace mutation acquires the parent directory exclusive lock.
- Whether a new filesystem registers only one canonical static descriptor, and whether boot, mount lookup, and
  `/proc/filesystems` add no parallel provider, object, or name branch.
- Whether filesystem mount data is parsed during context initialization according to that type's representation, whether unknown or unimplemented
  options fail explicitly, and whether mount state retains no borrow of `FsContext::data()`?
- Whether an immutable symlink target is stored only in the inode cached-link state, and whether the metadata size is kept in sync.
- Whether unlink/rmdir and rename replacement lock the victim inode.
- Whether cross-directory rename acquires the topology mutex first, then the inode lock.
- Whether directory locks are acquired before non-directory locks.
- Whether callbacks avoid re-entering the same set of VFS namespace objects.
- Whether cache commit is performed only after backend success.
- Whether the FIEMAP backend calls only `fill_next_extent()`, and stops immediately after it returns `false`.
- Whether the FIEMAP backend completes the requested `SYNC` writeback inside the inode data read lock held by the VFS, before starting the traversal.
- Whether a shared-file write fault completes `FileMappingOperations::page_mkwrite` prepare, folio dirty, and writable PTE publish inside the address-space invalidate shared lock and the folio lock, without taking the inode
  exclusive data lock.
- Whether the dynamic directory nlink callback runs after the node metadata lock is released, and overrides only the nlink
  of the returned copy; whether the default `None` and the callback error propagation semantics are preserved.

## Snapshot resource bounds

Snapshot generation may allocate and fail during open. The caller must bound
generated data; each read-capable open retains its own buffer until close.
Sequential iterators can preserve a typed filesystem error through
`SeqIterator::try_show`; the default implementation still maps `fmt::Error` to
`Io`. A rendering failure clears the partially generated record and resets the
iterator before returning. It must not expose a failed record on a later read.
Per-open sequential state remains serialized by its existing mutex.

Private snapshot operations own initialization and access to the private data;
no snapshot representation or construction API escapes the crate. Missing
state makes reads and `SEEK_END` return `InvalidData` rather than regenerating
content. `SEEK_SET` and `SEEK_CUR` need no snapshot and do not allocate.
Read offsets are checked against the buffer length before conversion and
slicing. Seek bases use checked signed conversion and addition; invalid seeks
do not change the file position. `SEEK_END` reads the descriptor's own buffer
length, so another open cannot change its EOF by updating inode metadata.

The helper imposes no aggregate memory or generation-work quota. Independent
readable opens each retain their own allocation, so a per-buffer bound does not
bound total memory across processes. Filesystem integrations must account for
this cost in access and resource policy.

### Per-open file readahead policy

`FileRaState` is a policy hint; it grants neither cache residency nor EOF authority. The policy lock is not held across I/O or user copies;
when a marker is hit, the lock order is folio → policy → index (bounded history lookups only); the policy holder never waits on a folio.
The budget check, marker consumption, and window decision are completed within the same policy critical section; only after the page lock and the policy lock are released
is invalidate shared acquired and EOF resampled, and then the subsequent window is published.
Concurrent pread can change the history of the same open file description, but cannot bypass initialization, identity revalidation, or truncate protection.
Independent opens do not share policy state; dup/fork references to the same open file description share state.

## Pinned file-page mappings

- `with_pinned_file_page` holds the invalidate read lock through EOF validation
  and publication, and rejects page starts at/beyond EOF or overflowing indices
  before materialization or admission. A callback error keeps an initialized
  in-range cache page charged for reuse. Its callback must not wait for this mapping's exclusive
  invalidation lock.
- Read-only publication consumes `PinnedFolio` and returns a must-use
  `MappedFolio` token. The mapping owner retains that token until its PTE is
  removed and the corresponding TLB invalidation completes. A deferred
  invalidation submission and cache detachment do not establish completion.
- The stable `Folio` owns its page identity, allocation and content lock.
  Ordinary access uses `FolioGuard`; initialization consumes `FolioRead`, which
  retains the allocation and lock until completion; replacing
  an index cannot replace a published owner or its lock.
- Copies deliberately remain available after detachment and are serialized
  with writers of the original object. They initialize all destination bytes,
  but are neither current-EOF validation nor an immutable snapshot.
- Pins and mapping tokens retain the allocation and its single capacity charge
  through ordinary shared ownership, without a separate pin statistics system.

## Cache residency lifecycle

The filesystem owns capacity policy and supplies one `CacheResidency` provider
when constructing an inode mapping. Only a winning new identity calls
`try_add_folio()` under folio and index locks. Its stable folio retains one
capacity charge until the physical allocation is freed. Cache detachment and
mapping destruction leave charges intact while pins, tokens or batches remain.
The last reference frees the allocation and returns capacity outside both locks;
its charge also keeps the accounting provider alive until this release.
Providers must not reenter mappings, acquire folio locks, or perform I/O.
Release and final provider destruction must not sleep: the last reference can
be released by I/O completion. Admission still requires task context.
For memfs, the provider is its filesystem-owned quota, so lock order is cache
folio -> index -> quota limit; remount and statfs never acquire cache locks.
Speculative physical storage remains outside the admission policy until insertion.


Sparse-hole zero filling applies only to absent cache identities. A retained
non-uptodate reservation retries initialization under the invalidate read lock;
failed reads do not expose partial contents or silently replace them with zeros.
An error after a copied prefix returns that prefix, while a first-page error is
propagated.

Filesystem sparse reads must not consume remaining quota by prefetching absent
pages. Truncate invalidates retained read-batch entries before removing cache
identity; references cannot serve stale bytes, and keep their physical page
charged until its final release.

Filesystem-driven resident traversal uses one invalidate read guard per bounded
31-page snapshot. Page callbacks may only consume data, cannot reenter the
mapping, and must check EOF while guarded. Missing snapshot slots never allocate
storage. Partial data before a later callback/initialization error remains visible
to the filesystem caller, which decides its normal short-read completion.

Read submission may return before initialization completes. The filesystem must
retain its `FolioRead` and every backing resource until all I/O releases the
page, then complete or drop the token. A waiting caller can hold inode data or
invalidate locks; completion must not acquire those locks or the mapping index.
Only completed whole-page initialization becomes uptodate. Truncate waits on
the transferred folio lock before modifying/removing the page; unconsumed
readahead reservations are still removed before unlocking.

## Unrecoverable filesystem write failure

`SuperBlock::mark_read_only()` only performs an atomic set on the existing flags; it does not create a separate writable state,
nor does it overwrite other flags that are set at the same time. This interface does not guarantee persistence of dirty data or transaction recovery; the caller must first handle
the unrecoverable internal error, and publish the read-only flag before releasing the mutation protection. The VFS write entry points reject subsequent writes accordingly.
remount may modify ordinary mount flags; the filesystem must use its existing failure state to refuse re-enabling writes, and keep checking that state at the internal
write boundaries, so that protection during error recovery does not rely solely on the `RDONLY` bit.

The submit-style `submit_cached_ranges` neither waits for I/O automatically nor stores deferred errors. The filesystem must drain the submitted batches before
synchronous writepages returns, including on the scan/admission failure paths; it must not allow physical mappings that are not protected by some other mechanism to remain in use after the invalidate
protection ends. Consuming and dropping a PageCache completion
require task context, and may only be done after the corresponding I/O has finished.

### Asynchronous writeback errors and lifetime

A submission-based filesystem may retain completion after `None` returns, but
cannot rely on the caller's invalidate guard to protect physical mappings. Before
truncate/release, it must exclude new same-inode submissions and drain accepted
requests. In task context, real preparation/submission/completion errors enter
both mapping and superblock channels before completion ownership is released or
folio writeback ends. Ordinary queue throttling must not create an error.

Per-file fsync/syncfs cursors observe the channels independently. Sampling and
non-consuming checks preserve unseen failures for later opens; advancing one
cursor leaves older observers pending. Observing the mapping error does not
consume the filesystem error. A successful retry cannot erase either channel.
Temporary writeback checks use the current generation rather than the open-time
sample that inherits unseen errors. The boundary is taken before waiting for
submission or existing I/O, so failures recorded during those waits still count.
This neither marks errors seen nor adds a persistent observer. Repeated syncfs
calls report history through their own superblock cursor; they must not depend
on a file fsync consuming mapping history to stop reporting an old failure.
These cursors do not replace waiting or prove device durability. Every channel
operation may sleep and requires task context without spinlocks or interrupt/
preemption exclusion. Superblock sync attempts remaining inode data, inode
metadata, and filesystem flush after failure, preserving the first error and the
backend's abort checks. Shutdown must drain every inode's accepted I/O.

## Automatic write timestamps

Removing timestamp publication from buffered write or shared writable faults violates
the observable inode metadata contract. Deferred filesystem persistence must preserve
resident visibility and retain pending metadata until the corresponding sync or
eviction boundary; it must not acquire filesystem locks while holding a folio lock.
The automatic `update_time` callback owns resident publication and must finish
fallible checks before publishing values. Its default publishes only timestamps
confirmed by `setattr`; explicit setattr retains the separate returned-field contract.
Mount read-only and atime/relatime checks remain at the Path boundary.

The filesystem-owned buffered-write preparation callback replaces both extra
admission checks and automatic timestamp dispatch. Its caller must preserve
mount/journal write rejection and inode timestamp limits, and publish mtime/ctime
before exposing changed data. Preparation also checks policy for empty requests
and applies generic size limits in the filesystem's required order. Empty or
rejected requests must not publish timestamps. The callback may only reduce the
accepted byte count;
failure prevents page-cache copying and I/O-position advancement. Shared writable
faults retain the ordinary `file_update_time` path.

## Quota state boundary

The superblock's `quota_info()` guard serializes its single generic quota
owner and its canonical backing-file inode references. File selection comes
from loaded quota state rather than from an independent provider registry.
It may span blocking quota-file I/O only in task context without
spin/IRQ-disable/preemption-disable guards. Providers must use direct quota
I/O without recursive charging or caller-held lock reentry. Administrative
authorization, ID mapping, backing-file protection and transaction recovery
remain integration obligations. Ext4 enables internal user/group/project usage accounting
from its superblock quota inode fields.
Inode snapshots pin live identities and must be released outside locks needed
by eviction. Waiting for evictions must likewise happen outside those locks.

## Notification-interest lifetime

Static `DirMapping` entries retain their inode independently of dentry-cache
residency. Fresh lookups share its subscriptions and metadata; dynamic child
providers must supply their own stable object identities. Mapping teardown
releases these references without an inode-to-mapping ownership cycle.

The notification registry pins watched inode objects until after routing removal.
It publishes their aggregate event masks with release/acquire ordering and
serializes recomputation with subscription changes. Hints never grant access or
replace per-watch validation. Final inode references are released outside the
registry lock because eviction can sleep and enter filesystem code.

Dentry linkage, parent and name share the location lock. Successful unlink and
rename replacement mark only the removed dentry; other hard links and renamed
sources stay linked. A snapshot owns its parent reference and name, and releases
the location guard before any notification callback. Cache membership alone is
not evidence that an entry has been unlinked.

Block files retain their opened device claim, preventing `rdev` reuse from
redirecting I/O or release. Superblock open/close runs outside registry locks;
failed open skips fill, and failed fill/final shutdown closes once before
unregistering. Dead superblock references retain identity without a live holder.

## Bounded private-fault readahead

Private-fault readahead holds the invalidate lock for reading across EOF
selection and submission, so truncate cannot change the bound before stale pages
are inserted. The page-cache index lock is not held across backend I/O. The
16-page bound limits one call, not global cache residency; sparse access may
read adjacent pages and later concurrent inserts still win over prefetched data.
Only returned I/O/allocation errors permit demand fallback; allocator aborts are
not made recoverable.

## Final filesystem resource release

Final `put_super` executes after inode/dcache writeback under umount exclusion.
Callbacks may sleep and must release retained inode references outside their
filesystem locks. Live-inode visitation pins a snapshot and drops the registry
lock before callbacks. Reconfigure callbacks use their context's sync entry to
avoid recursively acquiring the superblock umount read/write lock.

Read-only remount holds exclusive umount protection against ordinary VFS
mutations and writable-open admission. Existing writable files and zero-link
live inodes cause `EBUSY` before filesystem state changes. The live snapshot
pins identities through policy publication, while existing cache transition
queues drain evictions that had already started. No new eviction takes umount
protection: final shutdown may itself drop the last inode reference while
holding that lock. The VFS cache binds and registers initialized identities
before publication; inode constructors must not rely on write-dependent
lifecycle initialization outside mutation or writable-open admission.

FIFO/pipe timestamp mutations share remount write admission even though their
file descriptions carry no inode writer count. The guard spans the read-only
check and timestamp callback; read-only policy prevents metadata mutation while
allowing the special file's already-completed data operation. Regular writable
files reuse their retained writer count instead of taking a nested read guard.

## Quota administration admission

Quota callbacks execute only while the superblock remains available and the
umount lock excludes shutdown/reconfiguration. Unsupported filesystems return
ENOSYS; unsupported quota classes return EINVAL before command authorization.
Lifecycle commands take exclusive admission, without introducing an independent
quota-control lock. Callbacks must retain mount-before-quota lock order and
release retained inode references outside filesystem mutation guards.

Mutable attribute-operation flags are read from resident filesystem state.
Immutable files reject ordinary content and metadata mutation, and namespace
removal/replacement checks victim policy. A buffered write rechecks protection
under file-data exclusion, covering descriptors opened before protection was
published. Quota metadata I/O is an explicit filesystem path and bypasses these
ordinary-file gates.

Live cache invalidation must not use final eviction: success requires completed
PTE removal and TLB invalidation before releasing any folio. Deferred-only or
missing notifiers cannot authorize cache destruction. A failed revocation or
writeback leaves cache ownership intact; earlier PTE revocation may remain.
The invalidation guard is released before retrying a busy MM. Callers must not
hold an MM lock or writeback-completion locks across this operation.

`AddressSpace::invalidate_range()` applies the same rule to a byte range: a
busy target MM reports `Ok(false)` and no folio is released, a covering view
without a notifier is rejected with `OperationNotSupported`, and only fully
revoked views allow clean-folio discard. A dirty folio reported as
`ResourceBusy` may follow clean folios that were already discarded, so a caller
must treat the range as not fully invalidated and must not assume the cache
still holds every folio.

Buffered writes publish or dirty cache folios only while holding the invalidate
read lock, so an invalidation that holds it exclusively cannot report completion
while a concurrent writer can still make the range dirty again. A registered
view never spans a partial tail page: that page is anonymous and must survive a
file invalidation, so rounding the view to the page boundary keeps file
invalidation from zapping user data in an executable's BSS tail. Executable text
mapped directly from the cache is registered like any other private file view, so
a file overwrite or truncate cannot leave those PTEs pointing at stale pages.

A writable descriptor alone does not authorize later shared mapping writes.
Write faults recheck the current inode policy under data exclusion. A fault's
superblock admission and data guard are acquired without waiting under the MM
lock; transient contention becomes a retry, while immutable policy rejects the
write. Existing writable PTEs are revoked before quota activation flushes and
repurposes ordinary file contents.

A nonblocking shared write-fault denial due to contention is not an invalid
user address. Its sleepable wait phase runs outside the MM mutex on existing
lifecycle/data lock queues. Acquiring and releasing those resources does not
replace current VMA, mount or inode policy checks on the subsequent fault.


### Destination faults during buffered reads

A destination shared mapping can refer to the source inode itself. Its write
fault can wait for inode data or superblock admission locks. Buffered reads
therefore use nofault copies under the source folio lock and run destination
fault preparation only after releasing it. This prevents a folio-to-inode lock
inversion with buffered writers. A retry revalidates residency and EOF; neither
the earlier page reference nor a successful prefault authorizes stale data or
unchecked access after a concurrent truncate, unmap or permission change.

An address-space view guard can extend its existing registration under MM
serialization. The registry mutex protects the VMA/object spans, preserving the
registration identity and invalidation notifier. Coordinates are validated before
publication; invalidation snapshots do not hold this mutex while entering MM.

## Data completion and persistence boundaries

Raw writepages return is not a persistence acknowledgement. Initial zero budget
and empty range are rejected as data work before filesystem admission; explicit
file synchronization still checks capability and runs filesystem barriers.
`write_and_wait_range` validates range and sleepability before observing errors
or cache indexes, captures a finite resident bound including old clean WRITEBACK,
and waits that same bound even after submission failure. Index/invalidate locks
are absent from the completion wait. New completion failures are recorded before
WRITEBACK ends and checked without consuming another open file's observer.

Missing filesystem fsync support is EINVAL, rather than unearned success. Volatile
backends explicitly succeed; block-device sync always issues its actual flush.
The file wrapper validates backwards ranges before backend dispatch or advancing
its cursor; an empty data range still dispatches and observes errors. Generic data
completion does not replace filesystem drain-before-block-release protection,
metadata settlement or device barriers. A captured target bounds work; it does
not certify writable-PTE dirty detection or eliminate the separate G requirement.

Live cache invalidation holds exclusive invalidate admission while submitting
and waiting, without reacquiring the shared entry guard. Submission success alone
cannot authorize cache destruction: completion errors are observed before any
dirty retry data is discarded. Submission failure keeps precedence while earlier
owned I/O is still drained and cache ownership remains intact.

## Buffered acceptance/writeback handoff

The copied folio remains locked through write_end acceptance and i_size
publication, with invalidate shared held. Write-end implementations cannot
reacquire that folio or wait for its I/O. Cancellation and rollback release
those guards before lifecycle waits, while retaining writer data serialization.
Writeback samples live EOF only after taking the same folio lock. Attempted
work and filesystem declines share one pagecache-owned control; completion
never retains it. This does not replace shared-PTE dirty rearming/invalidation.
