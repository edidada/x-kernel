# KExt4 — Security and Reliability Analysis

## VFS owner allocation failure

If the Ext4Mount owner cannot be allocated, fill_super returns NoMemory and drops
the prepared state before installing operations, private data or a root. KVFS
performs its existing failed-initialization transition and releases the reserved
identity/claim. Orphan cleanup starts only after the owner is installed, so
this allocation failure precedes orphan mutations. Prior journal replay is not
undone, and nested mount allocations keep their existing contracts. NOFS still permits reclaim/I/O and is not used as a proof
that canonical-device initialization can safely be re-entered.

## Writeback callback allocation boundary

Callback owner OOM returns NoMemory before queue sequence/slot publication and
before any data BIO submission. The finish callback is not moved into a fallible
constructor: failed reservation first records the error, then releases captures
in the submitting task. Worker initialization remains retryable; an already
prepared worker is reused. Prior unwritten extent allocation is not rolled back
by this owner change and retains the existing retry contract.

Tests inject failure into private worker preparation and finish reservation, not
the global allocator. They check unpublished state, retry, NOWAIT reservation,
error-before-capture-drop ordering, no submitted data writes and exactly-once
completion/capture destruction on retry. WorkHandle storage, Arc/Vec allocations,
BIO callbacks and BIO buffers keep their existing allocation contracts; this is
not end-to-end OOM safety. NOWAIT does not promise NMI safety or bounded latency.

## Shared snapshot allocation boundary

Direct vector-to-`Arc` conversion does not make metadata snapshots fallible:
vector and shared-slice allocations retain the existing fatal OOM behavior.
It introduces no `KERNEL` reclaim permission and does not relax the existing
lock or execution-context requirements. BIO buffers and writeback callback
owners are outside this conversion cleanup.

## Trust Model

Allocation requests borrow their actual ext4 inode. A physical goal selects the
starting group; without a goal, data allocation uses the inode's group. Metadata
allocation and block release also receive their owner, while group bitmap
primitives do not own inode accounting. A preallocation group twin must retain a
strong reference to the object borrowed by the request; an identity mismatch
fails before the first bitmap mutation.

Recovery requires exclusive access before root publication and retains the same
`Ext4SbInfo` and journal owner. Replay refreshes the disk mirror and derived policy
before mount capabilities are fixed and the private owner is installed. Orphan
cleanup must succeed before the root inode is loaded. Failure follows nascent
superblock teardown, releasing private state and the device claim without
publishing an accessible root directory.

Metadata reload validates the decoded superblock's features and frozen
geometry before changing allocator counters. Recovery refresh additionally
checks journal backing identity and hash policy before installing that same
image as the new mirror. It does not decode the superblock a second time or
consult the previous mirror to initialize the new policy. Ordinary checkpoint
reload preserves the existing mount mirror and static policy.

Protected ranges are reconstructed from filesystem geometry and the mounted
journal mapping, using the same checked insertion path at mount and recovery.
The index must remain ordered and non-overlapping for binary-search queries;
journal ownership permits only its inode to reference its blocks. A journal range
that overlaps filesystem metadata is rejected before the new index is installed.
Recovery reconstruction does not trust or copy the previous derived index. Changes to the
loaded journal's backing identity are rejected before its mapping can be assigned
to a different inode.

KExt4 treats the block device only as an I/O transport layer and does not assume that the metadata on it is valid. The
superblock, group descriptors, directory blocks, extent trees, xattr blocks, bitmaps and
journal records read from disk are all external input, and must pass range, format and checksum validation before they are interpreted or modified.

When the superblock feature bitmap enters the filesystem logic, it is wrapped into strongly typed
flags distinguished by compat category. Unknown bits are not discarded during decoding, and unsupported incompat bits still cause the mount to fail, so that type
encapsulation does not relax the validation of features from an untrusted disk.

KExt4 currently provides a complete mutation contract only for extent-backed block maps, so images lacking
`EXT4_FEATURE_INCOMPAT_EXTENTS` fail before the mount state is published. This check prevents a valid but unsupported
legacy indirect format from first entering a writable runtime state and then having deferred writeback turn writes that already succeeded in user space
into asynchronous `EOPNOTSUPP`, a journal shutdown error or partial persistence. Even if the superblock enables the
extents feature, `write_begin`/`page_mkwrite` on an individual legacy inode is rejected synchronously, before the delayed-allocation
reservation is made and before user data is marked dirty.

KExt4's KVFS operation implementations trust that KVFS has already provided kernel-owned path names, dentries, inode `AddressSpace`
(including directly held `MappingPages` storage) and file lifecycle callbacks. The KExt4 core does not directly dereference user-space pointers.
It also trusts that `fill_super` receives only a nascent superblock that KVFS has already reserved by canonical `(s_type, BlockDevice)` and holds an
exclusive claim on; KExt4 does not build a parallel device-to-mount cache.

## External Boundaries / Attack Surface

The relevant boundaries include:

- ext4 块设备镜像，包括恶意或损坏的元数据；
- JBD2 journal replay record 和 checksum 字段；
- 从磁盘读取的 directory name 和 dirent file type；
- extent tree entry 和 block mapping；
- KVFS FIEMAP 请求范围，以及 core 从磁盘 mapping 与 inode extent-status 区间统一报告的 extent；
- KVFS fallocate's mode, offset and len: mode bits are admitted against the
  backend's `supported_fallocate_modes()` declaration. The kvfs layer filters
  requests routed through `VfsFile::fallocate` (mode 0, UNSHARE_RANGE,
  COLLAPSE_RANGE, INSERT_RANGE; kext4 declares KEEP_SIZE, stray combinations are
  rejected, and UNSHARE_RANGE — needing reflink — is rejected like Linux ext4
  without shared blocks), and the posix entry consults the same mask before
  dispatching PUNCH_HOLE/ZERO_RANGE: a backend that declares the mode owns it, and
  kext4 does not, so those are emulated by zeroing the range without freeing
  blocks;
  append-only inodes allow only plain preallocation and immutable inodes reject
  everything (Linux `vfs_fallocate`'s EPERM semantics, judged in the posix entry
  before the node-type dispatch, the size-bound check and the backend mask); the
  posix layer rejects `offset < 0` and `len <= 0` (EINVAL), and the kext4 entry
  additionally validates `offset + len` overflow and `max_file_size()`;
  on ENOSPC the committed batches remain as unwritten preallocation beyond EOF
  (block occupancy observable via df/fiemap/st_blocks, surviving across close and
  resumable via retry); before returning the original error, `i_disksize` advances
  to the successfully allocated prefix (capped at the request end, never shrinking)
  and ctime/mtime are refreshed, matching Linux `ext4_alloc_file_blocks`; this
  failure state is defined, not undefined;
- xattr name、value、external xattr block header、refcount 和 checksum；
- KVFS 运行态操作，例如 create、unlink、rename、truncate、writeback、fsync 和 syncfs；
- KVFS 传入的 atime、mtime 和 ctime，以及磁盘 inode size/extra timestamp 字段决定的可表示范围；
- transaction commit、journal replay 或 checkpoint 期间的设备写入/flush 失败。

As checked, this crate uses `#![forbid(unsafe_code)]`, does not access MMIO/PIO directly, does not manage DMA
ownership, and does not use FFI, inline assembly or architecture-specific raw interfaces.

## Read Buffering and Failure Publication Boundary

Whole-block initialized file reads reuse the caller's `&mut [u8]`, without introducing unsafe, user-pointer dereferences or
new DMA ownership. The block trait accepts an arbitrary byte start for both direct reads and partial-block staging buffers; the backend and its DMA capability layer
are responsible for device address alignment, contiguity, mapping and cache maintenance. The filesystem does not substitute constants or the incidental allocation alignment of a `Vec<u8>`
for the device contract, nor does it push buffer requirements onto the `read_at` caller.
This convention does not remove block length/range validation, and it does not promise failure atomicity for device I/O.
The hole/unwritten branches must not read old physical data, and partial blocks and EOF must not copy
bytes outside the request to the caller.

块后端可能先修改目标前缀再返回错误，因此 `read_at` 不保证失败缓冲保持旧值，也不返回
可发布的成功前缀。调用者必须传播错误，不能置 uptodate 或把失败内容复制到读迭代器。
Cached pages move into bios through `FolioRead` and complete only after device
access ends. Contiguous pages belong to one bio. Fragmented subpage reads use
disjoint part owners: zeroed holes, unwritten extents and device-read runs
converge before whole-page publication. A failed or abandoned part keeps the
page stale; pending parts retain the lock even after another part fails. Each
part spans only adjacent bytes with the same zero-fill policy or contiguous
physical mapping; grouping cannot bridge a hole or a physical discontinuity.
Each part zeroes its own EOF tail before completion. Larger-than-page filesystem
blocks retain the synchronous partial-block reader.
Failed pages remain non-uptodate until a complete retry succeeds. PageCache
removes unconsumed reservations, while independently successful pages may become
readable. Failed buffer contents are not valid file data. Submission holds the
core read guard to stabilize block mappings; truncate waits for transferred page
locks before freeing blocks. Completion uses only the bio status, original pages,
and sampled EOF, without taking inode data, invalidate, or core locks. After
reading the disk block containing EOF, it zeroes every remaining byte in that page.
Writable ext4 integration tests retain both tail-page bios after demand returns,
start a VFS shrink, and verify that mappings and free-block accounting survive
until the last owner retires, including when another part has already failed.
The same tests verify that shrink/regrow cannot expose the discarded suffix.
`vfs/read_tests.rs` 以真实 ext4 + KVFS + PageCache 链路验证上述隔离，并检查重试后的页未被
标脏、17 字节 EOF 尾部补零。失败读后部分写入的测试还验证完整重新读页先于写入复制，
只把成功写入后的完整数据标脏，不发布此前失败读污染的前缀。该验证不声称设备 I/O 具备失败原子性。

## Unsafe Code Inventory

None. `fs/filesystems/kext4/src/lib.rs` forbids unsafe code for the entire crate. Unsafe or device-specific
operations are located outside the boundary of this crate.

## Memory Safety Invariants

