# posix-mm — Security and Reliability Analysis

## Trust Model

`posix-mm` trusts that:

- the architecture fault/syscall entry has already switched control flow into the kernel;
- the current `kprocess` process/thread state is available;
- the `kfs`, `memspace`, and `filemap` subsystems each maintain their own internal invariants.

It is itself responsible for:

- validating user-supplied arguments;
- translating raw syscall arguments into typed requests;
- correctly assembling anonymous file objects and fds;
- converting lower-level errors into syscall results.

## External Boundary / Attack Surface

- User-supplied arguments of `mmap` / `mremap` / `mprotect` / `munmap` /
  `madvise` / `msync` / `memfd_create`
- The memfd name and other syscall structures read through user pointers
- The current process fd table and address-space state

## unsafe Code Inventory

This crate contains no direct `unsafe` code blocks.

## Memory Safety Invariants

- Raw syscall flags must first be converted into a typed request and must not be passed directly into `MmSpace`
  or `filemap`.
- Unknown flags must explicitly return an error and must not be silently dropped through `from_bits_truncate()`.
- Linux-known deferred `mmap` flags must be handled within the `posix-mm` boundary:
  an ordinary mapping may pass under a compatible no-op policy, while `MAP_SHARED_VALIDATE` must return
  not-supported.
- `MAP_FIXED` / `MAP_FIXED_NOREPLACE` addresses must be aligned to the target page size.
- successful `mremap` move must not retire the source with ordinary
  `MmSpace::unmap()`.
- `munmap(addr, 0)` 必须失败，不能作为成功 no-op 穿透到 `MmSpace`。
- `PROT_GROWSDOWN` / `PROT_GROWSUP` 必须在 syscall 边界拒绝。
- `MS_ASYNC | MS_SYNC` 冲突组合必须拒绝；unknown `msync` flags 也必须拒绝。
- `MS_INVALIDATE` 必须返回不支持，直到 locked VMA 与 invalidate 语义存在。
- `posix-mm` 不得直接访问 pagecache 或 filemap runtime internals；`msync`
  必须通过捕获的 `LiveAddressSpace::msync_range()` 进入 MM，保留 active user；
  syscall 不持有 MM mutex 执行文件持久化。
- `mremap` move must preserve the ownership split between VMA metadata,
  present PTE residency, and backing object pages.
- The file object returned by `memfd_create` must hold a valid `Location`.
- The anonymous file `Location` must be bound to a regular-file inode.
- The contents of an anonymous file inode must be provided by an inode-owned `kvfs::AddressSpace`.
- `memfd_create` must create the anonymous file through the `memfs::shmem` factory; the syscall layer must not
  duplicate private tmpfs file creation, reopen an anonymous location, or bypass the inode-scoped
  shmem policy state.
- `MFD_ALLOW_SEALING` must only affect the initial seal set: when unset, the initial set is
  `F_SEAL_SEAL`; when set, the initial seal set is empty.
- Reading the `memfd` name must have a Linux-compatible upper bound and must not scan user memory without bound.
- Anonymous file objects should not depend on global path uniqueness.

## Thread Safety

- fd installation is synchronized through the process resource table.
- The `memfs::shmem` state, the VFS inode, and the `AddressSpace` are each protected by their own locks.
- `posix-mm` does not additionally cache raw pointers or unsynchronized shared state across syscalls.

## Threat Analysis

| Threat | Mitigation |
|------|------|
| A user pollutes the global path space through the `memfd` name | The `memfs::shmem` factory creates the anonymous file on a private mount |
| A user constructs a name containing `/` to affect VFS path semantics | A `/` in the Linux debug label is accepted and replaced with `:` only in the internal file name on the private mount; the user label is never parsed as a path |
| A user provides an unterminated or overlong memfd name | `load_string_with_max_len()` limits it to 249 bytes excluding NUL |
| A user enters an undefined path through unknown `memfd_create` flags | `MemfdFlags::from_bits()` rejects unknown bits |
| Raw `mmap` flags silently leak into MM internals | `MmapRequest` explicitly validates unknown/deferred flags |
| An unaligned fixed mmap overwrites the wrong address | The fixed policy rejects unaligned addresses at the request parsing stage |
| `munmap(addr, 0)` is treated as a successful no-op | `MunmapRequest` rejects zero length at the syscall boundary |
| `MADV_DONTNEED` damages a file source object | Currently only pure private anon and file-private anon result objects are handled |
| `msync` writes private COW pages back to the original file | `MmSpace::msync_range()` lets only the shared file runtime perform writeback |
| `MS_INVALIDATE` is faked as success | Currently returns `OperationNotSupported` |
| An `mremap` move failure leaves a half-completed source mapping, or the old source teardown destroys private backing that the destination still needs | All move branches uniformly use the `relocate_mapping` transaction; on preparation failure the source state is preserved and the destination is revoked, and on success only the source metadata is retired |

