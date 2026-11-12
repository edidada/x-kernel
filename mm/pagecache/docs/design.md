# pagecache — Design Document

## Scope and Role

`pagecache` is the folio storage and generic caching algorithm library used by `kvfs::AddressSpace`. It corresponds only to the implementation details on the Linux
`address_space::i_pages` side. It is neither a file object nor a second
`struct address_space`.

`MappingPages` 不拥有 inode、可见文件长度、VM object identity、`i_mmap` views 或
mmap invalidation 生命周期。Direct dependents currently include `kvfs` and the filesystem crates `memfs`, `kext4` and `v9fs`. MM and the open-file runtime must not reference this crate directly.

## Scope

- sparse `PageIndex -> Folio` storage;
- insert-if-absent for folio materialization;
- dirty/writeback enumeration;
- tail-page zeroing and folio deletion after the owner supplies the old/new `i_size`;
- releasing cached folios at final inode teardown.

Not included:

- inode/`i_size` ownership;
- `AddressSpaceOperations`;
- VM object-id, mapped-view registration, or PTE invalidation;
- VMAs, fault dispatch, permissions, and the syscall ABI.

The implementation is split into private modules by responsibility, and the crate root exports only domain types:

- `mapping_pages.rs`：容器、锁内 resident tree/dirty marks 与 dirty reconciliation；
- `folio.rs`：页分配、数据访问、uptodate/dirty/writeback 状态和释放；
- `folio_batch.rs`：读取、写回与截断共用的有界引用集合，以及持页锁的读取访问接口；
- `filemap.rs`：lookup、连续引用批量收集、materialize 和 insert-if-absent；
- `readahead.rs`: bounded missing-range lookup, page reservation and transfer of read initialization ownership;
- `truncate.rs`：尺寸变化时的页移除、尾页清零和 final teardown；
- `invalidate.rs`: byte-range invalidation of clean resident folios, reporting
  dirty residue as busy;
- `writeback.rs`：dirty candidate、batch helper、开始/完成协议与结果类型；
- `ut-tests/`: resident storage, truncate, invalidate, and writeback tests.

## Architecture

```text
VfsInode
  -> kvfs::AddressSpace { host, a_ops, object_id, mapped_views }
       -> i_pages: pagecache::MappingPages (held directly)
            -> Mutex<MappingPagesState>
                 -> BTreeMap<PageIndex, Arc<Folio>> entries
                 -> optional Arc<dyn CacheResidency> (one per mapping)
                 -> BTreeSet<PageIndex> dirty_marks (derived index)
                 -> BTreeSet<PageIndex> writeback_marks (derived index)
                 -> BTreeSet<PageIndex> towrite_marks (本轮待写选择)
```

`kvfs::AddressSpace` supplies a closure at materialize time, and its own
`AddressSpaceOperations::read_folio()` shapes the folio. At writeback time, `AddressSpace` also passes in the current inode `i_size`; `MappingPages` does not cache a second copy of EOF.

`Folio::dirty` is the source of dirty state. The Rust `BTreeMap` has no per-entry
mark like the Linux XArray, so `dirty_marks` serves only as a derived index for `PAGECACHE_TAG_DIRTY`, used to enumerate
writeback candidates; it does not grant permission to write back and does not introduce a second copy of folio state.
`writeback_marks` is likewise a derived index for `PAGECACHE_TAG_WRITEBACK`, used only for bounded wait enumeration.

## Calling Constraints / Execution Context

- Lookup, allocation, waiting and explicit truncation require sleepable task context; final container destruction does not wait for initialization;
- `FolioRead::complete` and Drop may run in ordinary I/O completion context without acquiring sleepable locks;
- 不依赖 current process；
- owner 必须在调用 resize/writeback 算法时提供已串行化的 inode 状态。

共享页索引使用可睡眠 mutex，竞争时先执行最多 10 轮有界指数退避，再进入原有
等待队列。此锁的自旋预算内不主动 yield，避免已缓存页的短暂 tree lookup 竞争
变成调度延迟；长 mutation 或 owner 被调度走时仍可阻塞。该策略只作用于页索引，
不改变全局 mutex 默认值、锁序、folio 身份复核或文件读写语义。

## Algorithm Flow

### Materialize