- The on-disk offset and size must be validated before slicing metadata bytes.
- Metadata checksum validation must complete before a checksum-protected block is trusted.
- A metadata checksum failure of the primary superblock must be returned before mount feature negotiation;
  capability rejection may happen only after the checksum passes, and must run before the first open and before a recovery reload publishes the runtime state.
- Block numbers and inode numbers must be validated against the layout, group, bitmap and system-zone rules.
- A JBD2 handle must hold enough credits before it dirties or revokes a metadata buffer.
- A speculative prealloc space exists only in memory: it is not marked in the block bitmap, the group
  descriptor or the inode, so discarding it at any moment leaves no disk residue; it reaches the bitmap only
  when consumed, through the normal allocation path, and inside that same critical section both indexes are
  detached before the consumed span is marked, so no window separates exclusion from the disk mark.
  Conversely, an entry kept on one side alone protects nothing and must not be treated as available space -
  the inode-side logical range and the group-side twin entry must be added and removed together inside one
  allocator critical section, and `preallocated_blocks` must always equal the sum of the group-side span
  lengths (verified after every change in debug builds).
- group 侧孪生条目持有 owner inode 的强引用，所以 eviction 必须无条件丢弃该文件的 space；漏丢
  会让条目与其强引用一起活过 inode 对象。
- mount 生命周期内的 `MountedJournal` 是生产路径唯一 journal identity，同时拥有磁盘 mapping、
  活跃 superblock 状态、transaction engine 和 FIFO checkpoint queue。同一个 runtime
  transaction 必须按 Running → Committing → Checkpoint → Finished 迁移；队列只保存该对象，
  持久化证据属于其 Checkpoint phase，不复制 payload 或携带可替换的 coordinator 引用。
- 提交请求在事务锁下先关闭目标 running transaction 的新 handle 准入，再释放状态锁等待
  活跃 handle 退出；stop 和 abort 必须唤醒等待者。不能持有自身 handle 或退出方需要的锁
  等待提交，也不能嵌套 begin 等待自己所在事务解封。重复持久化请求必须识别 pending/finished
  目标，不能重复 append 或退休；无效未来目标必须失败，不能等待不相关事务。
- 可变事务的 ordered-data flush 只能确认 barrier 开始前快照的依赖代次；较旧 barrier
  完成不能倒退确认水位，失败不能清除依赖。冻结后不再接受新依赖，commit 在日志写入前
  满足其完整 ordered-data 依赖。
- journal 持久化 execution mutex 串行 append、checkpoint、ring/tail 与 recovery flag I/O；
  短 transaction/state/buffer guard 不得跨设备 I/O。home 和 tail 两次持久化成功前不退休，
  失败须在交出执行权前 abort。首次 feature 更新和 PreserveDuringRecovery 同样参加该协议；
  replay/reset 仍限于 mount 身份发布前的独占恢复。
- replay 完成后必须先把 transaction engine 的 next sequence、运行态 ring 和 checkpoint
  水位重置到 replay report，再开始 orphan cleanup；否则旧 sequence 不得用于创建新 transaction。
- internal-journal extent 必须连续且至少覆盖 JBD2 `s_maxlen` 声明的 logical block 数；
  inode-backed journal 的预分配容量可以大于 `s_maxlen`。每个 physical extent 都必须位于
  filesystem block count 内；磁盘映射校验完成前不得创建 `MountedJournal`。
- 多个 handle 可以独立退出并共享 running transaction；handle stop 必须归还未使用 credits。
  预期错误必须在该 handle 首次 metadata access 前完成校验并原样返回，不能改变其他 handle
  已发布的 bytes、dirty buffer ownership 或 transaction membership。单个 handle 不得在
  失败路径上删除已发布的 metadata/revoke membership，因为同一 block 可能已被其他 handle
  共享。任何普通错误若发生在 metadata 已发布之后，都按遗漏 preflight/unwind 的内部不变量
  失败处理并永久 abort journal；不得回滚同一 running transaction 中其他已经成功返回的
  operation，也不得把失败报告成可继续运行的局部回滚。尚未发布的私有状态由 ext4 算法显式
  unwind。
  普通 transaction 上界按 journal descriptor/commit/revoke 开销保守限制，不能用固定
  operation 数代替容量约束。显式 filesystem sync 必须先提交 running transaction，再推进
  committed checkpoint queue。
- Resident `Ext4InodeState` owns each regular inode's metadata target and records
  the handle's transaction identity at publication. Retain the transaction Arc
  and inspect its phase; long-lived tids must not alias across sequence wrap.
  Unattributed publication and extent mutation invalidate the target until later
  inode-table staging restores it. Unknown targets and non-regular inodes use
  conservative running-transaction commit. Committed targets still flush later
  data and check abort. Targets replace neither cache drain, full-sync timestamp
  staging, mount exclusion, nor independent KVFS error observation.
  Legacy orphan link staging advances the operation's resident inode target
  only after successful metadata publication and leaves dirty timestamps pending.
  A predecessor-only chain splice is recovery bookkeeping owned by the removal
  transaction, not a change to that predecessor's file attributes or mappings.
  It may retain the predecessor's old target: an uncommitted removal leaves the
  old chain and its allocated inode recoverable together. No temporary inode
  object or second resident-inode registry substitutes for the canonical owner.
- 释放 bitmap 位之前，分配器必须按块组和 freeing transaction ID 登记待复用区间；
  区间、统计与 bitmap/cache 发布归同一 allocator owner、受同一锁保护。JBD2 不保存物理
  空间记录；文件系统胶水仅在提交持久化成功后、释放 journal 锁后通知分配器解除该事务的
  保护。handle stop、freeze、缓存重建或失败都不能解除；无需等待 home checkpoint。
  连续区间按组有界处理，跨段失败必须走 mutation-error abort。分配同时排除 prealloc 与
  待复用区间；surplus 登记不得通过 bitmap-free 采样承诺不可复用块。提交失败保留记录并
  abort，不能向后续分配开放这些块。
- 回收阶段可共享事务，但每个因同步/额度压力提交的前缀必须保持 orphan 与剩余映射一致；
  每个回收阶段在 handle 创建成功后独立禁止 dying inode 的 extra-isize 自动扩展，不能
  依赖上一阶段标志，也不能在清理 xattr 后重新建立 EA block；准入失败不改变该标志。
  运行态回收仅裁剪最右侧叶并沿路径回收空节点，不分配或重建子树；操作前失效映射缓存。
  批次超过单事务容量时必须先有界缩批，容量判断共用 journal 契约；其他 NoSpace 或
  提交错误不能被吞掉并当成缩批重试。
- 有界物理空间准入只能在 mount mutation 排他下、打开 handle 前进行，不把 metadata
  credits 当成物理块需求。空间不足可先丢弃可选预分配，再提交可解禁的 freeing transaction；
  不得持有 active handle 等待自身提交，也不得重试已经部分发布 metadata 的 namespace 操作。
  writeback 转换以实际叶项变化与分裂布局计树块需求，零增长操作不能为固定预算推动提交。
  有待复用块时逐映射关闭 handle 再准入，停止 handle 本身不建立持久化边界。
  准入或 handle completion 隐式提交成功后，必须与显式 Restart 一样确认完成前缀、
  释放该范围预留。后续错误/abort 不得把已提交前缀重新算作未完成；失败的提交不得
  推进前缀。完成依据来自 journal 原有事务生命周期，并遵循事务 ID 回绕规则。
  现有 admission 失败不 abort、mutation 后失败 abort 并发布只读状态的边界不变。
- truncate 必须在释放旧 block mapping 前强制提交 orphan + `i_disksize` transaction；恢复期
  orphan cleanup 不得与普通 transaction 混合，并须在保持 recovery evidence 的策略下同步
  完成 commit/checkpoint。
- 多个 committed transaction 驻留磁盘日志时，sequence/start 必须保持指向最老 live
  transaction，追加只能从 mount 运行态 append head 开始并至少保留一个空 journal block；
  clean journal 首次提交必须先持久化非零 start 再发出 descriptor/data/commit，使激活与
  commit block 之间的掉电被恢复路径识别为未完成 active transaction；
  活跃期不得把运行态 head 持久化为 `s_head`，只有 clean journal 才写该字段；home-block flush 完成后
  才能把 tail 推进到下一个 transaction，队列非空时不得清除 ext4 recovery evidence。包含
  primary superblock 的 frozen checkpoint image 必须保留 recovery feature，不能让较老
  checkpoint 覆盖后续 live journal 的恢复标志。
- 缓存的 journal-superblock image 必须覆盖一个完整 journal block，且只能来自成功解码和
  checksum 校验的输入；每次内存更新必须重新解码生成下一份可信状态。journal write batch
  不得超过 128 KiB，不得跨越 ring wrap 或 internal-journal 不连续 physical extent。批量写入
  只改变请求粒度，不能合并 clean-journal activation flush 与 transaction 最终 durability
  flush，也不能省略没有 metadata commit 代为提供的同步 flush。
- extent 局部更新必须同时刷新被修改叶子、祖先索引、inode root 和 metadata checksum；
  ordered writeback 不得在同一 handle 内从路径更新切换到全树重写，credits 上界按本次写入
  规模与最大路径深度覆盖逐层分裂，且不得截断计算结果。
- ordered writeback 不得把全范围静态 mapping/space plan 当作执行真相。dirty range 的必需块
  已纳入 delayed-allocation reservation accounting；执行 cursor 在每个实际 mapping/allocation
  run 前重新查询 extent，并由 journal handle 自己约束 mounted credit limit。handle 扩展失败
  只能在完整 run 边界 flush 并提交 durable prefix。只有该 transaction 成功后才能释放对应
  delalloc range；因 metadata/fragmentation 发生的后续 ENOSPC 也必须按部分进度返回。未完成
  后缀及其 reservation 必须保留，optional preallocation budget 不能因 transaction restart 被
  重新授予，也不能占用 ext4 reserved blocks 或其他 inode 的 delalloc reservation。
  整体 writeback 在后续 transaction 失败时必须返回已完成字节前缀；PageCache 只结束其中完整
  folio 的 writeback，边界 folio 和后缀保持 dirty。普通可重试错误可以再次提交该 suffix；
  device/JBD2 错误会 abort 当前 mount journal，必须先 recovery/remount。
- range mutation 只有在完整操作可由单条 `ExtentPath` 表达时才能开始局部 metadata 写入；
  跨叶回退必须发生在首个 metadata write/create/revoke 之前。
