# posix-fs - Security and Reliability Analysis

## Overview

`posix-fs` is the boundary layer where user-space filesystem syscalls enter the kernel VFS, the fd table and device objects.
The main risks come from user pointers and path strings, file descriptor reuse, VFS magic-link follow,
the directory entry ABI layout, metadata permission semantics, mount flags compatibility and fd-to-fd data copying.

fd-backed kernel objects whose core is not filesystem state, such as `timerfd` and `eventfd`,
have been migrated to the standalone owner crate `kfd_objects`; this crate no longer bears responsibility for their state machines or security audit.

This crate contains a small amount of Rust `unsafe` code,
all of it used to construct Linux ABI structures, not to bypass VFS or fd table permissions.

## Trust Model

```text
user process
  │
  │ untrusted syscall arguments:
  │   path pointers, buffers, iovecs, fd numbers, flags, modes, offsets
  v
┌──────────────────────────────────────────────────────────┐
│ posix-fs                                                 │
│                                                          │
│ trust boundary                                           │
│  ├─ validates ABI flags and scalar ranges where needed    │
│  ├─ copies user strings/buffers through posix_types/osvm  │
│  ├─ resolves fd/path against current process context      │
│  ├─ delegates object semantics to kfd/kvfs/kfd_objects           │
│  └─ maps failures to KError/KResult                       │
└──────────────────────────────────────────────────────────┘
  │
  v
kfd resources / kvfs / device and pipe implementations
```

- User-space arguments are untrusted, including null pointers, invalid addresses, over-long paths, malicious flags, negative offsets and fd reuse.
- `posix-fs` trusts `posix_types` / `osvm` to perform bounds checks and error propagation on user addresses.
- `posix-fs` trusts `kfd` to remain concurrency-safe in fd lookup, duplication, closing and descriptor flags.
- `posix-fs` trusts `kvfs` / `kfd_objects` / device objects to perform the actual reads and writes and to implement the permission, directory and mount semantics.
- The current process context, the fd table, `FsStruct` and the user address space must come from the thread currently executing the syscall.

## External Boundaries / Attack Surface

| Boundary | Entry examples | Risk source |
|------|----------|----------|
| User path strings | `openat`, `linkat`, `statx`, `mount` | Null pointers, strings not NUL-terminated, over-long strings, path traversal, symbolic link loops |
| mount data byte page | `mount(..., data)` | Bad pointers, crossing into unreadable pages, binary content, and malicious or unknown options interpreted by the specific filesystem |
| User read/write buffers | `read`, `write`, `readlinkat`, `getdents64`, `statfs` | Bad addresses, short buffers, cross-page access failures, malformed kernel information written back |
| xattr names and values | `set/get/list/remove*xattr` | Missing NUL terminator, over-long name/value/list, non-UTF-8 suffix, short output buffers, namespace privilege violations |
| User iovec | `readv`, `writev`, `preadv2`, `pwritev2` | Too many iovecs, range overflow, wrong read/write direction |
| fd numbers | Almost all fd syscalls | Closed fds, type mismatch, fd reuse, confusion of `CLOEXEC`/non-blocking flags |
| Current process state | `chdir`, `openat`, `close_range` | Process resource lock, fs_struct updates, umask and current directory semantics |
| VFS magic link | `/proc/self/fd/<fd>`, `/proc/<pid>/fd/<fd>` | Cross-process fd access, target process exit, concurrent fd table changes, no-follow policy |
| VFS/device objects | `open`, `ioctl`, `mount`, `syncfs` | Device-specific ioctls, terminal objects, mount flags, differences between filesystem implementations |
| FIEMAP variable-length output | `ioctl(FS_IOC_FIEMAP)` | User-declared extent count, address of the array after the header, block mappings returned by the backend |
| fd-to-fd copying | `sendfile`, `copy_file_range`, `splice` | Wrong source/destination types, offset pointer TOCTOU, overlap within the same file, blocking semantics |

This crate does not directly access MMIO, PIO, DMA, firmware tables or architecture inline assembly.

## unsafe Code Inventory

