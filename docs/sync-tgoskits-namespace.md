# StarryOS 命名空间全栈实现

## 1. 对比背景：x-kernel 的增量 vs tgoskits 的既成体系

x-kernel 在 docker bring-up 中分批补齐命名空间：CLONE_NEWPID（子 PID 命名空间）、
`unshare`/`setns`、`pivot_root`+MNT_DETACH、per-process netns、`/proc/<pid>/ns/net`。每个
都是一到数个独立提交，且 NEWNET/NEWUSER/NEWCGROUP 明确返回 EOPNOTSUPP。

tgoskits 的命名空间是**一个已经完整的子系统**：`kernel/src/namespace/` 下七类命名空间齐备，
`NsFd`（namespace fd）是一等公民，`/proc/<pid>/ns/<type>` 文件可 bind-mount，`setns` 同时
支持 NsFd 与 PidFd 两条通道，疑难语义有独立设计文档（`docs/design/starry-pid-namespace-identity.md`）
与单元测试（`task/pid.rs` 的状态机测试）。

## 2. 七类命名空间

`os/StarryOS/kernel/src/namespace/`：

| 文件 | 类型 | 说明 |
| --- | --- | --- |
| `uts.rs` | `UtNamespace` | hostname/domainname 隔离 |
| `ipc.rs` | `IpcNamespace` | SysV IPC 标识隔离 |
| `mnt.rs` | `MntNamespace`（+ ax-fs-ng `MountNamespace`） | 挂载树视图（含传播语义，见 process-fs 文档） |
| `pid.rs` 语义（经 `task/pid.rs`） | `PidNamespace` | 每层 ns 一个进程号的层级视图 |
| `net.rs` | `NetNamespace` | 接口/路由/防火墙/socket 集隔离 |
| `user.rs` | `UserNamespace` | uid/gid 映射（`uid_map`/`gid_map`） |
| `cgroup.rs` | `CgroupNamespace` | cgroup 层级视图（配合 ax-cgroup） |

## 3. NsFd：namespace 文件描述符

`os/StarryOS/kernel/src/file/nsfd.rs`

```rust
pub enum NsFd {
    Uts(Arc<IrqMutex<UtNamespace>>),
    Ipc(Arc<IrqMutex<IpcNamespace>>),
    Mnt { ns: Arc<IrqMutex<ProxyMntNamespace>>, fs_ns: Arc<FsMountNamespace> },
    Pid(crate::namespace::PidNamespaceRef),
    Net(Arc<IrqMutex<NetNamespace>>),
    User(Arc<IrqMutex<UserNamespace>>),
    Cgroup(Arc<IrqMutex<CgroupNamespace>>),
}
```

- 通过打开 `/proc/<pid>/ns/<type>` 创建；`ns_type()` 返回对应 `CLONE_NEW*` 常量。
- 实现 `FileLike`：`path()` 返回 Linux 同款显示名（`anon_inode:[uts_ns]` 等），
  写访问恒拒绝（`InvalidInput`）。
- `Mnt` 变体同时持有内核 `MntNamespace` 与 ax-fs-ng 的 `FsMountNamespace`，
  保证 setns 到挂载命名空间时文件系统视图同步切换。

### 3.1 `/proc/<pid>/ns/<type>` 目录

`kernel/src/pseudofs/proc.rs`（`"ns" => SimpleDir::new_maker(...)`，节点
`"ns"` 在进程目录枚举中）：

- 每个 ns 文件是真实文件（inode 标识命名空间身份），**可 bind-mount** —— 这正是 Docker
  libnetwork 持久化沙箱命名空间的做法：bind `/proc/self/ns/net` 到
  `/var/run/docker/netns/<id>` 后 `setns` 重入。
- 文档保留 readlink 失败的偏差记录（与 Linux nsfs-backed magic symlink 的差异被显式标注）。

## 4. unshare / setns

`kernel/src/syscall/task/namespace.rs`

### 4.1 `sys_unshare`

```rust
const UNSHARE_NAMESPACE_FLAGS: u32 = CLONE_NEWUTS | CLONE_NEWPID | CLONE_NEWNS
    | CLONE_NEWNET | CLONE_NEWIPC | CLONE_NEWUSER | CLONE_NEWCGROUP;
const SUPPORTED_NS_FLAGS: u32 = UNSHARE_NAMESPACE_FLAGS | CLONE_FS | CLONE_FILES;
```

- **七类命名空间全部支持**（x-kernel 首版只做 NEWNS/NEWUTS/NEWIPC/NEWPID）。
- 特权检查：`NEWNS | NEWCGROUP` 需要 `has_cap_sys_admin()`。
- 流程拆成 `PreparedUnshare::prepare(flags, thread)?` 与 `prepared.commit(thread)`：
  所有校验（包括 x-kernel 提交里强调的"共享 fs_struct 时拒绝 NEWNS"这类 Linux 规则）
  在 prepare 阶段完成；commit 阶段只做状态切换。
- 与 `CLONE_FS`/`CLONE_FILES` 的组合语义按 Linux 处理（unshare 的 fs/files 共享检查）。

### 4.2 `sys_setns`