- 不带 revoke feature 的 journal 只能在“旧 transaction 已全部 checkpoint”时无 revoke
  释放 metadata block；forget 同时必须从当前 handle 的 metadata 集合中移除该 block。Clean v2
  journal 必须先 flush revoke feature 才能允许 transaction/checkpoint 重叠；v1 journal 保持
  同步 checkpoint 退化路径。
- 内存中的 counter 和 descriptor 只应在同一事务内完成对应 metadata bytes staging 后更新。
- block allocation/release 必须在同一 metadata mutation 中同步 primary superblock 与 group
  descriptor 的 free-block counter；delayed-allocation admission 依赖 primary counter，并另行
  扣除 ext4 reserved blocks 和 core 持有的 delayed-allocation mount aggregate。
- `Ext4StatFsMode` 只能改变对外总块数的口径；free、available、reserved-block 和
  delayed-allocation aggregate 必须保持同一唯一事实源，不能按 `minixdf` 重算或放宽空间准入。
- 从磁盘解码的 external xattr name 必须拒绝内嵌 NUL，保持与 Linux/e2fsck 的 corruption
  handling 一致。
- 目录记录长度必须由 `RawDirectoryEntry` 的唯一编解码入口按 Linux 磁盘语义解释。HTree root
  的固定字段、磁盘 hash version 和树深必须经共享 root decoder 校验；磁盘版本 `3/4/5` 是
  非法元数据，版本 `6` 是合法 SIPHASH 格式，但缺少 fscrypt name key 的 mutation 必须在依据
  该 hash 选择或写入 HTree leaf 前返回 `Unsupported`。
- HTree signedness 必须由 mount state 统一拥有：默认 hash 在 mount 时校验，`DIR_INDEX` 未启用
  时不得解释 signedness，显式 unsigned 优先于 signed，两者都未指定时遵循当前 Linux 的
  unsigned-char 选择；策略解析不得写盘，可写 mount 只能在 root inode 初始化成功后、superblock
  发布前持久化该默认，read-only mount 不得写盘。Orlov placement 必须固定使用 signed HALF_MD4
  和 superblock seed，不能复用 HTree default/signedness policy。
- 新写 external xattr block 必须按 Linux ext4 算法生成每项 `e_hash` 和聚合 `h_hash`，并在其后
  计算 metadata checksum；不能用只保证 KExt4 自身可读、但会被 e2fsck 判坏的零 hash 编码。
- Xattr name-only 遍历仍必须验证 entry 末端、value range、external block header 和 checksum；
  sink 只能在单次回调期间借用 name bytes，不能保存该引用。
- External xattr block 的 `h_reserved` 按 Linux 语义作为 opaque 字段处理，不能仅因其非零拒绝
  e2fsck 修复后的合法块；启用 metadata checksum 时仍由完整 block checksum 检测意外修改。
- 相同 xattr value 的允许替换必须在 journal credit 准备和 metadata access 前短路，不能更新
  inode ctime，也不能产生无意义的 journal 写入。
- 普通 xattr set/remove 必须固定使用当前 `i_extra_isize`，只改变目标 entry 的存储区域；可选的
  extra-isize 扩展只由统一 ordinary inode-dirty 入口触发，xattr apply 和 zero-link cleanup 必须
  绕过该入口以防递归。
- 扩大 `i_extra_isize` 前必须先证明全部 xattr 可在缩小后的 inode body 与一个 external block
  中完整分区；迁移、`i_file_acl`/`i_blocks` 和 inode checksum 更新必须属于同一 transaction。
  运行时 effective `want_extra_isize` 必须按 Linux 规则包含默认 32 字节，并仅在
  `RO_COMPAT_EXTRA_ISIZE` 存在时合并磁盘 min/want；新 inode 必须在分配事务中直接写入该值。
  effective want 无法满足时只能退到磁盘 `min_extra_isize` 或保留当前值，不能丢弃属性或阻断
  本来合法的普通 inode metadata 更新。迁移选择必须是有界的 entry-by-entry 扫描，并禁止把
  `system.data` 移到 external block。内容未变的已有 external block 必须直接复用；需要新
  external block 的候选必须在规划阶段检查 allocator 可用性，并在 apply 前完成分配。规划与分配
  之间的空闲空间竞争返回 `NoSpace` 时记录 resident `no_expand` 并继续普通更新，不能扩大为假
  `ENOSPC`；删除 xattr 清除该状态，journal 的暂时性 credit/忙碌失败不应永久禁止重试。设备 I/O、
  checksum 和 corruption 错误仍必须传播并触发既有 journal 失败策略，不能以 best-effort 为由吞掉。
- 运行态 inode allocation 必须使用 kext4 callback 已通过 `inode_init_owner()` 导出的显式
  UID/GID；KExt4 core 不得把新 inode owner 默认为 root。
- Filesystem 级 timestamp range 必须按 inode size 判断完整 `i_[acm]time_extra` 容量；128-byte
  inode 只能声明秒级精度和有符号 32-bit 秒范围。Core encode/decode 仍须按 inode 实际 extra
  field 存在性截断纳秒、限制 epoch，并把解码后的值作为唯一 resident metadata 结果发布。
- FIEMAP 的 logical/physical/length 乘法和加法必须 checked；遍历必须同时受请求末端与
  inode 创建时缓存的格式上限约束，损坏 mapping 不得越界输出。
- Resident identity 只属于 KVFS `VfsInode` 和 VFS-wide `(SuperBlock, ino)` table；KExt4
  不得建立 inode-number cache、
  `Live/Evicting/Evicted` 状态或基于 core handle 引用数的 last-reference 规则。`RawInode` 只能
  作为磁盘解码值，不能承载 runtime identity 或 ext4-private transient state。
- `VfsInode` 必须直接组合一份 `Ext4Inode` private component，不能只保存 inode number 并按操作重新
  解码。unlink/rmdir/rename 必须把已锁定 VFS victim/moved/replaced 的 private component 传入 ext4 算法，
  防止 open handle 与 namespace mutation 各自更新不同对象。
- Legacy orphan chain 的 `i_dtime` 必须从 journaled inode-table bytes 读取并原位更新；不得为
  resident 前驱按编号构造第二份 private state，也不得从可能过期的 private snapshot 恢复 next。
- delayed-allocation 的区间、per-inode reserved count 和 mount aggregate 全归 KExt4；一次
  reserve/release/truncate/writeback/eviction API 必须在 core mutation guard 内联合更新它们，
  kext4 KVFS 层不得维护 set、逐块节点或另一个 aggregate。
- KVFS 在 final hook 前发布 `Freeing`，因此普通 VFS 能力不能与 eviction Phase A/B/C 并存。
  Core 三个 phase 都借用 VFS inode 组合持有的同一 private component，不返回可逃逸的 eviction
  handle；inode slot 释放后由 KVFS 删除旧 cache entry，number reuse 构造全新的 private state。

## Thread Safety

运行态 KVFS `SuperBlock` 把无状态 `Ext4SuperOperations` 与唯一 `Ext4Mount`（内部 `RwLock<Ext4SbInfo>`）
分别作为 `s_op`/`s_fs_info` 保存；`s_op` 是所有挂载共享的真正静态表，private object 由
superblock 独占且只能借用恢复，不存在承载 mount identity 的 operation wrapper 或可逃逸的
typed owner。所有 inode mapping 也共享静态 `Ext4AddressSpaceOperations`，callback 只从
`mapping->host` 借用唯一 `Ext4Inode`；共享写 fault 的准备由 `Ext4Inode` 的
`FileMappingOperations` 完成。仍需联合修改
superblock/group metadata 的 mutation 由挂载级 write guard 串行化；只读 core 操作共享进入，
delalloc range 与 mount aggregate 则在 shared guard 下由独立 reservation mutex 联合更新。KVFS
cache 合并并发 `iget` 并封闭 `New/Freeing`；ext4 private metadata 与 transient state 在 inode
state mutex 下更新，KExt4 不再取得另一把 inode-cache lock。内部 JBD2 和 metadata-buffer 状态仍会记录
transaction ownership。
Shared-buffer atomic mutations hold the slot mutex and preserve old bytes on
failure. Writers in the same transaction must not independently read old snapshots
and then replace whole blocks.
Inode-table publication merges only the target entry, preserving neighboring
inodes' current data; callers still serialize logical changes to the same inode.
Writeback, both VFS truncate phases, last-writer preallocation cleanup, and final
eviction share the inode submission mutex. Mapping mutation/release drains
in-flight requests after acquiring it; completion workers do not acquire it.
Acquire it before mount protection, never while holding mount or inode-state
guards. PageCache enumeration holds no core mutex, and batch writers enter the
core only after mapping/folio locks are released. Journal execution protection
follows mount protection; stopping a handle does not acquire it.
容量 preflight 与 begin 仍由同一次 mount mutation 排他保证，尚不是独立 credit reservation，
不得据此放开全部 metadata mutation。后续引入细粒度锁时，必须保持该跨层边界，以及 buffer
ownership、journal handle、allocator bitmap 和 inode metadata 之间的顺序约束。
FIEMAP 由 VFS inode shared lock 稳定单 inode mapping；core read lock 和 delayed-block
mutex 只分别短暂取得，并在安全 writer 访问用户页前释放。不得把用户内存访问放进挂载级
core lock 或 delayed-block mutex。Buffered write 和 shared write fault 都必须在 inode
exclusive data lock 下先把 hole reservation 发布到同一个 delayed set，不能以 dirty-folio
扫描替代 filesystem mapping prepare。
speculative prealloc space 的 per-inode map 锁是 allocator 锁之下的模块私有叶锁，刻意不使用
inode state 锁：消费与 discard 都在持 allocator 锁的临界区内取放它，`update` 闭包内不再取得
任何锁，group 侧 discard 锁 foreign owner 的 map 也按同一顺序取，因此 prealloc 操作不会与
inode-state 使用者构成 ABBA。ENOSPC 重试阀自带 allocator 锁，只能在扫描的锁段之外调用。

## Threat Analysis

