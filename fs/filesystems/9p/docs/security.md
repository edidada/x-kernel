# v9fs — Security and Reliability Analysis

## Transport handoff boundary

A prepared KBox transport is transferred exactly once to the client. Mount
failure releases it through existing client/fid ownership. The caller must
allocate it before negotiation; this does not provide rollback of remote
operations already performed or change synchronous fid destruction requirements.


## Session-owner allocation failure

NoMemory from session-owner allocation drops the local session/client references
and root fid through their existing cleanup paths. No root is published locally;
remote negotiation has already happened and is not a transactional remote rollback.
Client, Arc and protocol buffer allocations retain their prior failure behavior.

## Trust Model

The analysis covers `src/` and the guest tests in `ut-tests/`. The extracted
metadata transport fixtures remain inside their original test modules under
`cfg(unittest)` and do not add a production transport or trust boundary.

Server metadata, qid, directory entries, directory offsets, symlink targets and file contents are all external input. v9fs
trusts KVFS to have already completed the pathname, permission, mount namespace and user-memory boundary checks;
PDU length, tag, type and field boundaries are validated by the `p9` client.

## External Boundaries / Attack Surface

- mount 时的 negotiation、root qid 和 attributes；
- lookup/readdir 返回的 object identity、type、name 和 offset；
- read/write、setattr、statfs 和 namespace operation 的 server result；
- the content and length a TREAD returns for a folio fault;
- the size confirmed by a `TSETATTR` truncate and its local cache publication;
- endpoint failure 与 synchronous fid Drop。

This crate does not directly access user pointers, DMA/MMIO, firmware, interrupts, FFI or architecture raw
interfaces. Transport and device safety reside in the specific provider/driver.

## Folio read validation

`read_folio` derives offset and length from the inode size and `folio.index()`
and trusts neither the requested nor the returned length. The client shortens
each TREAD to the negotiated msize/iounit, the loop advances only by the bytes
the server returned, and a zero-length reply before the folio window is covered
fails the fault with `EIO`. A failed folio stays non-uptodate, so a truncated or
malicious reply cannot publish a partially filled page.

The existing owned `FolioRead` token retains exclusive initialization access
through all fid operations. Both success and failure explicitly complete that
token after server I/O; success alone publishes uptodate state. A premature empty
reply after a filled prefix fails and unlocks the folio, so a subsequent fault
retries rather than exposing partial data or waiting on an abandoned lock.

## Truncate cache invalidation

A successful `TSETATTR` truncate is a backing-store size change made outside
the split `set_len` path. KVFS publishes the confirmed size with
`update_size_after_backing_change()`, which updates `i_size`, resizes cached
folios, and invalidates mappings past the new EOF. A failed or short remote
operation publishes nothing, so cached data cannot outlive the size the server
accepted.

## Unsafe Code Inventory

This crate has no `unsafe` blocks, unsafe APIs or custom `Send`/`Sync` implementations.

## Memory Safety Invariants

- Each `V9fsInode` holds a live, unopened fid and does not store an absolute path that becomes stale after rename;
- The typed private data of each opened `VfsFile` holds an independent opened fid;
- An inode is published only after the fid qid, attribute qid and VFS node type are consistent;
- Inode identity comes from the server qid, not from a fixed number or a directory array index;
- Namespace operations send only a single child name already resolved by KVFS;
- The directory resume position uses the server `d_off`;
- Automatic time updates modify only the local inode and do not issue additional remote metadata writes;
- When an explicit setattr fails, no modification is published; on success, the server-determined ctime is not passed off as a value confirmed by the client.

## Thread Safety

v9fs only shares `Arc<P9Client>`/`Arc<P9Fid>` and does not add a session mutex separate from the protocol state. The client
is internally responsible for request synchronization; inode, file and superblock each hold references that manage resource lifetime.
The Drop of the last fid reference may block, and must not be executed in IRQ context or while holding a spinlock.

## Threat Analysis

| Threat | Trigger Condition | Impact | Mitigation |
| --- | --- | --- | --- |
| stale path identity | rename 后按缓存 path 重新 walk | opened file 指向错误对象 | 只持 fid，不缓存 absolute path |
| type/qid spoofing | attributes 与 lookup fid 不一致 | 发布错误 inode 类型或身份 | qid path/type 与 mode type 交叉验证 |
| directory offset manipulation | server 返回异常 `d_off` | 重复或遗漏 entry | 原样使用 server offset，不伪造 local index |
| fid lifetime error | remove 后重复使用或 clunk | server handle misuse | `P9Fid` live state 与 `Arc` Drop |
| stale cached folio after truncate | `TSETATTR` shrinks the file but only updates the size | read/mmap keeps returning pre-truncate data | invalidate cached folios and mappings through `update_size_after_backing_change()` after a confirmed size change |
| broad fallback | server 返回真实 I/O/permission error | 错误被掩盖 | 只对 Linux 指定的 `ENOSYS`/`EOPNOTSUPP` fallback |

## Failure Mode and Effects Analysis (FMEA)

| Failure Mode | Local Effect | System Effect | Handling |
| --- | --- | --- | --- |
| negotiation/attach/root getattr 失败 | mount 失败 | superblock 不发布 | error propagation 和 RAII cleanup |
| lookup 不存在 | negative lookup | 单个 pathname 不存在 | 只把 `ENOENT` 转成 negative result |
| create 成功、child walk 失败 | local inode 不发布 | remote object 可能已存在 | 返回 error，不伪造 local state |
| statfs 不实现 | 无 remote statistics | mount 仍可用 | 仅 `ENOSYS` 回退 `simple_statfs` |
| truncate succeeds but cache publication fails | local size stays stale | later reads diverge from the server | propagate the `update_size_after_backing_change()` error without publishing partial state |
| final clunk 失败 | remote fid 暂时滞留 | 到 connection teardown 才回收 | `p9` warning |
| endpoint 永久不响应 | caller task 阻塞 | 同 client operations 排队 | 当前无 timeout/cancel |

## Fault Management

The range-fsync callback always contacts the existing open fid, including empty
ranges; it never substitutes local cache completion for server persistence.
The server receives the exact datasync intent and may synchronize more bytes
than requested because the protocol has no range field. Server errors propagate.

The typed `KError` of `p9` is propagated directly as a VFS error. Namespace/I/O failures do not use path-based retry;
unlink/rename use the protocol fallback only for `EOPNOTSUPP`, following Linux dotl semantics, and statfs falls back only on
`ENOSYS`.

## Privacy Analysis

File names, metadata, symlink targets and file contents are exposed to the selected 9P endpoint. v9fs provides no additional
encryption; the trust relationship is determined by the boot configuration, the transport and VMM/host isolation.

## Known Limitations

- 只实现 9P2000.L cacheless semantics；
- KVFS 没有 dentry-private fid slot，lookup fid 因而放在 `V9fsInode`；
- the page cache serves read faults only; no atomic-open contract, xattr or remote lock support;
- synchronous operations 没有 timeout/cancel。

## Audit Checklist

- Whether VFS operations use the existing inode/file fid rather than an absolute path;
- Whether an opened file obtains an independent fid and places it in typed private data;
- Whether namespace operations preserve the Linux fid role, fallback condition and wire order;
- Whether a new inode is published only after qid/attribute validation;
- Whether new fields correspond to Linux semantics or to a necessary boundary of KVFS/Rust ownership.

TFSYNC is a server persistence barrier and cannot certify local dirty mmap data.
The local `write_and_wait_range` must succeed first. Missing local writeback
support propagates its error without sending TFSYNC; the file's existing
writeback-error cursor retains its ordinary observation semantics.