| Location | Call path | Purpose | Invariant / safety rationale |
|------|----------|------|-------------------|
| `dir.rs::DirBuffer::write_entry` | `sys_getdents64 -> iterate_dir sink -> DirBuffer::write_entry` | Write directory entries into a temporary `Vec<u8>` according to the `linux_dirent64` ABI | The record length is computed and `remaining_space >= len` is checked before writing; `entry_ptr` lies within the allocated range of the `Vec`; the record length is rounded up to `align_of::<linux_dirent64>()`, but the byte buffer does not guarantee struct alignment at arbitrary offsets, so the header is written with `write_unaligned`; the name copy length comes from `name.as_bytes().len()`; a trailing NUL is written; after writing, `offset` only advances within the checked range. |
| `UserRead`/`UserWrite` implementations of `ioctl.rs::FiemapHeader` | `sys_ioctl(FS_IOC_FIEMAP)` | Copy the Linux FIEMAP header | The `repr(C)` structure contains only integers, the trailing explicit `reserved` covers the ABI alignment, and any bit pattern is valid; every byte belongs to an initialized field |
| `UserWrite` implementation of `ioctl.rs::FiemapExtent` | FIEMAP writer output | Copy a Linux FIEMAP extent | All ABI reserved regions of the `repr(C)` structure are covered by explicit integer arrays and zeroed on every output, so there is no uninitialized padding |
| `ioctl.rs::Fsxattr` 的 `UserRead`/`UserWrite` 实现 | 文件属性 ioctl | 复制文件属性 ABI | `repr(C)` 的整数和显式字节数组覆盖全部字段；输出时保留字段清零 |
| `notify.rs::entry_notify_wanted` (`transmute::<usize, WatchProbe>`) | `open`/`mkdir`/`mknod`/`symlink`/`link`/`unlink`/`rmdir`/`rename` entry, the "no watch anywhere" probe | Restores the installed `fn` pointer from its integer form | Only `set_watch_probe` writes the static, storing `probe as usize` -- the address of a `fn` item whose signature matches exactly -- so every non-zero value is such a code address; `0` is the "not installed" sentinel and is excluded before the cast; the `Release` store pairs with the `Acquire` load. Several syscall entries may install concurrently, but each stores the same typed value, so the invariant rests on the typed setter parameter, the sentinel and the ordering, not on a single writer. Failure consequence: if that invariant is broken the call jumps to a non-function address (UB), so no other path may write the static |
| `notify.rs::notify_entry` (`transmute::<usize, EntryNotifier>`) | `notify_entry_of_path` / `notify_entry`, directory-entry events | Restores the entry notifier the same way | Same argument: the only writer is `set_entry_notifier`, the signature matches the cast type, the `0` sentinel is excluded first, and the store/load pair is `Release`/`Acquire` |
| `notify.rs::notify_modified` (`transmute::<usize, ContentNotifier>`) | successful `write`/`writev`/positioned writes, `sendfile`/`copy_file_range`/`splice` with positive progress, `open(O_TRUNC)`, `truncate`/`ftruncate`, and `fallocate` -> `notify_modified` | Restores the installed content-event router | Only `set_content_notifier` writes `CONTENT`, storing a `ContentNotifier` function pointer as `usize`; the signature matches the cast type, `0` is the "not installed" sentinel and is excluded before the cast; the `Release` store pairs with the `Acquire` load. Concurrent installers store the same typed value. Failure consequence: if that invariant is broken the call jumps to a non-function address (UB), so no other path may write the static |
| `notify.rs::notify_self_object` (`transmute::<usize, SelfNotifier>`) | `link`'s `IN_ATTRIB`, self-events of removal and rename | Restores the self-event notifier | Same argument: the only writer is `set_self_notifier` |
| `notify.rs::retire_object` (`transmute::<usize, RetireNotifier>`) | retirement of an object's watches when it disappears | Restores the retirement notifier | Same argument: the only writer is `set_retire_notifier` |



A newly added `unsafe` block must satisfy all of the following:

- It is preceded by a `SAFETY:` comment;
- This inventory records the call path, the invariant being protected and the failure consequence;
- It does not pass user pointers raw to the underlying objects;
- It does not construct object references in a way that bypasses VFS/fd table permission checks.

### getcwd User Output Boundary

A NULL output address in `getcwd` is not a query mode; after the path and capacity checks are complete,
all actual output goes through `UserPtr::write_vm_slice`, and a bad address returns `EFAULT`.
The capacity is received as a `usize`, but only the actual path length is copied, so a huge capacity does not trigger an allocation of that size.
A short capacity returns `ERANGE` before any write, including when a NULL address and a short capacity occur together.
The regression tests use a separate process's `FsStruct` and mappings managed by `TestUserArray`,
and check that a short buffer is not written, that normal output does not go past the end of the path, and the error return for an unmapped address.

## Memory Safety Invariants

1. **User memory is accessed only through wrapper types**:
   Syscall entries read and write user addresses using `UserConstPtr`, `UserPtr`, `VmBytes`, `VmBytesMut`
   and `IoVectorBuf`, and never dereference user pointers directly.
2. **User output is first built in a temporary kernel buffer**:
   `getdents64` first writes into a `DirBuffer`, then copies it to user space through `write_vm_slice`.
3. **fd objects are held through `Arc<dyn FileLike>`**:
   An object obtained from fd lookup is kept alive by its reference count for the duration of the current operation.
4. **Type-dependent operations downcast and validate first**:
   Paths such as `ftruncate`, `fstatfs`, positional I/O and directory seek first confirm that the object is a `File` or
   `Directory`; pipe-specific operations confirm that a `PipeObject` exists in `VfsFile::private_data`,
   and a type mismatch returns an error.
5. **Ranges and overflow must be handled explicitly**:
   `fallocate` returns `InvalidInput` for negative offset/len and for `offset + len` overflow;
   the `lseek` directory offset uses `checked_add_signed`;
   positional I/O rejects negative offsets;
   fd-to-fd copying has the VFS update the kernel-side offset, which is then written back through `UserPtr`.