## Failure Modes and Effects Analysis (FMEA)

| Failure | Condition | Handling | Impact |
|---|---|---|---|
| 用户名字读取失败 | 无效用户指针、非法 UTF-8、缺少 NUL、超长 | 返回错误 | syscall 失败 |
| anonymous file 创建失败 | 内存不足或 VFS 分配失败 | 返回错误 | syscall 失败 |
| fd 安装失败 | fd table 满 | 返回错误 | 文件对象立即释放 |
| `mmap` flag 不支持 | unknown `MAP_*`、`MAP_SHARED_VALIDATE` 携带 deferred flag、或 file-backed hugetlb | 返回错误 | mmap 失败，不修改地址空间 |
| fixed mmap 地址未对齐 | `MAP_FIXED*` 地址不满足目标页大小 | 返回错误 | mmap 失败，不修改地址空间 |
| `munmap` 范围非法 | 长度为 0 或下游地址空间拒绝 | 返回错误 | 不修改地址空间 |
| `mprotect` grow flag 不支持 | `PROT_GROWSDOWN` 或 `PROT_GROWSUP` | 返回错误 | protect 失败，不修改 PTE/VMA |
| `MADV_DONTNEED` 范围非法 | 地址未映射、未页对齐、或 `addr + len` 溢出 | 返回错误 | advice 失败，不修改对象 |
| `msync` flag 不支持 | unknown flags 或 `MS_ASYNC | MS_SYNC` 组合 | 返回错误 | sync 失败，不修改对象 |
| shared file sync 失败 | 文件范围 datasync 的数据、元数据、journal 或设备持久化失败 | 传播文件系统/文件错误游标结果，未完成数据保留重试状态 | syscall 失败，调用者可重试 |

## Fault Management

- Argument and user-memory errors are returned through `KResult`.
- Ordinary syscall failures are not handled through panic.
- If fd installation fails after the anonymous file is created, the object is reclaimed naturally by the `Arc` lifetime.

## Known Limitations

- `memfd_create` 支持 `MFD_ALLOW_SEALING` 初始 seal policy；
  `F_ADD_SEALS` / `F_GET_SEALS` 由 `posix-fs` 处理，shared writable mmap
  seal enforcement 由 `mm/filemap` 处理，hugetlb 扩展未接入。
- `memfd` 名字仅用于调试，不保证与 Linux `/proc/self/fd` 展示完全一致。
- `madvise` 支持 `MADV_DONTNEED` 与四个性能提示（no-op，但仍要求范围完全
  映射并页对齐，未映射返回 `ENOMEM`）；`InvalidInput` 只在 advice 值未知、
  地址未对齐或范围溢出时返回。
- `mmap` 的 maximum permission 与 current permission 分离；shared file
  映射的 may-permission 受文件打开权限约束。
- `MS_SYNC` 已进入 shared file 范围 datasync；文件系统可保守同步更多范围。
  `MS_INVALIDATE` 和 locked-page `EBUSY` 语义未实现。
- dirty-PTE 收集与写回后重新写保护属于 Issue G；不能据此声称已经覆盖
  同一可写 PTE 上不再触发缺页的连续 mmap 写入。
- file-backed writable `MAP_SHARED` 由 `filemap` runtime 通过 write-fault
  dirty tracking 支持。

## Audit Checklist

- Does `uapps/memfd-test` check, through real syscalls, slash-label creation, mapped read/write,
  and content isolation between objects with the same formatted name; formatter string assertions cannot replace this check, nor can they
  alone prove the private mount topology.

- When a new syscall flag is added, are the typed request validation and the documentation updated together?
- Does the syscall first construct a typed request and only then call `MmSpace` / `filemap`?
- Does any `from_bits_truncate()` still silently swallow unknown ABI flags?
- Does fixed-address mapping still reject unaligned addresses?
- Does `memfd_create` still not depend on `/tmp` or any other global path?
- Does the shmem factory always return a regular-file inode on the private mount?
- Does the `memfd` regular-file inode always reuse the inode-owned `AddressSpace`?
- Does `MADV_DONTNEED` hit only private-anon objects and complete the PTE zap through the object-driven
  invalidate main path?

`brk` does not ignore errors from re-establishing/shrinking the mapping; on failure it does not publish the requested new heap top,
so that the user never observes an incomplete address-space change.

Shared file mapping creation and permission changes run under MM serialization.
Write faults may return a retry outcome to leave that critical section before a
lifecycle transition needs to revoke the mapping's PTEs.

`mremap` inherits the existing VMA permissions and runs under MM serialization.
File expansion and migration update registrations through the runtime open
callback rather than new-map admission. Fixed replacement removes the target,
trims a shrinking source, then resolves the remaining source and prepares its
move. These removals remain effective if a later preparation fails. The syscall
has no mapping-admission retry around this destructive sequence.