| ID | Threat Description | Impact Level | Trigger Condition | Countermeasure |
|------|----------|----------|----------|----------|
| T-01 | An xattr entry name or value range in a corrupted image is treated as a valid attribute by the name-only list path | Medium | A malicious or corrupted image encodes NUL, an out-of-bounds/overlapping value, or an invalid external block | `decode_xattr_entries()` rejects NUL and malformed entries; name-only traversal still validates the value range, block header and checksum, and is covered by regression tests |
| T-02 | The external xattr block leaks after unlink or rename overwrite, or the inode keeps referencing an already-freed block | Medium | `i_file_acl != 0` on a zero-link victim | Zero-link eviction first frees or decrements the EA block refcount, clears `i_file_acl`, updates `i_blocks`, and only then frees the inode |
| T-03 | A runtime unlink releases too early an inode that is still referenced by an open fd | Medium | Unlinking a still-open inode on the KExt4 runtime backend | The namespace transaction persists only nlink/orphan; the open file holds the single VFS inode and its ext4 private state, and only the destruction of the last VFS reference enters superblock final eviction |
| T-04 | Journal credits are underestimated, or overestimated based on data blocks, so that a metadata update fails midway through a transaction or is wrongly rejected | Medium | Namespace remove, HTree conversion/split, writeback, truncate, preallocation discard or final eviction modifies orphan, xattr, extent and inode metadata | The namespace insertion precheck explicitly records the HTree path; the split immediately after conversion is estimated as two independent single-block extents, and HTree credits are added before the inode flag is published; namespace removal covers only dirent, nlink and orphan metadata and excludes the subsequent final eviction; ordered writeback is fixed to a path-local algorithm in which a live cursor renews the handle per actual unwritten conversion and hole allocation run, and when the transaction limit is reached it commits the complete prefix and restarts; legacy final eviction and extent truncate are computed from the actual tree blocks, the old tree blocks that need revoke, and the affected groups, and the release of directory/block-mapped symlink data blocks counts revoke credits one by one per freed block; when the victim carries an external xattr, the final eviction budget includes EA cleanup |
| T-05 | A malicious extent or bitmap metadata causes a block that does not belong to the inode to be freed | High | A corrupted image points an extent/xattr block at the system zone or an illegal group | Block ownership, system-zone, bitmap and checksum validation is performed before release or mutation |
| T-06 | A device write/flush failure makes partial metadata visible | Medium | Device failure during commit, replay, checkpoint, explicit sync or xattr update | Journal abort preserves the recovery state or pending checkpoint; subsequent sync/mutation returns aborted, and in-memory modifications are not rolled back across syscalls; tests cover explicit-sync commit, checkpoint, xattr and replay failure |
| T-07 | An orphan remains on a clean journal, so mount permanently returns `NeedsRecovery` | Medium | The namespace transaction has been checkpointed, but final inode eviction has not yet happened | Explicit recovery traverses the legacy list and the orphan file regardless of whether the journal needs replay; the clean branch establishes recovery evidence with `PreserveDuringRecovery`, synchronously commits/checkpoints one by one, and clears the recovery features only after confirming that the journal start is zeroed and the orphan file is empty; a zero-link entry reuses the journaled final-eviction path |
| T-08 | The free-block aggregate, delalloc ranges and reservation aggregate drift apart, causing over-reservation | Medium | The VFS and ext4 sides are updated separately, or the per-block set and the mount counter are updated on only one side | A block mutation publishes the superblock and the group descriptor together; the ext4 range API jointly updates the inode interval, the equivalent of the `i_reserved_data_blocks` counter and the mount aggregate under a separate reservation mutex, and admission/statfs use this aggregate directly |
| T-09 | A newly created inode is fixed to root, bypassing the caller's owner semantics | High | A callback drops the credential, or the ext4 constructor implicitly fills in UID/GID 0 | The create/mkdir/mknod/symlink callbacks use `inode_init_owner()`, and the explicit `uid` and `gid` are persisted together with the namei transaction |
| T-10 | The journal head catches up with the tail and overwrites a commit that has not yet been checkpointed | High | Multiple committed transactions fill the circular log, and appending still continues to write | Live space is computed from the oldest tail/current head, and one empty block is always kept; when space is insufficient, a contiguous prefix large enough to free space is checkpointed first and then the write is retried, and no overwriting write is issued; a real ext4 image test covers tail advancement across two transactions |
| T-11 | A corrupted extent or an oversized FIEMAP range causes arithmetic overflow, a wrong physical address or unbounded traversal | High | The disk mapping length exceeds the format capacity, or the byte/block conversion is unchecked | The ext4 algorithm exposes the Linux-equivalent inode-format-dependent maximum byte count; the KVFS operation performs checked validation on every addition, multiplication and output field, and truncates the mapping to the request and format boundaries |
| T-12 | The immutable/append-only state of an on-disk inode is lost in the VFS integration, so xattrs get modified | Medium | iget always uses empty `NodeFlags` when constructing the KVFS inode | ext4 exposes `EXT4_IMMUTABLE_FL/EXT4_APPEND_FL` through semantic methods; the kext4 KVFS layer maps them to KVFS flags when publishing the VFS inode identity, and the generic xattr permission layer returns `EPERM` before the mutation |
| T-13 | A snapshot fork appears for the same inode, or an inode number reuse inherits the old transient state | Medium | Concurrent `iget` during KVFS initialization/release, namei reloading a live child by number, or orphan removal decoding a temporary object for a resident predecessor | The `New/Live/Freeing` states of the KVFS-wide `(SuperBlock, ino)` table are the only identity state; `New/Freeing` wait and retry; kext4 passes the existing private component to unlink/rmdir/rename; the legacy orphan next pointer reads and writes only journaled inode-table bytes; KExt4 has no resident cache; reuse can happen only after `Freeing` completes and the old slot is deleted |
| T-14 | Re-growing after a shrink exposes stale PageCache data or reuses an invalidated extent proof | High | ext4 backing prepare publishes `i_size` early so that `truncate_setsize()` skips cleanup, or a single atomic publication is mistaken for a size transaction and the mapping-invalidation mutex is removed | Regular-file metadata publish updates only `i_disksize`; the VFS and ext4 share the single `i_size`, which the VFS publishes with a release store at the PageCache ordering point, and EOF reads use an acquire load; size writes hold the inode-state write lock, a shrink clears the proof before publishing the new EOF, and the snapshot and the extent proof are sampled under the same lock; the inode data/invalidation lock continues to constrain the PageCache/PTE ordering, and the atomic provides no invalidation mutual exclusion; the generic attribute sharing, concurrent shrink/proof, core prepare, and KVFS shrink/regrow and partial-grow regression tests together enforce this responsibility boundary |
| T-15 | Enlarging `i_extra_isize` overwrites or loses xattrs, generates an external EA block that e2fsck does not accept, or rejects an update that could have completed when the disk is full | High | An xattr syscall wrongly changes extra-isize at the same time, the ordinary inode dirty path misses the expansion, new extra fields overlap the old inline range, the external entry/block hash is left zero, a shared block whose content is unchanged is wrongly COWed, or free blocks from the layout phase are taken before apply | Ordinary set/remove always uses the current extra-isize, and the unified ordinary inode-dirty entry then expands according to want/min/current; migration uses Linux-style entry-by-entry selection with `system.data` fixed; an external block whose content is unchanged is reused directly, a layout that needs a new block is preallocated before apply and downgrades `NoSpace` to `no_expand`; the same journal transaction rewrites the external block, generates a Linux-compatible `e_hash`/`h_hash`, clears the old inline area and updates the inode checksum; unit tests cover deferred expansion, directory inode dirty, shared external reuse when the disk is full, migration selection, and independent fixed hash vectors |
| T-16 | A corrupted orphan entry makes mount recovery fail, loop, or access an unallocated/special inode | Medium | `s_last_orphan` or an orphan-file slot is out of bounds, points to an inode whose allocation bit is zero or to an internal backing inode, the loaded inode has a link type that cannot be truncated, or the legacy `i_dtime` points to an illegal next | The unified `orphan_iget` validates the number, the special-inode boundary and the checksummed bitmap before inode-table decode, and validates the format and cleanability after loading; a legacy bad chain is zeroed persistently in a `PreserveDuringRecovery` transaction, an illegal orphan-file entry is skipped and processing continues with the subsequent valid slots, and in the end the recovery evidence is preserved and the operation fails; bitmap I/O/checksum errors, inode decode errors, and legal but unsupported cleanup are not downgraded |
| T-17 | `minixdf`/`bsddf` changes the source of truth for free space or wrongly deducts metadata overhead | Medium | The two modes each implement the whole statfs formula, or the VFS integration modifies the free counters directly | The typed mode only selects `blocks_count - overhead` or the raw `blocks_count`; the free/available/inode statistics go through shared code, and a Linux-image regression test verifies that the two agree except for the total block count |
| T-18 | A 128-byte inode exposes nonexistent nanosecond precision, or a newly created inode loses its nanosecond/extended epoch and wraps around after 2038 | Medium | Truncation only at inode-table encode time, the VFS comparing raw high-precision times, or the initialization structure storing only the base `u32` seconds | KExt4 returns the shared `TimestampLimits` according to the inode size, held by the single KVFS `SuperBlock`; `VfsInode::current_time()` truncates only once, before comparison, callback and publication; a new inode stores the full `Ext4Timestamp` and writes it to disk through the base/extra encoder, the 128-byte format clamps at the boundary, and the 256-byte format preserves nanoseconds and the epoch; regression tests cover capability selection, initialization encoding and resident publication for both inode sizes |
| T-19 | A valid SIPHASH HTree is misreported as corrupted | Medium | A root `hash_version=6` enters the directory write path | The shared HTree root decoder accepts only the on-disk versions `0/1/2/6`, and HTree signedness comes from the mount state; SIPHASH returns `Unsupported` at the hash boundary that needs a key, and the related transaction cannot commit a metadata mutation |
| T-20 | An old HTree image with no signedness mark has invisible names after being mounted across Linux, or Orlov directory placement drifts | High | The computation treats the image as signed when both on-disk signedness flags are empty, or Orlov reuses the HTree default/unsigned policy | The mount state generates `hash_unsigned` according to Linux unsigned-char semantics, RW persists the unsigned flag, and RO leaves the disk unchanged; HTree hashes only through the mount policy, and Orlov independently uses a fixed signed HALF_MD4 + seed; a high-byte name regression test distinguishes signed/unsigned |
| T-21 | The two indexes of a speculative prealloc space drift apart: a leftover entry on one side protects no block yet is treated as an available span, or a group twin outlives its owner inode | Medium | The inode side and the group side change in different critical sections, or close / writeback settlement / eviction drops only one side | Both sides change only together inside one allocator critical section; `preallocated_blocks` is conserved against the sum of the group-side span lengths and checked by a debug assertion; each twin holds a strong owner reference; close (when no delalloc reservation remains), writeback settlement (when reservations reach zero and no writable open remains, with an empty reclaim skipped via the inode-side emptiness hint) and eviction (unconditional, never short-circuited on the inode-side hint so a drifted pair cannot leave a twin behind) all discard; regression tests cover the close, evict and shrink reclaim paths and dual-index consistency |
| T-22 | The ENOSPC retry valve rescans without bound in the state "aggregate is nonzero but no block can be selected", or uses retries to mask bitmap corruption | Medium | The free aggregate counts unconsumed space as free; a corrupted group is hit again by every rescan round | The search count is bounded by the fixed `PREALLOC_DISCARD_SEARCH_LIMIT` (3 attempts, since a discard can only occur after the first); a discard that returns 0 ends the search immediately; a retry is triggered only by the no-space conclusion of a healthy group, and geometry/descriptor precheck skips do not set it; the first `InvalidBlockBitmap` encountered during the scan is remembered and returned in preference to `NoSpace`, but does not close off the reclaim retry for healthy groups |