6. **Temporary ABI structures must not leak uninitialized memory**:
   `statfs` explicitly initializes all fields with a safe struct literal, and `f_spare` is zero;
   the temporary `Vec` of `getdents64` starts zeroed, and directory entry records explicitly write the trailing NUL.
7. **FIEMAP variable-length output must use checked arithmetic**:
   `fm_extent_count` is first bounded by the Linux UAPI total byte limit, and the header address, the array byte count and each entry's
   offset all use `checked_add`/`checked_mul`; the reserved fields of every output extent are explicitly zeroed.
8. **mount data may be passed down only as a bounded kernel byte page**:
   The syscall entry copies one opaque buffer of 4 KiB base-page size and forcibly zeroes the last byte, imposing no UTF-8
   or early-NUL constraint; `FsContext` does not store user pointers, and if a filesystem wants to retain
   options after `get_tree()` returns, it must copy only the parsed state.

### fchdir descriptor boundary

`fchdir` performs an fd table lookup for every input; negative values (including `AT_FDCWD`) and closed fds
return `EBADF`. A non-directory returns `ENOTDIR`; for a directory, execution continues with the search permission check of the current credentials.
The cwd is written only after full validation succeeds; the special base-directory markers of path syscalls are not treated as valid fds.

## Permission and semantic invariants

Read and write syscalls restart on an interrupt-request signal only when waiting on recognized files, and the conversion must happen after the completed amount is aggregated.
Not every EINTR may be upgraded, and the conversion must not be applied unconditionally in the low-level helpers shared by copy-type syscalls;
otherwise output that has already been committed could cause the entire request to be replayed. A positive completed amount and ordinary errors must not be overwritten.

A copy request first commits the actual completed amount, and only then writes back the user offset; an EFAULT on write-back may coexist with the already output
data, and the request must not be replayed on that basis. The read-ahead position of the source file is not published directly, and a pipe source keeps the unaccepted
suffix. Position lock ordering and pipe consumption are managed uniformly by KVFS, and syscalls do not hold these locks while writing back to user memory.
A zero completed amount does not always exempt the write-back: a sendfile transfer error, as well as EOF in sendfile/splice,
may still return EFAULT because the offset is not writable; copy_file_range writes back only after a positive completion.

1. `O_CLOEXEC`、`FD_CLOEXEC` 和 `close_range(CLOEXEC)` 必须只影响 descriptor flags，
   不应改变底层 `FileLike` 对象。`close_range` 在 syscall 边界验证无符号范围后
   才转换到有符号 fd 编号；接受 `UINT_MAX` 不能使空范围跳过请求的文件表隔离。
2. `O_NONBLOCK` 和 `F_SETFL(O_NONBLOCK)` 必须修改 open file description 的
   `VfsFile::f_flags`，`nonblocking()` 只能从该字段派生。
3. `AT_EMPTY_PATH` 是空路径解析的必要条件；只有实际的空字符串与
   `AT_FDCWD` 组合才能选择持有引用的 cwd，其余情况查 fd 表。
   内部 `None` 形式保持仅按 fd 解析，避免 `fstat/fchmod/fchown/f*xattr`
   复用解析器时意外获得访问或修改 cwd 的能力。
4. 非空绝对 `*at` pathname 必须忽略 `dirfd`；只有相对 pathname 才允许
   `dirfd` 查找失败影响结果。
5. `AT_SYMLINK_NOFOLLOW` / `O_NOFOLLOW` 必须影响符号链接和 VFS magic-link follow。
6. `mount` 不得静默忽略会改变语义的 unsupported operation flags。
7. 普通新挂载只能精确查找已注册的 filesystem type；syscall 层不得维护与
   `/proc/filesystems` 分离的构造分支。
8. 创建文件和目录时应应用 `FsStruct` 中的当前 umask；写入 umask 必须截断为
   `0777`，`CLONE_FS` 必须与 root/pwd 一起共享它。
9. A `chroot` target must be a directory, and the caller must hold effective
   `CAP_SYS_CHROOT`.
10. `chown/chmod/utimensat` 必须把同一个 credential snapshot 传给 VFS metadata
   授权，不能直接调用后端 `setattr`。
11. `F_ADD_SEALS` 只能单调添加 memfd seals，且必须尊重 `F_SEAL_SEAL`。
12. Xattr 输入 name/value 必须先复制到有上限的内核缓冲区；list name sink 的累计长度不得
    超过 `XATTR_LIST_MAX`。`size == 0` 查询不得访问输出指针或物化名称序列，非零短缓冲区
    必须返回 `ERANGE`，raw set flags 不得进入 KVFS。
13. `mount`/`umount2` 必须在改变 mount namespace 状态之前用 syscall 入口捕获的那一份
    credential snapshot 校验 mount privilege；未授权调用在 mount point 状态校验之前返回
    `EPERM`，与 Linux `may_mount()` 的 `CAP_SYS_ADMIN` 门禁对应。