```rust
pub fn sys_setns(fd: u32, nstype: u32) -> StarryResult<isize>
```

双通道解析：

1. **NsFd 通道**：`file_like.downcast_ref::<NsFd>()` → `setns_via_nsfd(nsfd, nstype)`。
   - `nstype != 0` 时必须与 fd 类型匹配（`ns_type()`），否则 EINVAL；
   - 多线程进程改 PID 命名空间返回 EINVAL（对齐 Linux）。
2. **PidFd 通道**：`downcast_ref::<PidFd>()` → `setns_via_pidfd(pidfd, nstype)`。
   - Linux 5.8+ 语义：对 pidfd 指向的进程所属命名空间 setns；
   - 配合 `pidfd_open`（`syscall/fs/pidfd.rs`）形成完整闭环。

支持集合 `SUPPORTED_SETNS_FLAGS = SUPPORTED_NS_FLAGS & !CLONE_FILES`。

## 5. PID 命名空间：统一身份 + 按观察者投影

`task/pid.rs`（详解见 `docs/design/starry-pid-namespace-identity.md`，对照 Linux v7.1
`struct pid`/`upid` 设计）：

- 单一 `PidIdentity` 承担 PID/TGID、PGID、SID 等所有角色（对齐 Linux `enum pid_type`）；
  身份是稳定引用对象，不是全局整数——消除"数字复用前后两个 generation"的 ABA 风险。
- 命名空间内可见性按**观察者投影**：`visible_number(observer: &PidNamespaceRef)` /
  `visible_number_in(observer: PidNamespaceId)` 返回当前观察者命名空间下的编号；
  `is_lookup_visible()` 过滤未发布/回收中的身份（对齐 Linux 的 `pid_allocated` 生命周期
  ——exit 路径完成前不释放，防止半发布）。
- `PidFd` 保存 `Arc<PidIdentity>`，`waitid(P_PIDFD)` 与 `setns(pidfd)` 都通过 identity
  匹配，而不是裸 pid 数字。
- 配套状态机测试 `pid_identity_state_machine_rules_hold_for_test` 覆盖创建/切换/退出/回收
  的可审计状态转换（同文档背景明确这是跨 crate 的高风险身份重构）。

> x-kernel 的 `allocate_in(target_ns)`（子命名空间内分配多编号句柄）是同类问题的单点
> 修复；tgoskits 的重构面更广——一个身份事实来源 + 观测点投影，且此差异在
> `starry-pid-namespace-identity.md` 中有完整设计记录。

## 6. 网络命名空间：per-process 实例

`kernel/src/namespace/net.rs`

```rust
static NEXT_NET_NS_ID: AtomicU64 = ...;
pub static ROOT_NET_NS: LazyLock<Arc<IrqMutex<NetNamespace>>> = ...;
```

- `NetNamespace { ns_id: u64 }`：初始命名空间 id = 1，之后单调分配；
- `clone_ns()` 创建新实例；`unshare(CLONE_NEWNET)`/`clone(CLONE_NEWNET)` 后进程获得
  仅含 down loopback（127.0.0.1/8）的全新命名空间——Linux 初始态；
- 接口/路由/防火墙/socket 集合随命名空间隔离（kernel/netlink 等按 ns_id 过滤）。
- 进程侧：`Process` 携带 `net_ns`（plain atomic，可从任意执行上下文读取），
  fork 继承父命名空间 id。

## 7. cgroup 命名空间与 mount 命名空间

- `CgroupNamespace`：与 `components/ax-cgroup` 的层级模型配合（见 security-cgroup 文档）。
- `MntNamespace`：挂载树视图与传播语义见 process-fs 文档的 mount 一节；其 NsFd 变体
  （`Mnt { ns, fs_ns }`）保证 setns(CLONE_NEWNS) 同时切换 VFS 视图。

## 8. 相比 x-kernel 更完善的点（小结）

| 能力 | x-kernel（debin/bug-fix2 分期提交） | tgoskits（现成体系） |
| --- | --- | --- |
| namespace 类别 | PID 起步，NEWNET/NEWUSER/NEWCGROUP EOPNOTSUPP | 七类全实现，含 user/cgroup |
| setns 通道 | NsFd（后续补 net） | NsFd + PidFd 双通道 |
| `/proc/<pid>/ns` 文件 | 后续补 bindable 文件 | 体系自带，inode=ns id |
| unshare 校验 | prepare 逻辑散在提交内 | `PreparedUnshare::prepare/commit` 显式两段式 |
| PID 身份模型 | `allocate_in(target_ns)` 单点修复 | `PidIdentity` 统一身份 + 按观察者 `visible_number` 投影，含完整设计文档与状态机测试 |
| 设计文档 | 无独立文档 | `starry-pid-namespace-identity.md` 等 |

## 9. 边界与非目标

- user namespace 当前用于 setns/unshare 与 uid_map 呈现，非特权容器所需的完整
  chown/mount 权限降级语义需单独增量。
- cgroup namespace 的 controller 行为受 ax-cgroup 控制器集合约束（当前 pids）。
- 未验证 Docker libnetwork 的完整 ns 生命周期（bind/setns/GC）流程。