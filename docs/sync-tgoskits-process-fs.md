# StarryOS 进程与文件系统：waitid/execveat/memfd/pidfd/procfs/mount

## 1. 对比背景

x-kernel 用一个系列提交覆盖进程/文件系统语义：`waitid`（含 `P_PIDFD`）、`execveat`、
memfd exec 权限、`de_thread`、`/proc/[pid]/stat` starttime、`/proc/[pid]/exe` magic link、
mount 传播、`pivot_root`、pipe chown 等。tgoskits 中这些能力**原生存在且多数组装进统一
架构**（FileLike 多态、WaitTarget 联合、Location 抽象），无需逐点补丁。

## 2. wait 系列：统一 WaitTarget 架构

`kernel/src/syscall/task/wait.rs`

```rust
enum WaitTarget {
    Any,                         // P_ALL
    Group(...),                  // P_PGID
    Identity(Arc<PidIdentity>),  // P_PID / P_PIDFD：不存裸 pid，存统一身份
}
```

- `sys_waitpid`（:373）与 `sys_waitid`（:500）共用同一套扫描机制：
  `WaitCandidateScan`（`task/wait_scan.rs`）+ `waitable_processes` +
  `WaitChildFilter`（`from_waitpid_options` / waitid 侧选项）；不存在各 syscall
  单独实现的等待循环与行为漂移。
- `sys_waitid` 支持 `P_ALL/P_PID/P_PGID/P_PIDFD`；`P_PIDFD` 经
  `file_like.downcast_arc::<PidFd>()` 解析为 `WaitTarget::Identity(identity)`，按
  `PidIdentity` 匹配进程（`identity.matches_process(child)`）；`sys_waitpid` 的
  `WaitSelector`（AnyChild / 进程组 / 进程或线程）同样映射到该联合。waitid 的
  `WCONTINUED`/`WEXITED`/`WNOHANG` 选项语义在不引入独立等待循环的前提下接入同一扫描器。
- 这是 x-kernel `e8e5475eb`（waitid + 共享 wait 重构）与 `d6dc4986e`（P_PIDFD）
  叠加的内容；tgoskits 的差别是从设计上就以统一身份做等待目标，而非逐 syscall 补丁。

## 3. execve / execveat

`kernel/src/syscall/task/execve.rs`

### 3.1 sys_execveat

- 接受 `AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW`；`resolve_at(dirfd, path, flags)` 两种结果：
  - `ResolveAtResult::File(loc)`：普通文件按路径执行；
  - `ResolveAtResult::Other(f)`：必须是 `Memfd`（`downcast_ref`），取 memfd 内层
    tmpfs `Location` 执行，显示路径为 `/memfd:<name> (deleted)`。
- 两者汇入 `do_execve`（Linux `do_execveat_common` 的结构）：加载一次，
  `path` 作为显示名（argv0 无关的 comm/exe_path 来源），不重新解析 FS。

### 3.2 memfd 设计（`kernel/src/file/memfd.rs`）

- `Memfd::new(inner, name, allow_sealing)`：`allow_sealing=false` 时立即置
  `F_SEAL_SEAL`（对齐 Linux：无 `MFD_ALLOW_SEALING` 则不可再加 seal）；
- `check_write_seal`/seal 集合原子管理；共享可写 mmap 计数跟踪；
- `sys_memfd_create` 走 tmpfs `create_anonymous_file`（权限 0o666，配合临时空间语义）。
- **exec 权限路径**：tgoskits 的 execveat 从 memfd 的 `Location` 直接加载，
  无"文件可执行位"检查关卡——x-kernel 需要把 memfd 改成 0777 来绕过 EACCES，
  tgoskits 因为加载点设计（Location 而非 open 路径）不存在该问题。

### 3.3 de_thread

`execve.rs` 中多线程 exec 按 Linux 语义替换整个线程组：非 leader 调用者触发
leader 身份迁移（TID/TGID 转移），执行前对兄弟线程发线程定向 SIGKILL 并等待退出；
目标 TID 空闲性在迁移前检查。这对应 x-kernel `d11b8831f` 的 "de_thread()" 提交，
tgoskits 的 execve.rs 注释直接引用 Linux 的 leader transfer 规则。

## 4. pidfd 全套

`kernel/src/syscall/fs/pidfd.rs`

- `sys_pidfd_open`（+ `PIDFD_*` flags 位校验）；
- `PidFd` 文件类型保存 `Arc<PidIdentity>`，支持：
  - `waitid(P_PIDFD, fd)` 回收（见第 2 节）；
  - `setns(fd, nstype)` 经 PidFd 通道重入目标进程所在命名空间（namespace 文档 4.2）；
  - pidfd 信号类操作（`PidFdSignalScope`/`PidFdSignalFlags`）。
- 与 `clone3`（`CLONE_PIDFD`）配合形成完整 pidfd 生命周期管理。

## 5. procfs 关键节点

`kernel/src/pseudofs/proc.rs`

### 5.1 `/proc/<pid>/stat`：starttime 字段存在但未填充（已知缺口）