14. 挂载授权只发生在 syscall 边界：`kvfs::MntNamespace` 的 `mount_new`、`attach_bind`、
    `remount`、`reconfigure_mount` 和 `detach` 保持显式调用契约，不引入隐式
    current-process 查询；内核内部调用方自行保证已获得授权。

### readlink length input boundary

The user-supplied `readlink` / `readlinkat` length is interpreted according to the Linux syscall `int` ABI,
rather than treating the whole register directly as a `usize`. A non-positive value returns `EINVAL`, without reading the pathname
or writing the output buffer; a positive-value copy still uses `UserPtr::write_vm_slice` and does not introduce raw pointer access.

### openat2 input validation

`sys_openat2` copies and validates the user `struct open_how` before it reads
the pathname or resolves the directory fd, so malformed requests fail with the
argument error rather than a later `EFAULT`/`EBADF`:

- `read_open_how`: `size < OPEN_HOW_SIZE_VER0` (24) is `EINVAL`; `size > PATH_MAX`
  is `E2BIG`; any non-zero byte past the known fields is `E2BIG`
  (`copy_struct_from_user` semantics).
- `Openat2How::validate` (kvfs `OpenHow::from_openat2`): unknown `flags` or
  `resolve` bits are `EINVAL`; `mode` bits outside `0o7777` are `EINVAL`; a
  non-zero `mode` without `O_CREAT` is `EINVAL`; `RESOLVE_BENEATH |
  RESOLVE_IN_ROOT` is `EINVAL`; `OPENAT2_REGULAR` with `O_PATH`/`O_DIRECTORY` is
  `EINVAL`; contradictory `O_DIRECTORY|O_CREAT` / invalid `O_PATH` combinations
  are `EINVAL` before the `RESOLVE_CACHED` rule; `RESOLVE_CACHED` with
  `O_CREAT`/`O_TRUNC` is `EAGAIN`.
- The pathname is bounded to `PATH_MAX - 1` payload bytes; a longer or
  unterminated name is `ENAMETOOLONG`, invalid UTF-8 is `EILSEQ`.
- `RESOLVE_BENEATH` with an absolute pathname is `EXDEV` before the dirfd is
  consulted.

## Thread safety

The metadata and mount ID of `statx` come from the same target held after a single resolution; when obtaining the identity,
the user path is not read again. The newly added field returns the existing integer mount ID, does not expose kernel pointers, and does not change
path authorization. Errors from resolution, getattr and user write-back propagate along the original `KResult` path, and the complete metadata is finally
written back through the existing `UserPtr::write_vm`.

The reserved request bits and mutually exclusive sync flags of `statx` are rejected before the user pathname is accessed and the target is looked up; an illegal request must not be reported as successful because a parameter was
silently ignored; these failures do not write the output buffer. Other unknown mask
bits are not rejected outright, in order to preserve the extensibility of Linux query requests.

| Resource | Concurrency condition | Risk |
|------|----------|------|
| fd table | Protected by the internal lock of the current process resources | A close/dup/lookup race can cause fd reuse or object leaks |
| `FsStruct` | Accessed through the process fs_struct lock | `chdir`, `chroot`, umask and relative path resolution run concurrently; `CLONE_FS` shares the same object |
| Directory offset | Protected by the `Directory::offset` lock | `getdents64` and directory `lseek` update it concurrently |
| `FileLike` object | Handled by the specific file, pipe or device implementation | Blocking and non-blocking state and offset update semantics |
| procfs magic-link target | The procfs/kfd snapshot holds a reference to the target `FileLike` | Lifetime when the target process exits or the fd is closed |

`posix-fs` does not define a global lock order across resources.
New code should shorten lock-holding intervals,
and avoid calling complex VFS/device operations that may access the same resource again while holding the directory offset or fd table lock.

## Threat Analysis