1. 查询 folio tree，缺失时在索引锁外分配，再持锁复核并插入非 uptodate folio；竞争失败的候选在锁外释放。
2. 索引锁下仅尝试非阻塞取页锁；竞争时释放 state 锁等待，醒来后重新查找。
3. Release the state lock. For a non-uptodate page, transfer responsibility for
   the held folio lock into `FolioRead` and call the owner's read closure.
   The completer publishes uptodate before unlocking. The submitter waits,
   rechecks uptodate and resident identity, then accesses the data.
4. 初始化失败不暴露部分数据，后续读取可重试；持页锁更新 dirty index 后再解锁。

### Resize cached folios

`resize_cached_folios(old_len, new_len)` consumes only the sizes supplied by the owner:

- growth: zero the bytes in the already-cached old tail page between the original EOF and the new range, mark that folio dirty, and
  insert the dirty tag;
- shrink: clear and delete the whole pages beyond the new EOF;
- partial EOF: zero the tail of the surviving folio and mark it dirty.

For growth and shrink, candidate references are obtained under the index lock, the pages are locked after the index lock is released, and the index lock is retaken to recheck identity.
Tail zeroing is done inside the page lock and outside the index lock; the dirty tag is updated through the shared reconciliation before the page is unlocked.

This method does not update any visible length and does not produce VM invalidation. The Linux
`i_size_write -> unmap -> truncate page cache -> unmap` transaction is the responsibility of
`kvfs::AddressSpace`.

### Byte-range invalidation

`invalidate_pages_range(start, end)` discards clean resident folios in a
half-open byte range that is rounded outward to folio boundaries:

- an empty or inverted byte range is rejected before folio rounding, so it
  never discards a folio;
- folios under writeback are waited for before the folio and index locks are
  taken, and the identity is rechecked after locking;
- a folio that is still dirty is left resident and the whole call reports
  `ResourceBusy`; clean folios are discarded in either outcome;
- the scan advances a cursor across bounded batches, so a dirty folio does not
  stall the rest and the range is visited once.

The operation only touches page-cache state. Callers must already have removed
the user PTEs and completed the matching TLB invalidation for every mapping
view covering the range, because discarding a folio returns its physical frame
to the allocator immediately. Serialization with cache population and with
truncate belongs to the owning `kvfs::AddressSpace`, not to this container.

### Writeback

`pagecache::WritebackControl` owns one invocation's range, mode, attempted-page
budget and filesystem-skipped count; KVFS reexports the same definition. All
three adapters borrow it directly. Pages consume budget when handed to the
filesystem, including failed or declined attempts; completion never refunds it.
NONE stops on exhaustion or the first error. After starting with a nonzero
budget, ALL processes its selected set despite later exhaustion/errors and
returns the first error. A busy cache candidate is not a filesystem decline.

Each pass captures a finite resident endpoint. ALL adds DIRTY indexes to the
shared TOWRITE derived index in windows of at most 31 and then scans that index;
NONE scans DIRTY. These are selection marks, not private snapshots. Handoff
consumes TOWRITE; detach/replacement removes the old mark. Returning from a pass
never clears other callers' selection. `util/kxarray` has no tag API, so the
existing BTree indexes remain the storage representation.

The common preparation helper locks the folio, samples the owner's live EOF,
and rechecks identity/dirty/WRITEBACK before preparing valid bytes. NONE skips
existing WRITEBACK; ALL waits and revalidates. Batch adapters submit pending
snapshots before a contended folio lock or writeback wait. A partial EOF folio
ends its batch, so later growth starts another payload at its real page offset.
The cache stores no EOF, generation or new per-folio state.

`with_folio_or_create` and `pin_folio_or_create` share the same lookup, allocation outside the lock,
insert race, resident identity recheck, initialization, and dirty reconciliation flow; the two differ only in the
access result after the lock is taken. The read closure is not consumed until the possibly-retrying lookup/wait phase has completed.

writeback enumerates the target range from a separate dirty folio index and does not scan all resident folios.
The dirty tag only produces candidates `(index, Arc<Folio>)`. Before actually starting writeback, writeback rechecks, in a single critical section
under folio lock → MappingPages state lock:

1. `state.entries[index]` is still the same as the candidate `Arc`;
2. the current folio is still dirty;
3. the current folio is not yet under writeback.

