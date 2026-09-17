# StarryOS 基础框架：异步 syscall 运行时与统一抽象

## 1. 对比背景：x-kernel 为什么需要 `copy_file_range → ENOSYS`

x-kernel 在 docker bring-up 时发现：Go 运行时（containerd/dockerd）**不**把 `copy_file_range`
注册为可 poll 的 IO，runc 用它转发容器 stdio pipe 时，该调用会在同步内核路径上阻塞整个 M-thread；
M-thread 耗尽导致 Go netpoll（epoll）无法运行，等待 socket 读的 goroutine 永不被唤醒 → 死锁。
x-kernel 的修复是让 `copy_file_range` 恒返回 `ENOSYS`，迫使 Go 回落到 `read+write`
（可注册进 netpoller 的路径）——这是对同步 syscall 模型的 workaround。

tgoskits 不存在这个问题的根因：**内核 syscall 是异步 future 模型**，阻塞操作挂起的是 future，
而不是执行线程；Go 的 M/N 模型依赖"内核永不整体阻塞一个线程"的前提在这里天然成立。

## 2. 异步 syscall 模型

### 2.1 入口：`handle_syscall`

`os/StarryOS/kernel/src/syscall/mod.rs:122`

```rust
pub fn handle_syscall(current: &UserTaskRef, uctx: &mut UserContext) -> SyscallRestart
```

- 用户线程入口持有 `UserTaskRef`（scheduler 拥有的强引用），syscall 帮助函数在整个阻塞期间
  借用同一 capability，而不是反复重新获取 `current`。注释明确对齐 Linux 的
  "syscall-local use of `current`"。
- 返回值 `SyscallRestart` 区分 `Allowed` / `Suppressed` / 重试语义，支持 EINTR 驱动的重启路线。

### 2.2 阻塞即 future：`block_on_user` / `poll_io` / `future::timeout`

阻塞 syscall 内部用 `ax_task::future` 运行时驱动 IO future：

- `kernel/src/syscall/signal.rs`：`block_on_user` / `block_on_user_timeout`（sigsuspend/sigtimedwait 等）。
- `kernel/src/file/fs.rs`：读写路径 `block_on_user(...)` + `poll_io`。
- `kernel/src/syscall/io_mpx/epoll.rs`：

```rust
let count = with_blocked_signals(
    nullable!(sigmask.get_as_ref())?.copied(),
    || match block_on(future::timeout(timeout, poll_io(...))) { ... },
)?;
```

`sigset`/signal 掩码处理与阻塞等待是组合进同一个 future 的，而不是"先设置掩码再同步睡"。

### 2.3 该模型带来的下游收益

| 场景 | x-kernel（同步模型） | tgoskits（异步模型） |
| --- | --- | --- |
| `copy_file_range` 转发 pipe | 阻塞整个 M-thread → Go 死锁 → 被迫 ENOSYS | 挂起 future，线程可继续调度；保留真实实现（`syscall/fs/io.rs:841`） |
| epoll_wait 等待 | 需要特殊事件交付处理 | `poll_io` + `register_waiter_wakers` 组合，天然异步 |
| 信号与阻塞组合 | 需手写 EINTR 重试状态机 | future 化掩码 + 超时，超时/信号统一走 `future::timeout` 结果 |

> 注：tgoskits 不是"比 x-kernel 少功能"，而是"用不同内核模型消除了问题本身"——这正是
> 对照文档要强调的结构性差异。

## 3. seccomp 前置入口（syscall 分发的一部分）

`handle_syscall` 在分发前先执行 seccomp 评估（`thread.rs:916 evaluate_seccomp`）：

- `SeccompDecision::Allow` → 继续分发；
- `Errno(errno)` → 直接设置返回值，`Suppressed`（不表示可重启的中断）；
- `KillProcess` / `KillThread` → `do_exit(SIGSYS, ...)`；
- `UnsupportedAction` → ENOSYS。

Linux 的 `raw_syscalls:sys_enter` tracepoint 在 seccomp 之后、分发之前触发
（`trace_sys_enter`，无消费者时退化为 inline 开关检查，不收集六参数）。

分发表以 `Sysno::new(raw_sysno)` 查表，未知 syscall 返回 ENOSYS（`syscall/mod.rs` 的
`Sysno` 全覆盖匹配，`clone3`/`setns`/`unshare`/`seccomp`/`pidfd_open` 等均在表中）。

## 4. 统一文件抽象：`FileLike`

`kernel/src/file/mod.rs:211`

```rust
pub trait FileLike: Pollable + DowncastSync { ... }
```

所有 fd 语义对象（普通文件、pipe、socket、eventfd、signalfd、timerfd、memfd、pidfd、NsFd、
netlink、packet、dmabuf 等 `kernel/src/file/` 下 20+ 类型）实现同一 trait：

- 读写：`read(IoDst)` / `write(IoSrc)`，通过 `IoDst`/`IoSrc` 统一用户内存与内核缓冲；
- 查询/修改：`stat()`、`path()`、`owner()`/`set_owner()`、`inode_key()`、`open_flags()`；
- 复用：`ioctl()`、`file_mmap()`/`device_mmap()`、`poll`（继承自 `Pollable`）；
- 特性：`nonblocking()`/`set_nonblocking()`、`async_mode()`/`supports_async_mode()`。

设计收益：

1. **pipe chown 等"小缺口"不存在**：x-kernel 需要为 pipe 新增 `MetadataOnlyInodeOperations`
   （setattr EOPNOTSUPP 阻塞 runc fchown），tgoskits 的 `set_owner` 是 FileLike 的一等公民，
   runc fchown 容器 stdio pipe 走 `sys_fchownat(fd, ..., AT_EMPTY_PATH)` → `resolve_at(...)`
   → FileLike 元数据路径，无特殊 inode-op 分派。
2. **fd 泛型组合**：`waitid(P_PIDFD)`、`setns` 复用同一 `downcast_arc::<PidFd>()` 模式；
   `execveat(AT_EMPTY_PATH)` 用 `downcast_ref::<Memfd>()` 区分"匿名 fd 可执行"。
3. `fd_is_path`（O_PATH 语义）、`release_locks_on_close` 等在统一层实现，避免各实现重复。

## 5. 错误模型：`StarryResult` / `StarryError`

syscall 层统一 `StarryResult<T> = Result<T, StarryError>`；错误转换在边界（如
`map_elf_error`：内核解析失败 → `InvalidExecutable` → execve 的 ENOEXEC 路径）。

与 Linux errno 的映射集中在 `Errno`（`syscall/mod.rs` 中有 `into_raw()`）。
文档约定（`docs/guideline/starry_syscall.md`）要求用户可见 syscall/Linux ABI 语义变更
走该文件的评审镜片——本框架是这些评审得以落地的类型基础。

## 6. 边界与非目标

- 异步模型覆盖 syscall 阻塞点，但**不**意味着全内核 IO 异步（ext4 块 IO 等仍是同步调用点）。
- `SyscallRestart` 只表达用户可见的重启语义，不引入 io_uring 级别的异步提交接口（io_uring
  文件是独立子系统的方向）。
- 本框架的"更完善"应理解为**结构性差异**，而非与 x-kernel 逐点对齐的功能清单。