| ID | Threat Description | Boundary | Impact Level | Trigger Condition | Mitigation |
|------|----------|------|----------|----------|----------|
| T-01 | 用户坏指针导致内核越界读写 | 用户路径/缓冲区/iovec | 高 | 直接解引用用户地址或忽略 copy 错误 | 统一使用 `UserPtr`、`UserConstPtr`、`VmBytes`、`IoVectorBuf` 并传播 `KResult` |
| T-02 | `getdents64` 写出目录项时越过临时缓冲区 | 用户输出缓冲区 / ABI 布局 | 高 | 记录长度计算错误或未检查剩余空间 | `DirBuffer::write_entry` 先检查空间，记录长度按 ABI 对齐 |
| T-03 | `statfs` 向用户泄露未初始化内核栈数据 | 用户输出缓冲区 / ABI 布局 | 高 | C ABI 结构 padding 未初始化 | `statfs` 用结构体字面量初始化所有字段及 `f_spare`；四个受支持的 64 位 ABI 布局没有隐式 padding，ABI 变更时须重新核对布局和 copyout |
| T-04 | `/proc/<pid>/fd/<fd>` magic-link 绕过普通路径解析访问不该访问的对象 | VFS magic-link | 高 | procfs follow 未保持 fd 生命周期或缺少权限模型 | procfs fd entry 通过 fd snapshot 持有目标对象；非 VFS 目标 follow 返回不支持；跨进程权限策略仍需收紧 |
| T-05 | `O_NOFOLLOW` 被忽略导致符号链接或 magic-link 被跟随 | 路径 flags | 高 | open/at flags 没有传入解析层 | syscall 层统一把 flags 转成 `LookupFlags`，`kvfs::namei` 按 `LookupIntent` 处理 final/non-final symlink 与 magic-link |
| T-06 | `fallocate` offset/len 溢出破坏文件大小或范围操作 | scalar 参数 / VFS | 高 | 负数或 `offset + len` 溢出 | 校验非负并用 `checked_add` |
| T-07 | `copy_file_range` 同文件重叠复制造成数据破坏 | fd-to-fd 复制 | 中 | 同文件重叠检查未实现 | 已在设计文档列为限制；非零 flags 在边界被拒绝；当前不宣称等价 Linux |
| T-08 | mount flags 被错误分层、类型分派漂移或 unsupported 语义被静默忽略 | mount flags / filesystem type | 高 | 用户态把 move、shared/slave/unbindable propagation 或 recursive-bind bit 与 bind/remount/new mount 组合，或 syscall/procfs 各自维护支持集合 | `path_mount` 在任何 operation dispatch 前统一拒绝未实现位，再独立生成 superblock/per-mount flags；`MS_PRIVATE[|MS_REC]` 仅对 mount root 成功，因为 KVFS mount tree 天生不传播；初次 bind 按 Linux 继承源 flags 并忽略普通请求位，bind remount 才替换 flags；普通 remount 仅默认保留 atime mask；新挂载和 `/proc/filesystems` 共用 KVFS registry |
| T-09 | 文件锁冲突或关闭竞态破坏应用互斥 | fd_ops / KVFS | 中 | owner、区间或关闭释放错误 | inode 锁上下文串行化区间更新；阻塞等待支持信号取消和 POSIX 死锁检测；关闭按 files owner 清理，加锁后复查 fd |
| T-10 | 创建文件使用错误所有者导致 DAC 语义错误 | open/create | 高 | 固定 root owner，或忽略 setgid 父目录 | syscall 传递当前 `Cred`，后端用 `inode_init_owner()` 初始化 fsuid/fsgid 与继承组 |
| T-11 | `faccessat2` 权限检查使用错误身份 | access | 高 | 默认错误使用 filesystem IDs，或 `AT_EACCESS` 强行覆盖显式 fs ID | 默认构造 real-ID credential；`AT_EACCESS` 直接使用当前 credential 的 `fsuid/fsgid`，并用于完整遍历与最终检查 |
| T-12 | 设备 ioctl 参数被错误解释 | ioctl / 设备对象 | 中 | syscall 层错误处理设备私有命令 | `FIONBIO` 在本层处理，其它命令转交 `FileLike::ioctl`；常见 isatty 探测错误不刷 warning |
| T-13 | memfd seals 被非 shmem fd 或普通文件伪造 | fcntl / shmem | 中 | `F_ADD_SEALS` / `F_GET_SEALS` 没有校验 fd object 类型和 inode state | fcntl 先取得 `kvfs::VfsFile`，再由 shmem inode state 判断是否存在 |
| T-14 | fcntl 复制忽略最低编号或越过软限额 | fd_ops / kfd | 中 | `F_DUPFD*` 的 minimum 为负、超限或范围已满 | 先校验源 FD，再按 Linux signed-int minimum 校验并在软限额和物理容量内分配；失败不改变已有槽位和 CLOEXEC。 |
| T-14 | 同一 pathname 操作混用多个凭据快照 | syscall/VFS | 高 | 路径组件逐次查询 current task，期间 credential 被提交 | syscall 入口只取得一次 `Arc<Cred>` 并沿完整操作传递 |
| T-15 | `ftruncate` 在 UID 改变后错误重做 pathname DAC | fd/VFS | 中 | fd 操作调用 `Path::truncate(cred)` | `VfsFile::truncate()` 只验证 open write mode，再使用 opened-file authority |
| T-16 | 非 owner 绕过 VFS 直接修改 mode、owner 或时间 | metadata | 高 | syscall 直接调用后端 `setattr` | syscall 只做 ABI 转换，`Path::chown/chmod/set_times` 统一执行 owner、group 和 write authorization |
| T-17 | Unprivileged user changes the process root | chroot | High | Only target-directory searchability is checked | After directory DAC, additionally require effective `CAP_SYS_CHROOT` |
| T-18 | Unprivileged user creates a device node through `mknodat` | namespace / device | High | The special inode is created from the mode alone | Character/block devices require effective `CAP_MKNOD`; other node types still apply umask and VFS directory authorization |
| T-19 | 绝对 `*at` 路径错误访问无效 `dirfd` | pathname / fd | 中 | 在判断 pathname 是否绝对之前解析 `dirfd` | 所有非空 pathname 入口复用 `with_fs_at`；绝对路径强制选择进程 cwd snapshot 并由 KVFS 从 root 开始 |
| T-20 | `mknodat` 锁外授权与最终名称状态竞争 | namespace / device | 高 | syscall 先做特权/DAC，再由 VFS 查找或创建 | syscall 只转换 ABI；KVFS 在父目录 exclusive lock 内按 positive-first 顺序执行最终 lookup、授权、mode 准备和 callback |
| T-21 | 非法 mknod 类型被无效 dirfd 错误遮蔽 | syscall / fd | 中 | 进入 `with_fs_at` 后才执行 `may_mknod` | 复制 pathname 后立即调用 namei 层 `may_mknod()`，验证成功后才允许解析相对路径的 `dirfd` |
| T-22 | umask 出现两份 owner 或被高位污染 | process / fs context | 高 | process runtime 与 FsStruct 各保存一份，或 `sys_umask` 未截断 | `FsStruct` 是唯一 owner，`replace_umask` 统一执行 `mask & 0777`；fork/clone 与 pathname snapshot 都复制或共享该对象 |
| T-23 | namespace syscall 对 final name 做两次 lookup 并操作同名替代对象 | pathname / namespace | 高 | syscall 先解析完整目标或锁外确认目标不存在，再由 Path 按名称重新查找 | link/symlink/unlink/rmdir 调用专用 `Filename::*_at`；Dentry 在父目录 exclusive lock 下执行唯一 final lookup |
| T-24 | `linkat` 默认错误跟随 source symlink 或静默接受未知 flags | pathname flags | 中 | 通用 resolve helper 默认 follow，或 syscall 只记录 warning | syscall 只接受 `AT_SYMLINK_FOLLOW | AT_EMPTY_PATH`；默认显式使用 no-follow，未知位返回 `EINVAL` |
| T-25 | FIFO open 根据第一次错误重新解析 pathname | pathname/open | 高 | 两次 lookup 之间目标被 rename 或 symlink replacement | syscall open 只调用一次 `Filename::open_with_flags_at`；FIFO dispatch 在 KVFS 已授权的 resolved inode 上完成 |
| T-26 | FIEMAP 数量或地址计算溢出导致越界写或内核数据泄漏 | ioctl / 用户输出 | 高 | 信任 `fm_extent_count`、用未检查指针运算或复制未初始化 reserved 字段 | 限制最大数量，所有地址运算使用 checked arithmetic，输出结构显式初始化全部字段；用户地址只通过 `UserPtr` 写入 |
| T-27 | `listxattr` 的名称聚合溢出或 size query 产生无界中间分配 | xattr / 用户输出 | 中 | 先收集属性和值，再构造多个完整名称副本，或累计长度未检查 | KVFS borrowed-name sink 直接写入单个 `XattrListWriter`；逐项 checked_add 并限制 `XATTR_LIST_MAX`，`size == 0` 只计数 |
| T-28 | mount data 导致无界用户内存读取、错误拒绝 binary data 或把瞬时用户指针带入文件系统 | mount data / filesystem type | 高 | 通用层把 `void *` 当路径字符串解析，或直接向后端保留用户指针 | syscall 边界复制一个零填充的 4 KiB opaque byte page，并强制清零末字节；`FsContext` 只借用内核 slice，后端只保存解析后的 mount state |