`task/stat.rs` 的 `TaskStat` 定义了 `starttime: u64`（field 22 位置正确），但构造函数对
该字段一律走 `..Default::default()`（恒 0），全内核再无赋值点——**与 x-kernel `2d02c38ea`
修复前的缺陷相同**。runc 用它对比持久化的 `InitProcessStartTime` 以判断进程是否仍是自己
创建的，恒 0 会破坏其状态机（invalid transition stopped→created）。

→ 需要补齐：进程创建时记录单调时钟并按 USER_HZ(100) 换算后填充；这是 x-kernel
已修而 tgoskits（main 分支）未修的少数几处之一，属于后续增量。

### 5.2 `/proc/<pid>/exe`：符号链接 + VFS 跟随解析

```rust
"exe" => SimpleFile::new(fs, NodeType::Symlink, move || {
    Ok(task.as_thread().proc_data.exe_path.read().clone())
})
```

- x-kernel 的等价实现是 `SimpleFile` 存字符串，open 打开的是 **procfs 文件本身**
  （magic-link 到 backing exe 的分派因 trait 对象安全缺陷失效，`4de00b5cc`/`5b65b41dc`）。
- tgoskits 中该节点是 **NodeType::Symlink**，ax-fs-ng 的 open 路径
  （`fs/ax-fs-ng/src/file/open.rs`，`no_follow=false` 时 `loc.read_link()` 跟随；
  `fs_core/context.rs` 有 SYMLINKS_MAX 递归保护）会把 `/proc/self/exe` 解析回真实可执行
  文件——runc/crun 的 CVE-2019-5736 自克隆检测（`open`+`F_GET_SEALS`）因此成立。
- 结构上 tgoskits 的实现"天然正确"，不需要 magic-link 分派修复。

### 5.3 `/proc/<pid>/ns/<type>`

`"ns" => SimpleDir::new_maker(...)`：每个 ns 文件为真实文件，inode 即命名空间身份，
可 bind-mount（namespace 文档 3.1）。

## 6. mount 传播与 pivot_root

`kernel/src/syscall/fs/mount.rs`

```rust
const MS_SLAVE: i32 = 1 << 19;
const PROPAGATION_FLAGS: i32 = MS_SHARED | MS_PRIVATE | MS_SLAVE | MS_UNBINDABLE;
```

- `mount(2)`：`MS_SLAVE`（可带 `MS_REC`）与 `MS_PRIVATE` 并存；KVFS 无共享传播组，
  slave/private 行为等价，但各自校验并拒绝非法组合；递归 bind 仅在源子树无挂载时
  接受（语义与非递归一致），其余返回 EINVAL——不静默压平挂载树。
- `sys_pivot_root(new_root, put_old)`（`mount.rs:870`）：对齐 Linux `do_pivot_root()`
  语义——`put_old` 可与 `new_root` 重合（`.`+`.old`），失败前置校验后单次更新
  fs 根与 pwd（镜像 chroot 的 fs refs 重定位）。
- `set_slave_recursive`/`set_slave` 挂载点操作齐全。对应 x-kernel `84f4b89b9`。

## 7. tmpfs 与 devpts

- `kernel/src/pseudofs/tmp.rs`：完整 tmpfs 文件系统实现（FileNode/DirNode/FilesystemOps
  接入 axfs-ng-vfs），`/dev/shm`、`/run`、memfd 的匿名文件都落在它上面。
- `kernel/src/pseudofs/dev/mod.rs`：`ptmx`（:551）/`pts`（:560）devpts 节点，
  容器 tty 与 `docker exec -t` 依赖。

## 8. pipe 元数据（x-kernel runc 阻塞点对照）

x-kernel 需要新增 `MetadataOnlyInodeOperations` 让 `fchown` 作用于 pipe inode；tgoskits 的
`FileLike` 直接提供 `owner()/set_owner()`，`sys_fchownat(...AT_EMPTY_PATH)` 的权限检查
（CAP_CHOWN、组归属、SUID/SGID 清除）在 `syscall/fs/ctl.rs` 统一实现——pipe 与普通文件
共用，无 inode-op 分派缺口。

## 9. 边界与非目标

- `copy_file_range` 有真实实现（`syscall/fs/io.rs`），但针对块设备的性能语义未与
  async 模型深度调优（功能性可用）。
- **`/proc/<pid>/stat` starttime 恒 0**：字段存在但未填充（§5.1），需在进程创建时记录
  单调时钟并换算 USER_HZ 后补充——这是对照中 tgoskits 逊于 x-kernel 的少数点之一。
- procfs 的 `exe` 跟随走 path 解析；对 memfd exec 的 `/proc/self/exe`（删除的 memfd
  path）依赖 Location 显示名回退，未与 Linux nsfs 完全对齐（记录为已知偏差）。
- mount 传播只覆盖 slave/private 等价语义；shared/unbindable 传播组未实现——这是
  Docker 叠加挂载传播（propagation=slave）之外场景的限制。