## Failure Mode and Effects Analysis (FMEA)

The shared eviction preflight for namespace removal validates the inode data
format before the first metadata mutation. The orphan file is validated and initialized after mount/replay; at runtime, `OrphanInfo`, under mount-level exclusive access,
maintains the slots and the free-entry count; the count is updated only after metadata publication succeeds, and on failure the journal abort path
preserves the on-disk recovery evidence. An ordinary inode with multiple hard links whose link count does not drop to zero is not given an orphan entry. Kernel component tests use synthetic
metadata to cover unlink, rmdir, overwriting rename, and out-of-order final eviction; image interoperability and e2fsck belong to the external harness.

| ID | Failure mode | Cause | Local effect | System effect | Severity | Mitigation |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | Unsupported is returned for a feature that is valid in Linux ext4 | Features such as EA inode, bigalloc, and inline-data write are not yet implemented | The current operation fails | The filesystem remains in an auditable state, but the capability is unavailable | 3 | Feature negotiation and an explicit `UnsupportedKind` |
| F-02 | Journal commit or checkpoint fails | Device write/flush error | The committing or pending checkpoint state is preserved and the journal is aborted | Subsequent sync/mutation on the current mount is refused, and recovery is relied on at remount | 2 | Abort the journal permanently before the failure is returned; preserve recovery evidence and pending state; forbid subsequent syncs from falsely succeeding |
| F-03 | Recovery-time orphan cleanup fails | The crash happened after the namespace commit but before final cleanup, a bad legacy chain must be cut, or orphan-file/bitmap/inode metadata is bad, the device fails, or an inode uses a format that is not yet supported | An explicit legacy bad orphan terminates after a durable clear; errors that cannot be safely downgraded preserve the orphan entry or recovery evidence and return | The filesystem is not exposed as a writable root without durable cleanup, and checksum/I/O errors are not discarded as stale orphans | 2 | Orphan cleanup uses an independent journal transaction; both bad legacy clear and ordinary inode cleanup use `PreserveDuringRecovery`; after a checkpoint, only the mutable superblock/group state is reloaded and the orphan-file free count is maintained incrementally, and only after success does processing continue; N3 completes the fault/powercut matrix on the final execution graph |
| F-04 | The coarse-grained write lock serializes slow mutation I/O | The live kext4 KVFS layer holds the mount-level core write guard around blocking filesystem mutations | Throughput drops | Other KExt4 mutations wait; read-only paths can still share the read guard | 4 | Inode private state already has an independent state lock; N2 continues to build per-group/journal/metadata-buffer lock domains based on the actual workers and shared state; mutation concurrency performance is not claimed before the locks are split |
| F-05 | Journal reservation has insufficient space | An operation's actual metadata targets exceed the capacity of an empty journal, or a reservation mixes in work from another lifecycle phase | A single mutation that cannot fit fails before modifying metadata; when a writeback transaction fails to extend, only the complete prefix is committed | Admission failure leaves the disk unchanged; if a later transaction of a segmented writeback fails again, the caller may receive an error while the durable prefix is preserved | 3 | Namespace and final eviction are budgeted separately; directory insertion checks disk capacity before the transaction, the credit is reserved at the structural upper bound for the current extent depth, and a single-block directory that may be converted is counted for a second append; before each live mapping run, ordered writeback guarantees headroom for that operation and the inode publish, first tries `reserve_more()`, and if capacity is insufficient it commits the already flushed prefix, settles its delalloc exactly, and reopens the transaction, rejecting at the admission stage only when even a single mutation cannot fit; a cross-leaf truncate chooses, before the first metadata write, the whole-tree path estimated from tree blocks/groups; constrained by regression tests for small-journal actual-run restart, fragmented unwritten, failure/retry, rename-overwrite, balanced-split, and preallocation-tail credit |
| F-06 | Lock order inversion between PageCache and the filesystem core | Writeback holds the core mutex while waiting for the mapping mutex, while a cache miss holds the mapping mutex and enters a backing read | Concurrent buffered I/O and `sync()` stop making progress | The watchdog reports a mutex deadlock, and the filesystem workload cannot continue | 2 | The kext4 KVFS layer does not hold the core mutex outside the PageCache traversal; writeback of the same inode is serialized independently, and the batch callback enters the core only after the mapping/folio mutex is released; VFS/MM must still eliminate backing I/O under the tree lock |
| F-07 | Committed journal fills the appendable space | Checkpoint lags behind commit, and the head approaches the oldest tail | New transactions temporarily cannot be made durable | Mutations wait for checkpoint progress | 3 | Before appending, validate the space against the circular live range and keep one empty block in reserve; the commit path catches `JournalBusy`, synchronously processes a prefix of pending checkpoints sufficient to free space, and retries |
| F-08 | A FIEMAP query encounters a corrupted mapping | The extent tree/legacy pointer returns an invalid length or physical range | The current ioctl returns a data error | File contents are not modified, and the caller cannot obtain the layout | 3 | Reuse ext4 mapping validation; the KVFS operation rejects zero progress and arithmetic overflow and does not output unchecked extents |
| F-09 | Eviction interleaves with a concurrent `iget` | Another path loads the same inode number while final teardown has already begun | A caller may observe metadata that is being released, or establish a parallel identity | Stale I/O, double free, or crosstalk of transient state | 2 | `Live -> Freeing` is completed under the KVFS cache mutex; lookup/insert wait internally, and finish removes exactly the old Weak entry, wakes waiters, and they retry, without returning `EINVAL`/`ESTALE` to path operations |
| F-10 | The extra-isize expansion of an existing inode fails repeatedly on every dirty write | The xattr layout or external block resources continue to be unsatisfiable, but the failure conclusion has no resident state | Every ordinary inode dirtying repeats metadata I/O, checksum, decoding, sorting, and layout planning | Metadata update throughput drops, and the disk-full scenario may be amplified | 4 | When the layout is determined to be infeasible, set `no_expand` in the unique resident `Ext4Inode` state; clear it after an xattr is successfully removed; journal busy/insufficient credits remain retryable |
| F-11 | Wrong selection of the statfs total-capacity mode | The context loses the explicit option, a reconfigure without options resets the current value, or the default mode becomes minixdf | `f_blocks` is inconsistent with the Linux ext4 ABI semantics | Capacity monitoring and application space decisions are wrong | 3 | A new mount defaults to `Bsd`, and the explicit mode is stored in the unique `Ext4SbInfo`; a reconfigure without options keeps the current value; the exact total block count of minixdf and the overhead result of bsddf are verified separately |
| F-12 | A speculative prealloc space is not reclaimed with its owner's lifecycle | Close was gated by a delalloc reservation and eviction did not cover the gap, or only one of the two indexes was changed | Blocks are counted in `preallocated_blocks` without an owner, and the group scan keeps excluding those bits | Shows up as "the free aggregate is nonzero but allocation fails"; the ENOSPC retry valve's discard bounds it, and a group twin holds a strong reference so a missed drop would keep a dead inode alive | 2 | Both sides change inside one allocator critical section; the conservation debug assertion; eviction discards unconditionally; close drops only when no reservation remains, and otherwise writeback settlement (reservations zero and no writable open) or eviction reclaims; a settlement racing the writer-count decrement keeps the spaces for the next settlement or eviction; retries are bounded and stop when discard returns 0; close/evict/shrink/settlement regression tests assert both indexes and the counter return to zero |

## Fault Management

KExt4 uses `Ext4Result` and typed `Ext4Error` to express corruption, unsupported format,
out-of-bounds metadata, checksum mismatch, journal credit exhaustion, and device error.
The journaled modification path moves ordinary failures ahead of the first metadata access; an empty failure of multiple handles does not affect other modifications in the same
running transaction. After metadata/revoke has been published, an ordinary error, a device/checksum/state-machine error, or a
handle accounting error permanently aborts the journal, and no cross-operation in-memory rollback is performed.
A successful data I/O only records a persistence dependency and does not mark the handle as having published metadata; an ordinary failure before metadata access
keeps the dependency and allows a retry, for example an unwritten extent conversion that returns `NoSpace` because too few tree blocks could be allocated.
Ordered writeback that publishes metadata must set the transaction dependency after the device I/O completes; the dependency is kept after freeze, and commit performs one device flush before
writing any journal/recovery evidence. A flush failure must not clear the dependency and must not enter the
checkpoint phase; it must abort the journal and report an error to the synchronous caller. This path is still serialized by the mount write lock;
only after the explicit flush of a capacity restart succeeds can all current data dependencies be considered satisfied. Both the running and committing
`flush_ordered_data` entry points internally issue a barrier first and clear afterwards, expose no standalone clear method, and do not hold the transaction's internal lock while performing I/O.
A data-only overwrite of already-mapped blocks does not depend on new metadata; its persistence is provided by the same-device journal flush or direct
flush of inode sync, or by the final flush of filesystem sync, and it is not promised to be persisted before the commit record of an unrelated transaction.