Impact level definitions:

- High: may lead to memory corruption, kernel information leaks, privilege escalation, or unauthorized access.
- Medium: may lead to data corruption, incorrect compatibility semantics, service unavailability, or broken application synchronization.
- Low: mainly leads to imprecise error codes, performance degradation, or log noise.

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure Mode | Cause | Local Effect | System Effect | Severity | Mitigation |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | Path read failure | The user pointer is invalid or the string is inaccessible | The syscall returns an address-related `KError` | The current operation fails | 3 | A `load_string` failure is propagated directly |
| F-02 | fd type mismatch | A file write on a directory, or a pipe operation on a non-pipe | Returns `BadFileDescriptor`, `InvalidInput`, or an underlying error | The application receives an error equivalent to the Linux errno | 3 | Use `get_file` followed by object type checks (for example `is_dir`) and downcast validation |
| F-03 | User output buffer too small | `getdents64` cannot fit even one record, or the `getcwd` size is insufficient | Returns `InvalidInput` or `OutOfRange` | The caller can retry with a larger buffer | 4 | Compute the length before writing |
| F-04 | Device open failure | The device file operations reject the open or fail to initialize | open returns an error | Terminal-related programs cannot open the target device | 3 | The error is propagated; device semantics are maintained by the corresponding device implementation |
| F-05 | The backend zero-fill write of `fallocate` returns 0 | The underlying filesystem cannot advance the write | Returns `WriteZero` | The operation fails, but the file must not loop forever | 2 | `write_zeros_range` detects a 0-byte write |
| F-06 | `sendfile`/`splice` encounters `WouldBlock` | A non-blocking source temporarily has no data | If part has been written, partial progress is returned; otherwise an error is returned | The caller can poll and retry | 4 | `kvfs::transfer_files` preserves partial-write semantics |
| F-07 | `mount` requests an unknown filesystem type or an invalid backing source | The name is not registered, or the source is not a usable block-special path | Returns the corresponding error such as `NoSuchDevice`, `ENOTBLK`, or `ENXIO` | User-space mount fails | 3 | Exact lookup in the KVFS registry; nodev/device-backed types all go through `FsContext`, and the device source is validated by `get_tree_bdev` |
| F-08 | The `syncfs` target is not a file or directory | The fd refers to a pipe/socket/device | Returns `InvalidInput` | The current sync request fails | 4 | Only flush filesystem objects after the downcast |
| F-09 | `copy_file_range` semantics are incomplete | The overlap and regular-file checks are TODO | Data results inconsistent with Linux are possible | Copy behavior of related applications is abnormal | 2 | Non-zero flags are explicitly rejected; the other restrictions need additional tests before implementation |
| F-10 | `fcntl` unsupported cmd returns success | Compatibility placeholder | The application wrongly concludes that some control operations have taken effect | Behavioral differences may result | 2 | A warning is logged; commands with security impact should be explicitly implemented or rejected |
| F-11 | `close_range(UNSHARE)` cannot obtain the files owner | The process has exited and has detached from the fd table owner | `unshare_fd_table` returns `NoSuchProcess` | The current syscall fails, and the detached fd table is not reinstalled | 3 | `sys_close_range` propagates the resource-layer error as a user-space syscall error through `?` |
| F-12 | FIEMAP output capacity is insufficient or a bad user page is encountered midway | The caller provides a small array or an unwritable address | Returns the count already collected or `BadAddress` | The current query fails, and the filesystem state is unchanged | 3 | `FiemapExtentInfo` stops normally once capacity is reached; the writer writes each entry through `UserPtr` and propagates copy faults |
| F-13 | Mount data copy failure | The first byte is unreadable, or the whole-page read fails and the byte-by-byte recovery also cannot read the first byte | `mount(2)` returns `BadAddress` before the superblock is created | The current mount does not happen and no partial topology is left behind | 3 | Try a whole-page copy first; on failure, recover the readable prefix and zero-fill the tail; the error is propagated only when zero bytes are readable |