缺失或已替换的候选不会写回，也无权修改新 resident 的 marks；同身份 clean 候选清除 dirty tag。
通过校验后，writeback 在同一临界区内清 dirty、设置 under-writeback 并删除 dirty
tag。I/O 期间发生 redirty 时，folio access 会重新插入 tag；I/O 失败时，当前 resident
folio 会恢复 dirty 并重新插入 tag。Completion holds only the index lock and checks resident identity plus atomic
dirty/WRITEBACK state; it never acquires the content lock or inode data lock.
只有仍是当前 resident folio 的失败写回才能重新标脏并修改 dirty tag，
已移除或替换的 folio 只结束 under-writeback。batch writeback 同时核对最终 dirty 状态；
单页同步 writeback 在持有 folio lock 的 callback 成功后保持 clean，后续 writer 会在
clean → dirty 状态变化时重新插入 tag。
开始时同步设置 writeback 位与 mark；完成时按 folio identity 清 mark，释放锁后
唤醒共享哈希等待队列。等待者持有 folio 引用并重查 writeback 位。

`with_folio` and `with_folio_or_create` perform reconciliation only when the final dirty state differs before and after the closure.
dirty → dirty and clean → clean do not change the tag, which avoids taking the MappingPages state lock
an extra time on read-only access to a dirty folio.

`submit_cache_pages` 与同步 `write_cache_pages` 共用枚举和状态转换逻辑，
但把数据快照与完成责任交给 `WritebackBatch`。`into_parts` 将数据交给 BIO、
将 folio 引用和索引引用留给 `WritebackCompletion`；提交回调返回不结束写回。
All adapters count pages handed to filesystem work, including declined batches.
The callback borrows the same mutable control until it returns and may account
filesystem skips; deferred completion retains no control. Whole unsent NONE
declines account their page count, restore all folios synchronously and return
WouldBlock; ALL waits admission. Each batch holds at most 31 pages; filesystems
bound total in-flight work and serialize selection through admission/rollback.

The index mutex is embedded in `MappingPages`; synchronous completion borrows
it. Cross-task `into_owned` creates an `OwnedWritebackCompletion` retaining the
complete address-space owner and validates that its index projection matches the
original batch. It neither extends index lifetime independently nor copies index
state. The filesystem still retains inode and physical-mapping protection;
address-space ownership does not replace truncate/drain coordination.

Completion acknowledges fully completed folios within a byte prefix, redirties
the suffix, and preserves concurrent dirtying. An invalid prefix or dropping an
unconsumed completion redirties the entire batch. Completion changes atomic folio
flags and index marks without the content lock or the submitter's wait handle.
The index mutex requires sleepable task context for completion and Drop, never
BIO IRQ/BH callbacks, and only after associated I/O has drained. Upper layers own
error recording, background scheduling, and fsync semantics.

## Concurrency Model

- Folio 与 pin/token 共享同一稳定 `Arc<Folio>`，页的所有权、identity 和数据锁不分离；
- Ordinary access callbacks borrow `FolioGuard`; initialization callbacks own
  `FolioRead` until completion. Neither can replace or move out the published
  folio; cache replacement updates the index reference.
- `copy_to_uninit` 在原页自己的锁内完整复制初始化页；摘除不禁止复制，EOF 由调用方校验；
- 一个 mutex 保护 resident folio tree 和 dirty index；
- The folio lock protects bytes, readahead state and charge attachment. The existing atomic flags also hold dirty, uptodate and invalidation state, allowing final detachment without taking the content lock;
- 同时核对 resident tree、dirty index 与 folio 状态时，锁顺序固定为
  folio lock → MappingPages state lock；索引锁下只允许非阻塞 try_lock，失败后释放索引锁再等待。
  folio access closure 持页锁完成 dirty index reconciliation；
- 本 crate 不拥有 inode data lock、mapped-view lock 或 MM lock；
- 文件系统 I/O 在 folio-tree mutex 外执行；读入前将非 uptodate folio 纳入 resident tree，
  页锁串行化初始化，等待者释放索引锁睡眠并在醒来后重新查找身份；