Checkpoint batching must preserve the FIFO write-back order of frozen images: only after all home-block
writes of the selected contiguous prefix have succeeded and been flushed may a single persistent update of the journal tail be allowed; before the tail flush and recovery-state handling succeed, no
transaction in the batch is removed and no journal space is reclaimed. A failure midway through write-back or in either flush keeps the whole batch of pending work
and aborts; retrying an already consumed cache snapshot in the current mount is forbidden, and a remount relies on the retained journal for recovery.
When space is insufficient, the prefix is selected according to the worst-case encoding overhead of the actual journal block count of the transactions awaiting commit plus the upper bound of the next transaction credit,
and the target is capped at the ring capacity minus one, so that one empty block is always kept. `usable_log_capacity()` is the single source of
the reservation rule, shared by append admission, rejection of oversized transactions, and target capping, which avoids semantic drift where the target still cannot be
met after a full drain. The optional budget must not block a current
append that fits; the computation uses the same descriptor/revoke format and checked arithmetic as journal writes. The complete next-transaction
budget covers any metadata/revoke mix sharing the credit pool; the proof is in the design document. Capping at the ring capacity may
reduce the optional budget, so it cannot be taken as a promise that the next transaction will be exempt from another reclaim. The remaining transactions and their policy are not removed. The tail points to `next_oldest`, the journal must remain active, the ext4 recovery feature
must not be cleared, and caches that still have frozen checkpoint images must not be invalidated. Prefix retirement first reclaims, by targeted block key,
entries that are clean, unpinned, and without subsequent ownership; caches of the suffix or of the committing transaction must not be released.
General cache cleanup is not triggered by the checkpoint batch count. `MetadataBlockCache` computes demand from whether the resident block payload
exceeds the 4 MiB soft retention quota; the read/write guard of `Ext4Mount`, at the end of an access and while still holding the mount
protection, advances an incremental scan of at most 64 entries. Read-only accesses can advance reclaim without journal pressure.
Each round fixes its end key, so newly added high addresses cannot postpone wrap-around; dirty, pinned, owned, and frozen-image entries cannot be evicted,
and the budget limits the number of checks rather than the number of successful evictions. A hit sets a one-bit reuse mark under the existing index lock, and a first insertion does not set it;
the scan clears the bit and gives recently reused entries one chance to be retained, which still consumes check budget, and no extra scanning is done because an entry was not evicted.
This bit does not grant eviction permission and does not replace the pin/ownership/frozen-image checks; targeted checkpoint reclaim
and explicit full reclaim ignore the access bit, so that hotness does not block an explicit cleanup. Sustained reuse may leave the soft quota exceeded for a long time.
When there is no demand, only a Relaxed read of the cache's own `AtomicBool` hint is made,
without acquiring the cache mutex; the hint is updated from the actual length inside the same cache critical section as each index insertion/removal/invalidation,
so that a stale scan does not overwrite the demand created by a new insertion. It neither publishes buffer data nor grants eviction permission, and a concurrent stale value is only allowed to
postpone maintenance or trigger one redundant recheck. The actual demand recheck, cursor, and eligibility checks are performed under the same cache mutex;
device I/O is not performed while holding that lock. Ordinary cache read/insert does not evict; an immutable buffer pins both its bytes and its
slot, and the allocator holds prefetch pins until the end of the critical section, so even if the bytes are replaced by a new publication, the slot cannot be reclaimed and
rebuilt. Removal after forget/revoke likewise checks under the index mutex whether another slot reference exists.
The maintenance entry of the mount guard is retained for now, but the correctness of the ordinary reclaim above does not depend on mount exclusivity.
Whole-cache invalidate still requires an exclusive recovery/lifecycle boundary and must not run in parallel with active pins.
Explicit full reclaim and invalidation of the whole cache reset the cursor, and syncfs still drains and reclaims globally.
There is no periodic full scan and no checkpoint-count state. The soft retention quota does not include index/frozen-image overhead and cannot guarantee a
hard memory upper bound: pin/transaction holds, a large number of insertions in a single access, or additions faster than the scan speed can all cause overshoot;
without subsequent accesses there is no automatic progress. There is as yet no general shrinker, capacity admission, or memory-pressure feedback.
The space target limits the reclaim completion condition, but I/O inside the mount write lock can still take long,
and this policy does not guarantee a latency upper bound under device failure.
A production checkpoint entry must first obtain persisted evidence and then be enqueued under the transaction lock; missing evidence is
a broken internal invariant and must not be skipped as a still-committing `JournalBusy` state. Before write-back, it must be confirmed that each transaction's
policy record exists. After `PreserveDuringRecovery` is appended, it is drained synchronously under the mount write lock; if the batch contains
that policy, it must be the last transaction and the whole batch retains recovery evidence; a later ordinary transaction must not be allowed to
overwrite the end-of-batch policy before the drain. The final recovery feature clearing may be performed only after all recovery work has completed.

A commit/checkpoint failure keeps the committing or pending work and permanently aborts the journal, to avoid silently
losing metadata that has not yet reached disk; subsequent sync and mutation both observe `JournalAborted`, and an in-memory
transaction must not be treated as durable. The journal tail may only advance past a contiguous prefix of transactions whose home-block persistence has completed;
the journal start and the ext4 recovery
evidence are cleared only after a batch covering all pending commits has completed. Currently inode sync
uses the conservative semantics of committing the whole running transaction. Explicit recovery likewise may return success only after the legacy list and
orphan-file cleanup have all completed commit/checkpoint, the journal start is confirmed to be zero and the orphan file empty,
and the final superblock state has been flushed; a failure before the cleanup is durable keeps the orphan entry or the recovery feature
as the entry point for the next recovery, and a failure of the final state flush must also return an error to the caller.

The completed N1 `MountedJournal` boundary must continue to preserve the recovery-evidence, revoke, and
checkpoint-failure-retain invariants. The later N2 backgrounding must not swallow errors or clear pending state on the grounds that the complete N3 fault matrix is temporarily missing.
The full linkage of the user-space sticky error/errseq and forced-readonly is to be completed in N3,
after the background execution graph is stable.

## Privacy Analysis

KExt4 stores and returns filesystem data and metadata, where an xattr value may contain a security label
or application data. The core does not log xattr values or file contents.

## Known Limitations

- 整盘 legacy direct/indirect block-map 格式当前不能挂载；只读 legacy mount 需要先把
  core mount state 拆成不可取得 mutation API 的能力类型。带 extents feature 的镜像中，
  个别 legacy inode 仍只支持受检读取；buffered write/page fault 在 reservation 前同步拒绝，
  其他 mutation 最迟在首次 metadata access 前拒绝。
- 运行态已通过 KVFS 暴露 `user.*`、`trusted.*` 和 `security.*` xattr；namespace/DAC 在
  KVFS 检查，缺少完整 LSM/capability 时 `security.*` set/remove 要求 privileged
  credential，create/replace 的四种标志组合由 KExt4 core 在同一写锁的 mutation plan
  中检查。磁盘 immutable/append-only flags 在 iget 时映射到 KVFS 并阻止 xattr mutation；
  当前尚未实现 `FS_IOC_SETFLAGS`，因此不存在需要运行时刷新这些 flags 的修改入口。
- POSIX ACL 当前只是 core 内的 opaque xattr bytes，不实现 ACL permission enforcement、
  mode 同步或 inheritance，因此 kext4 KVFS 层不把 `system.posix_acl_*` 作为普通 xattr 暴露；
  mount parser 接受 `acl` 只用于兼容 Linux 默认开启选项的正向拼写，不得改变上述权限边界。
- EA inode、oversized xattr、bigalloc、inline-data write、huge-file write
  block-unit accounting、encryption/casefold、direct I/O 和 mmap coherence 仍是后续工作。
- HTree SIPHASH root 可以完成格式分类，但 encryption/casefold name-key 语义尚未实现；需要名称
  哈希的 create/unlink/rename 等操作返回 `EOPNOTSUPP`。
- KVFS unmount 已在 topology detach 前后同步 VFS 与 journal 状态，但阻止新引用/写入、
  lazy unmount、freeze 和后台 worker drain 仍需最终 lifecycle gate。
- Regular inodes have metadata transaction targets, but fsync and fdatasync do
  not yet distinguish timestamp-only and data-required metadata. Directories,
  missing targets, and unattributed mutations may still commit unrelated metadata.
- 新增或扩展 mutation 时必须继续证明所有预期失败可在首次 metadata access 前发现；若无法
  做到，需要实现 path-local 显式 unwind，不能恢复 operation savepoint。遗漏路径会保守地
  abort 整个 running transaction，保证正确性但降低该次 mount 的可用性。
- Owned data BIOs and task-context completion cover allocation, unwritten extents,
  and append. Partial initialized-block reads and metadata I/O remain synchronous;
  background triggering, wider inode-target coverage, and mount-lock separation
  remain future work.
- Birth and consume of speculative prealloc spaces are wired into the writeback path: the Hole branch consumes
  a registered space first and, only on a miss, allocates and registers the surplus inside the same allocator
  lock section; the reclaim side (close, eviction, shrink, writeback settlement), group-scan exclusion and the
  ENOSPC retry valve are all live. The `preallocated_blocks` conservation assertion is checked after every
  change in debug builds, so the two sides desynchronizing is exposed immediately.

## Audit Checklist

- Does the metadata parser reject truncated, out-of-bounds, unsorted, or checksum-invalid input?
- Is the directory parser's Linux `rec_len` decoding performed uniformly by `RawDirectoryEntry`? Do HTree reads and writes
  reuse the same root decoder, and are an invalid on-disk hash version, a mount-time unsigned algorithm, and a valid but unimplemented
  SIPHASH classified separately? Do RW/RO mounts with unspecified signedness persist/preserve the on-disk flags respectively? Does Orlov
  always bypass the HTree default/signedness policy and use signed HALF_MD4?