Severity definitions:

- 1: Fatal; may cause a kernel crash, memory corruption, or unrecoverable data corruption.
- 2: Serious; functionality is unavailable, semantics are wrong, or a reboot/manual recovery is required.
- 3: Moderate; a single syscall fails or the application can degrade gracefully.
- 4: Minor; the caller can retry or the impact is limited.

## Fault Management

- All syscall entry points return `KResult<isize>`,
  and errors are mapped to Linux errno by the upper layer of `core/ksyscall`.
- Bad fds, bad paths, illegal flags, and unsupported modes preferentially return an explicit `KError`.
- I/O errors such as `WouldBlock`, `WriteZero`, and `BrokenPipe` are propagated by the underlying object.
- For common probing ioctl failures, for example `TCGETS` / `TIOCGWINSZ` on a non-terminal fd,
  no warning is logged, to avoid log noise.
- Unsupported `fcntl` arguments currently log a warning but return success;
  this is a compatibility placeholder and must not be extended to new commands with security impact. `F_ADD_SEALS` and
  `F_GET_SEALS` are already explicitly wired to the shmem object state.
- This crate has no unified retry mechanism;
  user space or the upper-level syscall scheduler is responsible for deciding whether to retry based on errno.

## Privacy Analysis

`posix-fs` handles pathnames, directory entry names, file content buffers, fd numbers, mount points, and device control parameters.
The module itself does not persist this data,
but debug logs record some paths, fds, flags, modes, and ioctl cmds.
New logging should avoid emitting file contents, user buffer data, or sensitive cross-process path relationships.

`read`, `write`, `sendfile`, `copy_file_range`, and `splice`
move user file contents within the kernel,
but the contents are not parsed by this crate or stored in global state.

## Known Limitations

1. Credential DAC uses effective capabilities and Linux mode semantics, but
   LSM, ACL, user-namespace ID mapping, and idmapped mounts are not implemented.
2. FAT 等不能表达 Unix owner 的后端无法完整保存创建者 UID/GID。
3. 文件锁是 advisory 锁；不阻止未参与加锁的读写，不实现 lease 或网络锁协议。
4. `copy_file_range` regular-file type, same-inode overlap and cross-superblock admission checks are performed by the KVFS transfer layer; there is no zero-copy path.
5. `mount` 支持已注册 nodev filesystem、已接入 `get_tree_bdev()` 的 block-backed type、
   非递归 bind、普通只读 remount、bind remount，以及普通新挂载的一页 opaque mount data
   转交；支持对 mount root 幂等执行 `MS_PRIVATE[|MS_REC]` 和 `MS_SLAVE[|MS_REC]`（无共享
   传播组时两者行为等价），支持 `pivot_root` 与 `umount2(MNT_DETACH)`；不支持 move、
   带挂载子树的 recursive bind（`MS_BIND|MS_REC` 仅在源子树无挂载时接受，否则返回
   `EINVAL`）、shared/unbindable propagation、文件系统专用 reconfigure 和 force
   unmount。通用层支持 binary data 转交不等于具体 filesystem 已实现相应格式或选项。
