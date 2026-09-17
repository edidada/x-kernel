# StarryOS 安全与容器约束：seccomp、capabilities、cgroup v2

## 1. 对比背景

x-kernel 的 22 个提交没有专门的 seccomp/cgroup 实现（容器 ABI 增量的命名空间与 syscall
部分之外，安全约束依赖 host 与 guest 边界）。tgoskits 把这些能力作为**独立子系统**内建：

- `task/seccomp.rs`：原生 classic-BPF 解释器，执行 Linux 内核翻译后的 seccomp 过滤器；
- `components/ax-cgroup`：跨内核/虚拟化可复用的 cgroup v2 领域 crate；
- capabilities：`cred()` 体系贯穿 syscall 权限检查（mount/chown/setns...）。

## 2. seccomp

### 2.1 入口与操作

`kernel/src/syscall/sys.rs:981`

```rust
pub fn sys_seccomp(op: u32, flags: u32, args: *const ()) -> StarryResult<isize>
```

- `SECCOMP_SET_MODE_STRICT`：安装严格模式（只允许白名单 syscall）；
- `SECCOMP_SET_MODE_FILTER`：`read_seccomp_filter(args)` 读取用户态 `sock_fprog`，
  `append_seccomp_filter` 链式追加（过滤链可多级叠加）；
  `SECCOMP_FILTER_FLAG_TSYNC` 时 `sync_seccomp_to_thread_group()` 同步线程组；
- `SECCOMP_GET_ACTION_AVAIL`：查询内核支持的 action；
- `SECCOMP_ALLOWED_FLAGS` 显式白名单（TSYNC/LOG/SPEC_ALLOW/TSYNC_ESRCH），
  非法 flag 一律 EINVAL，不存在静默忽略。

### 2.2 classic-BPF 解释器

`kernel/src/task/seccomp.rs`（模块注释：compact classic-BPF interpreter for
`struct seccomp_data`）：

- `SockFilter`/`SockFprog` 与 linux_raw_sys 对齐；指令解释执行
  `seccomp_data`（syscall number + args），过滤结果转 `SeccompDecision`；
- 决策在 syscall 入口前置评估（`handle_syscall`，见 framework 文档 §3）：
  `Allow / Errno / KillProcess / KillThread / UnsupportedAction`；
- `KillProcess`/`KillThread` 以 `do_exit(SIGSYS)` 实现——与 Linux
  `SECCOMP_RET_KILL_PROCESS/THREAD` 语义一致；
- 前置评估发生在 `trace_sys_enter` 之前，对齐 Linux"先 seccomp 后 raw syscall trace"

顺序。`evaluate_seccomp`（`task/thread.rs:916`）是线程级评估点，`has_seccomp_syscall_work()`
为调度保留的预检位。

### 2.3 与 x-kernel 的差距

x-kernel 未实现 seccomp（其容器 ABI 增量不含过滤器支持）；Docker 默认 seccomp profile
（任意用户态 syscall 白名单）只有解释器就绪后才可执行。tgoskits 该能力已完整
（STRICT/FILTER/TSYNC/GET_ACTION_AVAIL + BPF 解释），是"能跑 dockerd 默认配置"的
必要条件之一。

## 3. capabilities

syscall 层权限检查统一经 `current().as_thread().cred()`：

- `has_cap_sys_admin()`：`mount`/`umount` 的传播与 `pivot_root`、`unshare(CLONE_NEWNS/
  CLONE_NEWCGROUP)`、`setns` 特权类操作（`syscall/fs/mount.rs` 多处、`task/namespace.rs`）；
- `has_cap_chown()`/组归属检查：`fchownat`/`fchown` 的 Linux 语义
  （`syscall/fs/ctl.rs`：变更 owner 需 CAP_CHOWN；仅改 group 时 owner 匹配+成员组
  即允许；chown 同时按 `should_remove_sgid` 规则清 SUID/SGID）；