- Does the xattr list only borrow names, while still validating value ranges and the external block checksum? Does a set with an identical
  value return before the journal/ctime update? Are the on-disk immutable/append-only flags
  mapped to KVFS at iget time, and does every xattr mutation return `EPERM` before doing anything? Does a mixed inline/external layout preserve
  all attributes, do ordinary set/remove keep the current extra-isize, does the unified ordinary inode-dirty entry point cover
  directories and non-write metadata updates, does a failed extra-isize expansion fall back safely and cache only
  failures whose layout/resource determination has completed, without recording a transient journal busy condition as a permanent state?
- Does every mutation reserve enough journal credits for all metadata blocks that may be dirtied or revoked?
- When a linear directory is converted to an HTree and then immediately split, does the extent precheck treat the two independent block allocations as two
  worst-case non-merging mappings, and reserve the HTree credits before `EXT4_INDEX_FL` is published?
- Does zero-link cleanup release data, the external xattr block, the orphan entry, and the
  inode bitmap state in a single auditable transaction?
- Can a zero-link inode be accessed only through an existing VFS identity, with its inode number,
  extents, and xattrs remaining valid until the last reference goes away?
- Is FIEMAP bounded both by the end of the query and by the maximum file size of the inode format, and does it correctly distinguish mapped,
  unwritten, delayed, and hole results, as well as `LAST` for truncated results?
- Does a MAP_SHARED write fault first establish a delayed reservation through `page_mkwrite()` before publishing a writable
  PTE; does that path hold the address-space invalidate shared lock and the folio lock without taking the inode
  exclusive data lock, so that non-`SYNC` FIEMAP does not miss dirty holes?
- Is the runtime `iget` private state decoded only by the KVFS cache initializer, with `New/Freeing` waiting and retrying?
- Do namespace mutations pass in the existing VFS victim/moved/replaced private state, rather than having the core reload it by inode number?
- Are new ext4-private transient fields placed in the `Ext4Inode` held by the VFS inode composition, rather than in a separate wrapper
  snapshot, a mount-wide inode-number map, or a generic state container?
- When a directory insertion needs to allocate a new block, does link/rename reserve the bitmap/group/superblock
  growth quota before publishing metadata, rather than relying on the child inode allocation budget that exists only on the create path? Does ordinary file
  creation avoid requesting the parent directory growth quota twice, while preserving the independent child block reservation for mkdir/long symlink?
- Does the documentation distinguish core support, live KVFS syscall exposure, and roadmap capabilities?
- Does the statfs mode change only `blocks`, while keeping free/available/reserved/delalloc accounting shared?


### Directory growth extent-credit boundary

Namespace insertion reserves an operation bound independent of directory contents or
predicted insertion paths. The extent layer bounds new blocks and old external rewrites
using the validated current depth: existing external blocks are reused, each level can
add one sibling, and inline spill/root growth are included. Only a single-block linear
directory eligible for index conversion reserves a second append, allowing one level
of depth growth between appends. HTree dispatch and credits share the feature-and-inode
index predicate. Allocation/create and release/revoke keep distinct credit definitions.
Capacity validation remains ahead of metadata publication because namespace mutations
have no operation-local rollback. No plan object or cached growth-credit field is retained.

An oversized admission returns `NoSpace` without aborting or publishing metadata. A full
running transaction waits for active handles, then commits before the caller retries its
reservation; exact-capacity admission is allowed. Waiters observe the existing transaction
state and share its notification queue. Handle stop and abort notify outside journal locks.
The mount mutation lock serializes complete filesystem mutations. Admission checks the
existing active-handle count and rejects nested handles before any setup or wait, so a
caller cannot wait for its own handle while holding that lock. Independent journal-core
users retain the wait/stop protocol; handle stop requires no mount lock.

Credit exhaustion before metadata publication returns `InsufficientJournalCredits`, maps
to `ENOSPC`, and leaves the journal writable. Exhaustion after publication violates mutation
prevalidation, becomes `InvalidJournalTransaction`, aborts the journal, and maps to `EIO`. The existing
abort state rejects subsequent mutation with `EROFS` before feature or superblock setup;
the mount write guard publishes `RDONLY` to the existing VFS superblock before releasing
mutation protection. Writable remount validates the same abort state before applying
options; buffered-write admission and delayed reservation also reject abort independently
of VFS flag publication, before dirtying the page cache. The guard borrows the caller's
superblock; the mount retains no additional reference or
writable-state field. Regression fixtures cover HTree conversion,
conversion followed by split, indexed leaf splits with external extents, fragmented linear
link/rename, admission one credit below the bound, 64-credit batching across conversion,
feature/index-flag mismatch, nested admission, remount rejection, stop/abort wakeups and
durable reopen.

### In-flight BIOs for allocation, conversion, and overwrite

Hole preparation publishes only bitmap ownership and unwritten extents, never
initialized mappings or a larger `i_disksize`. Committing that metadata before
data cannot expose old disk contents. Once allocated, blocks no longer consume
delalloc reservations; retries reuse their mappings. A later preparation failure
retains completed unwritten allocations for retry, truncate, or final cleanup,
and submits no BIO from the failed batch. Partial unwritten buffers must be zero
filled; partial initialized blocks use read/modify/write outside mount protection.
Blocks larger than cache pages retain synchronous draining to prevent overlapping
partial writes.

Synchronous aligned overwrites use task-owned `BioWait`; deferred overwrites and
allocation/conversion use the inode queue. Both use the same four-batch limit and
drain the other path before switching. Producer exclusion and these drains keep
the combined bound at four; each cache batch separately holds at most 31 pages.
The synchronous pipeline retains submission protection and waits for every
submitted BIO before any failure exit releases/redirties cache completion.
Waiting holds no mount guard and does not depend on the completion worker.
Initialized overwrites update inode metadata and ordered-data dependencies
without reinterpreting extent mappings.

All BIOs are prepared before queue installation and device submission. Truncate,
FIEMAP, last-writer close, and eviction acquire submission protection and drain
without mount protection. Producer preparation and task/worker metadata completion
serialize through mount write protection; no journal handle crosses data I/O.
Workers take neither submission nor invalidate locks. `Completed` mode neither
rewrites data nor allocates holes; it converts only successfully written unwritten
ranges and registers the ordered-data barrier before commit.

Device callbacks only publish BIO results and wake `BioWait` or queue normal work.
Metadata, page state, buffers, and references whose destruction can evict inodes
are handled in task context. Private work exposes no cancellation/closure. Every
accepted BIO must complete exactly once; a missing completion blocks drain and
cannot justify early block release. Workers process submission order, at most
four batches per occurrence, removing slots and waking waiters before releasing
captures. Synchronous callers retain the inode, release submission protection,
wait for their target prefix, and flush completion work before shutdown proceeds.

VFS writeback always uses DataOnly and acknowledges only a contiguous prefix whose metadata settled;
successful BIOs alone cannot clean pages after conversion/accounting failure.
Full fsync no longer changes data-error handling through writeback control.
Real metadata/journal failures retain the abort policy. Original device errors precede
prefix-settlement errors; unsuccessful ranges stay dirty. Preparation and
submission failures are recorded before releasing completion ownership, without
running post-I/O metadata settlement. Queue throttling is not an error. Both
mapping and superblock errors are recorded before ending page writeback. Retry
and non-consuming checks preserve unseen failures, including for newly opened
files; fsync observation does not consume the independent syncfs channel.
Temporary passes read the current error generation before waiting for submission
or pending I/O and check for newly recorded failures after draining. They do not
inherit old mapping errors or mark them observed. Thus repeated syncfs calls on
one file stop reporting a recovered failure after its superblock cursor advances,
without consuming mapping history belonging to existing or later file observers.
Regression tests cover syncfs-only retries, clean repeats, subsequent new errors,
independent file cursors, and a pending I/O failure during submission-lock wait.
Superblock sync attempts remaining inodes, metadata, and filesystem flush after
failure while retaining the first error and honoring filesystem abort checks.
Transaction restart, pending-free admission, and ordered/commit barriers remain
unchanged. A finite error generation cannot distinguish an entire 2^64 wrap cycle.

## Deferred automatic timestamps

Regular-file writes and shared writable faults retain immediate resident mtime/ctime
updates. Only their journal staging is deferred. Per-field dirty state is protected
by the existing inode-state lock; no state guard survives device I/O. Metadata
publication acknowledges only captured values, preserving racing automatic updates
and explicit setattr values. Failed staging leaves timestamps dirty.
Automatic write updates check journal writability before publishing, then compare
and update both timestamps and dirty bits under one inode-state write lock. They
preserve timestamp precision, pending atime, and already-dirty unchanged fields.

Writability queries use the transaction engine's atomic `has_abort_error`
projection rather than acquiring its shared transaction mutex. Abort retains the
first error under that mutex and publishes the projection before unlocking and
waking waiters. Only a successful empty-state replay reset clears it. The atomic
query does not expose transaction data or authorize later metadata mutation:
those operations retain their locked abort checks. A buffered write that races
abort can pass a check before publication, as with the previous locked query;
subsequent checks reject the published abort. This adds no timestamp batching,
precision loss, or change to the superblock read-only publication boundary.
Mounted abort callers, including asynchronous writeback metadata completion,
retain the mount write guard through atomic abort publication. The guard's Drop
marks the superblock read-only before releasing mount exclusion; another write
guard for that mount cannot overlap this publication. Any abort source moved
outside mount protection must publish superblock read-only state at its own
failure boundary rather than depending on a later guard drop.

File-range fsync waits for the requested data before filesystem persistence; an
empty range still runs journal/device sync. Generic data wait does not flush the
device and does not advance an open file's error cursor.

Full fsync, syncfs/unmount inode metadata writeback and linked-inode eviction stage
pending timestamps. Data-only mapped overwrite may omit a timestamp-only transaction;
a later full fsync must still persist timestamps with no dirty folios remaining.
This does not weaken the data or journal flush barriers or add a background worker.

Buffered write admission checks live immutable policy even for empty requests,
before generic size limits, and publishes automatic timestamps only after the
size and journal checks plus the Path read-only check. It uses the VFS inode's
representable current time and the same resident dirty-field update as `update_time`.
This VFS policy also applies with `KFEAT_FS_TIMES=n`: disabling the adapter feature
must not replace an existing write timestamp with a synthetic epoch value. Shared
writable faults and explicit timestamp updates retain the same VFS policy.
It does not suppress timestamps or coarsen their precision. Journal abort may race
a completed admission check, as before; write-begin and later metadata mutation
retain their own authoritative rejection points. Shared write faults and explicit
setattr keep their existing paths.