- `visit_resident_folios` 一次索引快照收集至多 31 页的驻留引用与位置；随后
  逐页访问时，已初始化命中只取 folio 锁。非 uptodate/失效候选重新查找，
  初始化错误不转为空洞；owner 持 invalidate shared 覆盖整个批次；
- `with_folio` 仅暴露 uptodate 数据；初始化失败保留非 uptodate 页供重试，
  truncate 不将其部分内容标脏；`readahead_folios` 使用固定容量 FolioBatch 保留并锁定缺页段，
  回调返回后保持页锁，取得索引锁复核身份并摘除未消费后缀，再释放页锁；
  等待者醒来后重新查找 resident，不初始化已摘除的旧预留页；
- `filemap_add_folio` is available only in unittest builds for handing over a complete Folio; on conflict it returns ownership unchanged;
- to keep the candidate stable during single-page synchronous I/O, `writeback_until` holds the folio lock while calling the
  backend callback; that callback must not re-enter the same `MappingPages`;
- `write_cache_pages` completes the state transition and data copy under the lock, and calls the batch
  backend callback only after releasing the folio lock;
- `wait_on_writeback_range` does not sleep while holding a lock; truncate first removes non-busy pages, then waits for writeback and removes the remaining pages after rechecking identity.
  KVFS still uses the invalidate shared lock to cover the complete writeback operation; this phase does not narrow its lock scope.

## Design decisions

The Linux reference source is the user-specified `/root/codes/linux`, version 7.2, commit
`818bebeb63dd6bf5f4e07e145f6cdbace520a34c`. In `include/linux/fs.h`,
`address_space::i_pages` is an embedded XArray; `mm/filemap.c`, `mm/truncate.c` and
`mm/page-writeback.c` operate on that index around the mapping. Here the methods of `MappingPages` express
the low-level algorithm, and no separate PageCache owner is created.

`MappingPagesState` is lock-protected data internal to the crate and is not exported. It is not a new semantic object:
Rust `BTreeMap` has no per-entry marks, so the entries and the derived dirty marks must be placed under the same
mutex to keep the index consistent. Folio data and flags remain encapsulated in `folio.rs`.

1. The standalone algorithm crate is kept because folio allocation, dirty/writeback and sparse storage are reusable
   algorithms; its API does not expose file object semantics.
2. Length is not stored. The only EOF is `VfsInode::size()`.
3. Neither object-id nor views are stored. They belong to `kvfs::AddressSpace`, corresponding to Linux
   `address_space::i_mmap`.
4. Resident folios use a `BTreeMap`, and dirty/writeback tags use a separate `BTreeSet`. This set only makes up for
   the lack of per-entry marks in `BTreeMap`, so the enumeration cost depends on the number of tagged folios;
   a Linux XArray dirty-tag style implementation can replace the concrete index in the future without changing the `AddressSpace` contract.

The immutable `index` of a Folio corresponds to Linux `folio::index` and is retained after detach, for use by batch processing and
identity validation; FolioBatch stores only references, and the entries key must match this index at insertion.
Ownership is still expressed by the `Arc` identity of `i_pages[index]` in the owning AddressSpace; no back-pointer to the owner is added.

## Drop / resource release

`AddressSpace` directly owns `MappingPages`, and does not separately allocate or reference-count a cache container.
When the AddressSpace is destroyed, the container is released together with its fields; the folio references held by the entries are released with it, and existing
folio references still keep the page data alive through `Arc`.
`truncate_final()` is the explicit discard path for final inode teardown; an ordinary writeback failure still keeps the
dirty state. Shrink and final teardown delete both the resident folio and the corresponding dirty index entry;
the surviving truncated tail is zeroed after writeback completes and re-added to the dirty index; when growing, the tail page is retained, zeroed and marked dirty under the folio lock, without waiting for batched writeback that holds a data copy.

MappingPagesState Drop has exclusive access to the index and atomically invalidates
each entry, clearing dirty without waiting for its content lock. Read tokens
retain their page, capacity charge and backing resources independently; a later
completion cannot reinsert a detached page. Writeback completion either borrows
the index or retains its complete mapping owner, so it cannot overlap final
index destruction. Owners that must drain I/O
before releasing backing resources call `truncate_final()` explicitly. That
operation waits for initialization and writeback before removal. Admission to
resize, final truncation and writeback validates sleepable context before mutation.