6. `preadv2` / `pwritev2` 当前只支持 `flags == 0`，
   非零 flags 返回 `Unsupported`。
7. memfd `F_ADD_SEALS` / `F_GET_SEALS` 已接入；shared writable mmap 和
   `mprotect(PROT_WRITE)` seal enforcement 由 `mm/filemap` 执行。
8. POSIX ACLs and LSMs are not implemented; the final security policy for
   `security.*`/`system.*` is constrained by this gap. KExt4 currently exposes
   only ordinary `user.*`, `trusted.*`, and `security.*` xattrs, and KVFS
   requires effective `CAP_SYS_ADMIN` to set/remove `security.*` until complete
   concrete LSM hooks are connected.

## Audit Checklist

When modifying `posix-fs`, verify:

- [ ] New user pointer accesses go only through `UserPtr`, `UserConstPtr`, `VmBytes`, or `IoVectorBuf`.
- [ ] New `unsafe` blocks have a `SAFETY:` comment and are added to the unsafe inventory in this document.
- [ ] New path resolution entry points correctly handle absolute paths ignoring `dirfd`, `AT_EMPTY_PATH`,
  `AT_SYMLINK_NOFOLLOW`, and `AT_FDCWD`.
- [ ] New open/stat/namei entry points make explicit the difference between ordinary paths and procfd paths.
- [ ] A failed fd table modification does not leave a half-created object or wrong descriptor flags.
- [ ] Paths involving offset/len check negative values, overflow, and 0-length semantics.
- [ ] FIEMAP flags are explicitly supported at the ABI boundary or rejected according to the `EBADR` rule, and all reserved output fields are initialized.
- [ ] Cross-fd copy paths check source and destination types, blocking semantics, and the write-back of the user offset.
- [ ] Metadata and access changes stay in sync with the `kcred` credential model.
- [ ] Each multi-step pathname operation takes the credential snapshot only once and passes it through the entire resolution process.
- [ ] Pathname and descriptor operations do not confuse the caller's DAC at call time with the open-file authority.
- [ ] New mount/umount flags are either fully implemented or explicitly rejected.
- [ ] `MS_PRIVATE`/`MS_SLAVE` propagation requests are validated against their respective flag sets (`MS_REC`/`MS_SILENT` allowed,
  any other combination gives `EINVAL`); `MS_REC` is still rejected when not accompanied by `MS_BIND` or propagation bits.
- [ ] `pivot_root` completes all URL validation before the call, and, following `chroot_fs_refs()` semantics, redirects the `FsStruct` of every
  process whose root/pwd is exactly the old root to `new_root` (not only the caller); `umount2(MNT_DETACH)` detaches via `detach_tree` rather than silently unmounting synchronously.
- [ ] Is mount data copied with bounds at the syscall boundary as one page of opaque bytes, with the filesystem storing only the parsed state?
- [ ] New filesystem types are wired in through the KVFS registry, without adding concrete crate dependencies at the syscall layer or
  a name table separate from `/proc/filesystems`.
- [ ] New logging does not emit file contents or sensitive user buffers.
- [ ] Does the xattr list check the cumulative length item by item, use only one output buffer, and only count when `size == 0`?
- [ ] If a current known limitation is fixed, update this document and `design.md` accordingly.

The ioctl entry passes the current caller's credential snapshot through KVFS.
The block dispatcher authorizes `BLKROSET` using effective `CAP_SYS_ADMIN`
before copying the argument, returning EACCES on failure. The opening credential
does not authorize later ioctl callers.

Content-change notification uses inode event masks only to avoid unnecessary
routing work. Permissions remain checked at registration and the notifier
revalidates subscriptions under its own lock. Parent/name/linkage snapshots use
one location guard; callbacks run after it is released. The registry pins inode
objects through routing removal, preventing address reuse. Bind aliases share
object subscriptions. Special files do not report parent content events, and
`IN_EXCL_UNLINK` is applied per watch using the particular dentry's unlink state.

## Disk quota control

Commands, quota types, format IDs, record identities, information masks and
numeric ranges are validated at the syscall or core administrative boundary.
Classic unknown record-valid bits are ignored; information masks are strict.
Only selected input fields are converted, so irrelevant fields cannot reject an
otherwise valid update. Quota carriers explicitly represent ABI padding and
initialize every byte copied to user memory.

The current subjective credential authorizes changes and enumeration. Own-user
queries compare effective UID; group queries compare effective GID and
supplementary groups, independently of filesystem GID. Descriptor access
retains descriptor error ordering and target filesystem lifetime. Superblock
admission serializes quota enable/disable against reconfiguration and shutdown.
File-attribute ioctl copies use the typed user-memory interface. VFS checks
inode ownership and capabilities before passing validated attributes to the
filesystem; output padding is initialized before copyout.

Quota format and information queries build owned ABI snapshots under the quota
state mutex and release it before copying to user memory. Record and extended
status queries follow the same boundary: a user destination may fault into file
allocation and quota accounting, so no quota-state or record guard spans copyout.