Buffered preparation selects the extent/bitmap size limit and publishes mtime/ctime
under one inode-state write lock. The mounted capability snapshot contains only
immutable geometry/format limits and a strong reference to the same journal;
abort is checked live, never cached. Empty or rejected
requests do not modify timestamps; clipping uses the remaining representable file
range without offset addition. These guards are released before data copy or I/O.

An initialized-range hit may skip mount protection at write-begin, but still checks
journal abort and supported inode format. KVFS data/invalidate protection excludes
truncate and fallocate, and the inode-state lock observes size, reservations and
mapping-proof invalidation together. Holes, unwritten extents, extensions, partial
EOF blocks, pending reservations and cache misses retain the existing slow path.
The hit does not allocate or remove blocks, change reservation accounting, submit
data or claim durability. Each fsync retains its own data wait, timestamp staging,
transaction target, device barrier and error cursor checks. Tests require a cached
overwrite to complete while mount mutation protection is held, then verify its
bytes on the backing device after fsync; format and aborted-journal rejection are
tested separately.

Stable mapped overwrite completion skips inode-table rewriting when no timestamp
is dirty: mapping and disk size are already protected by submission exclusion.
It still registers ordered-data flush for every successful range, even if an earlier
transaction committed the inode metadata. A later timestamp update remains dirty
for the next range or final full fsync. Data/barrier failures retain the existing
error recording and journal abort behavior; clean metadata never implies durable data.

## Fsync transaction and data coverage

Every file synchronization waits for its data and applies its own metadata intent
before synchronizing the tracked transaction. Transaction progress can avoid a
duplicate journal commit; it cannot cover data written after that commit. The
already-committed target and no-new-transaction paths therefore retain device
flushes. Full fsync must stage dirty timestamps even after fdatasync cleaned the
PageCache. Journal abort is checked before accepting a committed target.

KVFS retains per-open error cursors independently of transaction completion, so
a successful retry does not erase another file's unobserved failure. There is no
inode request batch, saved batch result or coordinator lock. Existing submission
guards and drain-before-truncate/release rules still protect concurrent mapping
changes; missing device completion can stall data waits and is never treated as
successful persistence. Tests exercise overlapping full/data-only requests,
per-open failure observation and later data barriers after an earlier commit.

## Quota metadata I/O boundary

Quota files use the canonical VFS inode retained by superblock quota state.
The trusted caller must validate same-filesystem membership and protect the
file against ordinary writes before activation. The storage adapter accepts
an existing mount-state borrow and, for writes, a journal handle in task context;
it introduces no unsafe code, persistent owner, atomics or recursive lock.
Mount write exclusion precedes the quota mutex. It must not execute under
spinlocks, IRQ/preemption-disable guards, in IRQ or during early boot.

Quota payload reads use the metadata cache to avoid stale home-block reads
before checkpoint. Writes reject filesystem-block crossings before mutation,
check existing extent/inode credit bounds, initialize newly exposed blocks and
journal payload with mapping/size changes. A mutating format operation must remain
in one admitted transaction; its caller must complete or abort the handle before
releasing the filesystem state. Physical admission includes optional
extra-isize external-xattr allocation through the existing inode bound; journal
credit extension alone is insufficient. Native read and write failures are classified
before errno conversion. A pre-publication nonfatal error leaves the journal
usable; a failed operation with published metadata still requires abort.
Inactive dirty dquots report failure once without quota-file I/O and clear
their dirty indication; initialization remains owned by acquisition. Structural failures cannot be recovered by
retrying a partially modified tree. Filesystem sync uses the existing abort
policy on write failure and still attempts journal drain; dirty record state
for active records is only cleared after its quota-file write succeeds. A later commit failure
requires filesystem recovery even if in-memory dirty bits were cleared.

Internal user/group/project quota activates from validated allocated regular-file inode
numbers before recovery can change allocation. Activation validates the reserved
user/group numbers and rejects malformed quota identities with EUCLEAN before
quota-file I/O; invalid identities never exempt ordinary inodes from charging.
Quota identities are excluded
from namespace lookup even when already cached, and only those identities bypass
charging. A weak back-reference reaches the sole VFS quota owner without an
ownership cycle. Project identity is read from the inode only when the filesystem
advertises the project feature.

Acquisition precedes inode reservation locks; each inode binds the same record
as the cache and imports existing reservations under its state lock. Cached updates cannot
perform quota-file I/O. Extent and inode-counter validation precede all
reservation admission; successful admission has no fallible publication step.
Rollback failures are logged without replacing the original admission or
allocation error, and fatal journal errors abort mutation. Lock order is mount
exclusion, ordinary inode state, quota state, then quota-file inode/allocator
state. Internal quota files cannot
enter ordinary buffered-write reservation paths. Record updates are atomic
across active classes; inode/space limits fail before bitmap publication.
Preallocation hints pass through the same quota admission and settlement as
fresh physical runs. Rejection leaves the hint and bitmap intact; successful
allocation consumes reservations once. Failed published operations
abort rather than reusing partial state. Ownership transfer includes both
allocated and reserved bytes and persists quota records with the inode owner.
Allocated-sector changes and quota record changes share the inode state lock;
overflow and quota admission failures leave both unchanged. Quota backing inodes
update their own sector count without recursively charging quota records.
Shared external-xattr reference release frees one owner's usage even if the
physical block is retained; copy-on-write cannot accumulate a duplicate charge.
Fallocate batches include quota-record credits alongside allocator metadata.
Fallible extra-isize expansion precedes transfer and resident owner publication.
Writable feature and journal-abort checks are repeated under the mutation guard
used to activate, resume or initialize quota state.

Read-only remount first excludes VFS mutations and writable opens and rejects
remaining writers or unlinked live inodes. It drains writeback and cached
records before setting the existing suspended state, retaining file ownership.
A failed writable remount suspends loaded classes without further I/O; it does
not leave accounting active on a read-only superblock. Lookup and read-only
open require no quota acquisition or writable journal, including after abort.
Writable open and metadata/namespace mutations acquire their affected owners
before changing allocation state; journal-abort checks still reject mutation.
External-xattr release validates the inode block count and bound quota records
before changing a block bitmap or shared reference count; unsupported inode
accounting and invalid counts cannot partially publish that release.

Final disable drains inode I/O and completion captures before taking mount
mutation protection, then drains cached records with bounded transactions.
Suspension waits for in-flight inode eviction outside mount mutation protection
before detaching inode records. Suspended
classes unload without quota-file writes. If the journal has aborted, final
release invalidates the quiesced class cache and file information without I/O.
Writeback failure is returned separately from file ownership, all classes are
processed, and recovery evidence is preserved on disk. Retained quota
inode references are released by the mounted owner after its filesystem/quota
guards end. The core release helper is private to quota integration, preventing
callers from receiving a file under their own mount guard. Final eviction can
therefore enter filesystem code safely. Quota-control authorization is
performed by the syscall adapter.

Filesystem range/length checks precede sector-addressed Bio I/O. Live media
capacity, queue alignment and read-only checks remain in block core; a later
media shrink returns a device error. Aligned byte reads borrow the destination
and may partially modify it on failure; partial blocks use staging.

## Quota-control boundary

Quota operations run under superblock shutdown admission. On/off hold its
exclusive side; queries and record updates share admission. They acquire native
mount protection before the canonical quota mutex. Backing-file references are
copied while holding that mutex, then inspected or released after unlocking it;
status queries therefore cannot invert the native mount/quota lock order.

External activation rejects a foreign superblock, non-regular backing inode,
duplicate class, or already protected quota file. `NO_QUOTA` excludes payload
allocation from recursive charging. Persistent immutable/no-atime flags and
live VFS flag queries prevent ordinary writes, metadata changes, namespace
removal and automatic atime updates from modifying active quota files.
Quota-file data exclusion precedes native mount protection. Sync invalidates
old file-cache pages only after journal and device persistence completes.

Privileged record changes require the current subjective CAP_SYS_ADMIN.
Own-user and effective-group queries remain available without that capability.
The integration checks CAP_SYS_RESOURCE for a current user caller before quota
admission; no opener credential or per-inode permission cache supplies bypass.
Project-ID changes transfer allocated and reserved usage before the new ID is
published. Project-inherit directories reject links and renames whose project
IDs differ, preventing an operation from bypassing the directory's quota domain.
Attribute changes require inode ownership or CAP_FOWNER; changing immutable or
append-only flags remains unsupported until mapped-write exclusion is complete.
For an inode that remains immutable, project identity is checked only when the
filesystem supports the project feature.

An external quota header is file input until activation succeeds. Header-format
rejection is propagated without aborting the filesystem's journal. The read-only
load has no journal handle, and its I/O adapter retains ordinary filesystem
I/O/corruption classification. Activated quota updates remain subject to the
existing metadata transaction and abort rules.

Read contexts cannot write without the optional borrowed journal handle. This
is write authority, not another journal or quota owner. Record acquisition reads
existing state before reserving space for a missing record; queries retain the
same acquisition requirements as accounting. Existing-record updates and grace
headers need bounded journal credits without physical-growth admission.

Quota file ownership handoff requires both mapping revocation and persistence.
Activation revokes writable PTEs before flushing ordinary data. Existing writable
descriptors cannot bypass immutable policy through mmap, mprotect or write faults.
Each ordinary write reenters shared superblock admission; quota enable/disable
retain exclusive admission through initialization and teardown. Disable stops
class accounting, detaches inode references and waits for in-progress evictions
before removing records. Reservations remain with their inodes for a later
activation. Disable drains the final quota metadata
transaction through checkpoint before restoring ordinary file access. No parallel
mapping registry, quota file table or compensating policy flags are added.

## Selection/admission boundary

The existing producer mutex excludes another scan until every declined unsent
batch is redirtied and its WRITEBACK ended. NONE admission refusal is capacity
feedback with skipped-page accounting, not an I/O failure. ALL waits capacity;
workers require no producer mutex. No new rejection field or coordination lock
is added. Buffered write_end cannot reacquire the copied folio or wait for
writeback while it publishes accepted size. Completion remains independent of
the inode data lock retained by writer rollback.