- `cred.fsuid`、`cred.in_group()` 支撑身份语义。

与 x-kernel 相同点：两者都做基础 cap 校验（x-kernel 的 unshare 也要求 CAP_SYS_ADMIN）。
tgoskits 的差异在**范围**：cap 检查贯穿 mount 传播、ns 创建、chown 等所有受影响路径，
且 `cred()` 作为线程/进程身份的组成部分统一管理（而非靠散落的特权位）。

## 4. cgroup v2

### 4.1 可复用领域模型：`components/ax-cgroup`

```
components/ax-cgroup/src/
├── lib.rs         ProcessId(NonZeroU64)、CgroupError、init()/root()/
│                  attach_initial_process/begin_process_at/relative_path
├── node.rs        CgroupNode（层级/控制器状态）
├── membership.rs  进程成员关系（CgroupForkGuard/CgroupTaskExit）
└── pids.rs        pids 控制器（try_charge/release、pids.current/peak 计数）
```

- `CgroupForkGuard`：fork 时守卫——子进程创建前先 charge pids 预算，失败则 fork 失败
  （EAGAIN），成功路径在 fork 回滚/线程退出/进程退出/迁移时**保持计数器平衡**
  （pids.rs 中有 current 溢出/下溢 debug_assert）；
- `pids.peak`：节点生命周期内 `pids.current` 峰值只读接口；
- 仓库已有 L4 设计文档 `docs/design/starry-cgroup-pids-l4.md`（成功标准：
  迁移进非 root pids 使能 cgroup、设置 `pids.max`、观察超限 fork 返回 EAGAIN）。

### 4.2 内核集成：`kernel/src/cgroup/mod.rs`

- 实现 `ax_cgroup::CgroupProvider`（is_zombie/membership/set_membership 等），
  与 `task/pid.rs` 的 identity/zombie 状态机对接；
- 接口文件白名单：`cgroup.procs`、`cgroup.controllers`、`cgroup.subtree_control`、
  `pids.max`、`pids.current`、`pids.peak`、`pids.events`（七项，不暴露未实现控制器）；
- `sysfs.rs` 预建 `/sys/fs/cgroup` 挂载点（注释明确：systemd/容器栈在 rootfs 上
  tmpfs 后挂 cgroup2，mount point 必须存在否则 `mount("/sys/fs/cgroup")` ENOENT）。

### 4.3 与 x-kernel 的差距

x-kernel 无 cgroup 子系统（dockerd 的 cgroup 挂载和 `pids.max` 限制不可用）。
tgoskits 提供 cgroup v2 层级 + pids 控制器；仍缺 cpu/memory/cpuset 控制器（属
`starry-cgroup-pids-l4` 明确非目标）。

## 5. 三类约束组合成的容器运行前提

| 约束 | tgoskits | dockerd/runc 使用点 |
| --- | --- | --- |
| seccomp FILTER | BPF 解释器 + TSYNC | 默认 profile 白名单 |
| capabilities | cred() 系统化校验 | 容器 init 降权、mount 权限 |
| cgroup pids | ax-cgroup 层级 + pids.max | OOM/进程上限管理 |
| （配套）namespace | 七类完整（见 namespace 文档） | clone/unshare/setns 隔离 |

## 6. 边界与非目标

- seccomp 目前是线程级过滤链，未实现 user notification（`SECCOMP_RET_USER_NOTIF`）、
  `SECCOMP_GET_NOTIF_SIZES`、TSYNC 的 ESRCH 区分行为未完整验证；
- cgroup 仅 pids 控制器；cpu/memory/cpuset、threaded 模式、delegation、no-internal-process
  规则等不在当前范围（以 `starry-cgroup-pids-l4.md` 为准）；
- capabilities 模型覆盖内核侧检查点，未实现完整的 file cap 执行转换语义
  （对应 x-kernel 中 PR_SET_NO_NEW_PRIVS 的"exec 不授特权"假设在此同样成立）。