### Readahead marks and cache history hints

The readahead mark of a Folio denotes the trigger point of the next window; KVFS sets it on newly reserved pages, and on a cache hit it is
consumed under the page lock; pages under writeback retain the mark. The mark does not indicate that initialization is complete, nor does it carry page ownership.
`preceding_cached_pages` performs a bounded backward lookup under the index lock and provides a hint of contiguous residency history;
the policy state belongs to the open file, and the window and EOF policy do not enter MappingPages.

A batched read may consume the readahead mark after rechecking initialization and invalidation state under the folio lock; the callback does not change data,
dirty or residency state, and does not wait for other folios. Readahead I/O runs after the callback returns and the page lock is released.

## Stable page objects and long-lived pins

Each resident entry stores `Arc<Folio>`. The object owns its allocation, immutable
index and a task-independent lock protecting content and mutable
state. Ordinary access callbacks receive `FolioGuard`, which has no mutable projection to
`Folio`; safe callbacks cannot move out or replace the published page. Replacement
updates the cache index reference. Existing pins retain exactly the old object,
including its original synchronization, without retaining the replacement.

`PinnedFolio` owns one reference to this same object. `map_readonly(self, ...)`
consumes the access pin and returns a `#[must_use] MappedFolio` ownership token.
The mapping owner stores that token until the PTE is removed and its TLB
invalidation completes. Publication remains unsafe because its owner must
validate EOF and exclude invalidation through the enclosing VFS guard.
`MappedFolio::map_readonly(&self, ...)` publishes another PTE of the retained
object, for fork or reconstruction, and returns its own ownership token.

`copy_to_uninit` accepts exactly one page through an array reference, encoding
the destination length and full initialization guarantee without a runtime
assertion. It locks the same object even after cache detachment or replacement.
It deliberately permits copying the retained page; detachment is not revocation
or an immutable snapshot. File extent must be validated separately, and partial
truncate can change a retained tail under the same lock. No identity-mismatch
fallback or second allocation owner is necessary.

Pins and mapping tokens use ordinary shared ownership without a second pin
counter or registry. The physical address is derived from the stable allocation.

## Residency admission and initialization

The inode address space retains the invalidate lock covering EOF, cache
reservation and read submission. Transferred folio locks serialize later completion with truncate.
The filesystem owns capacity policy and supplies one `CacheResidency` provider
when constructing an inode mapping. Only a winning new identity calls
`try_add_folio()` under its folio and index locks. That successful admission
attaches one provider-owned capacity charge to the stable folio. Detaching the
cache identity does not return it: pins, mapping tokens and read batches keep
the physical allocation and its charge alive together. Cloning never charges
again. After the last reference drops, Folio frees the physical allocation and
returns its charge with neither the folio nor mapping index lock held. The
charge retains the provider even when the inode mapping has already gone away.
Providers must not reenter mappings, acquire folio locks, or perform I/O.
Release and final provider destruction must not sleep: the last reference can
be released by I/O completion. Admission still requires task context.
For memfs, the provider is its filesystem-owned quota, so lock order is cache
folio -> index -> quota limit; remount and statfs never acquire cache locks.
Speculative physical storage remains outside the admission policy until insertion.

The cache publishes non-uptodate reservations before filesystem I/O and keeps
its index lock out of I/O, preserving the current main concurrency contract.
Unconsumed readahead reservations detach while locked and release their charges
when the batch drops its final references after unlocking. Failed reads retain
their non-uptodate reservation and charge until retry or removal.

`visit_resident_folios` snapshots only one bounded page range. Missing identities
are reported as None from that snapshot and never reserved or initialized. This
allows a filesystem to own its sparse algorithm without a cache policy flag.
Callbacks may stop before the next page; errors retain earlier callback progress.
The fixed FolioBatch and index array allocate no additional heap storage.

## Transferable initialization

`FolioRead` owns one `Arc<Folio>` and the already acquired folio lock. It is the
read responsibility, not a second page owner or a request registry. Consuming a
readahead reservation transfers this token; unconsumed reservations stay locked
until removal. Success publishes whole-page uptodate before releasing the lock;
failure or abandonment unlocks without publishing partial data. Completion may
outlive submission and move between CPUs. It cannot require inode data,
invalidate or mapping index locks held by a waiting caller.
The token implements the block contract's `BioMemory`: its original page can
enter a Bio and return as a typed owner to filesystem completion. This adds no
folio fields, request registry or driver dependency on pagecache.

For subpage I/O, consuming `FolioRead::split_at` creates disjoint `FolioReadPart`
owners. Only these reads allocate a temporary shared completion containing the
original lock token and a count of bytes not successfully initialized. Parts may
split further or complete in any order. Each successful part credits its bytes
once; failed or abandoned parts retain their deficit. The last part releases
the original token, publishing uptodate only if every byte succeeded. No folio
fields or per-block validity bitmap are added; failed reads retry the whole page.

The folio's atomic flags contain lock, writeback, waiters, dirty, uptodate and
invalidation bits. Acquire/Release
on the lock protects `UnsafeCell<FolioState>` and page bytes. Hashed IRQ-safe
wait buckets match folio identity and bit: unlock wakes one lock waiter;
writeback completion wakes all matching writeback waiters. Stack-pinned wait
entries are unlinked before destruction, and task parking happens outside the
bucket lock. Enrollment sets the waiters hint and rechecks the predicate under
the bucket lock; uncontended completion takes no bucket lock. Only prepared,
IRQ-safe notifiers borrowed from `CompletionWait` run under that lock, without
allocating or sleeping. Registration, wake storage and progress stay on the
waiting task's stack; no per-contention owned Waker is created.
Lock waiters distinguish notification from granted ownership. After five lost
acquisitions following notification, a waiter requests handoff: the waking side
acquires the lock bit and publishes the grant before notifying it. A granted
waiter accepts ownership without reacquiring. Scheduler-only wakeups do not
consume this budget. Exclusive wake stops after one selected waiter and may
leave the conservative waiters hint set until a later scan. Shared bit waits
observe one completion; the writeback API rechecks the writeback bit afterwards.
Writeback waits read its atomic bit without taking/releasing the folio lock,
avoiding self-notification. References protect allocation lifetime separately
from exclusion and current cache identity. No task-owned MutexGuard is transferred.

Demand-read waits may return `Interrupted` for a pending uncatchable kill. A
concurrent ownership grant wins over cancellation; otherwise the waiter unlinks
under the bucket lock before releasing its notifier borrow. Cancellation does
not cancel the read token or its I/O. Explicit truncation and writeback scans
remain uninterruptible after successful context admission. Writeback completion
requires no folio wait, even when another task holds the content lock. Completion
clears the writeback bit and its index mark under the same index lock, then wakes
waiters after releasing that lock. Lock statistics count one successful
acquisition and one contention on the first actual block; task snapshots expose
the waiting folio address until parking returns.


## Finite data-completion targets

`MappingPages::capture_writeback_range` validates a half-open byte range and
samples its highest resident index through the existing BTreeMap range endpoint.
The resident tree includes clean pages with older in-flight WRITEBACK, so a dirty
index alone would be insufficient. The result clips an overflow-safe exclusive
page endpoint to the requested end; an absent target becomes `start..start`.
`0..u64::MAX` remains a valid full-file sentinel even for its final partial page.
No page references, index lock, extra field or copied EOF survive the capture.

KVFS passes the same invocation-local target to submission and
`wait_on_writeback_range`. Higher indexes inserted later cannot extend its wait.
The target bounds work rather than freezing contents or identity; normal folio
revalidation and the filesystem's backing-map lifetime protection still apply.
Capture and wait require sleepable task context, with no current-process
requirement. Pagecache reports no journal/device durability and owns no fsync
intent or per-open error cursor. TOWRITE selection and attempted-work accounting
share the finite pass boundary; buffered EOF publication belongs to the owner.

### Bounded snapshot allocation

Snapshot builders derive a contiguous run from the already collected FolioBatch,
limited before traversal by the callback byte limit and NONE's remaining budget.
They reserve capacity for that run before copying, avoiding repeated Vec growth
and copying of the existing prefix. Partial EOF reserves only its valid bytes.
Geometric rounding preserves the previous Vec capacity bound across windows
(at most 128 KiB for 31 data pages). Capacity is not prepared work and consumes
no extra budget. There is no retained buffer pool, cached size or new field.
