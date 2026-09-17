# StarryOS（tgoskits）与 x-kernel 容器/内核能力对照文档集

## 背景

x-kernel 为把 dockerd/containerd/runc 跑进客户机，在 `debin/bug-fix2` 分支积累了 22 个修复提交
（AF_UNIX SCM_RIGHTS、execve 架构校验、AT_PHDR、namespace、waitid/execveat/memfd、procfs、
ext4 journal、madvise、epoll_pwait 等）。对照审计发现：tgoskits（StarryOS）在绝大多数同类能力上
**已经有更完整、更贴近 Linux 语义的实现**，且多数学术点拥有独立设计文档与单元测试。

本系列文档逐一拆解 tgoskits 在这些领域的设计与实现细节，作为后续同类移植、评审与扩展的参照基线。

## 文档索引

| 文档 | 领域 | 相对 x-kernel 的典型差异 |
| --- | --- | --- |
| [sync-tgoskits-framework.md](sync-tgoskits-framework.md) | 基础框架：异步 syscall 运行时、seccomp 前置入口、统一文件抽象、错误模型 | 阻塞 syscall 走 async future（`block_on`/`poll_io`），不存在 Go netpoll 死锁类问题；无 `copy_file_range` ENOSYS workaround 需求 |
| [sync-tgoskits-namespace.md](sync-tgoskits-namespace.md) | namespace：七类 ns、NsFd、`/proc/<pid>/ns`、unshare/setns 双通道、PID 多级编号、每进程 netns | `setns` 同时支持 NsFd 与 PidFd；`/proc/<pid>/ns/<type>` 可 bind-mount；包含 user/cgroup 命名空间（x-kernel 未做） |
| [sync-tgoskits-network.md](sync-tgoskits-network.md) | 网络：AF_UNIX SCM_RIGHTS 流式传递、字节标记 cmsg、unix namespace 钩子、per-process netns | SCM_RIGHTS 是完整 Linux 语义（byte-marked cmsg、MSG_PEEK scm_fp_dup、无 control buffer 丢弃），非"补丁式"支持 |
| [sync-tgoskits-process-fs.md](sync-tgoskits-process-fs.md) | 进程/文件系统：waitid、execveat、memfd、pidfd、de_thread、procfs、mount/pivot_root、tmpfs/devpts | `waitid(P_PIDFD)`、`pidfd_open`、`clone3` 原生具备；`/proc/<pid>/exe` 用 VFS 符号链接跟随解析；mount 传播与 `pivot_root` 对齐 Linux |
| [sync-tgoskits-security-cgroup.md](sync-tgoskits-security-cgroup.md) | 安全/容器约束：seccomp BPF 解释器、capabilities、cgroup v2 pids | seccomp 是原生 classic-BPF 解释器（STRICT/FILTER/TSYNC/GET_ACTION_AVAIL）；cgroup 是可复用 `ax-cgroup` 领域 crate（含 pids 计数平衡） |

## 能力矩阵（x-kernel 22 提交 ↔ tgoskits）

| x-kernel 修复 | tgoskits 对应实现 | 相对成熟度 |
| --- | --- | --- |
| `de040ed59` SCM_RIGHTS stream | `net/ax-net/src/unix/stream.rs` 字节标记 cmsg | tgoskits 更完整（详见 network 文档） |
| `c33a5c481` e_machine 校验 | 本仓库 `debin/docker-startup` 已移植（`components/kernel-elf-parser`） | 相同 |
| `1491fd9c7` AT_PHDR 可选化 | 同上 | 相同 |
| `b555fe18e` PID ns/unshare/setns/pivot_root/pipe chown | `kernel/src/namespace/` 全栈 + `NsFd` | tgoskits 覆盖 user/cgroup ns 与 PidFd setns |
| `e8e5475eb`、`d6dc4986e` waitid/execveat/memfd/P_PIDFD | `syscall/task/wait.rs`、`execve.rs`、`file/memfd.rs`、`fs/pidfd.rs` | tgoskits 原生具备且统一在 WaitTarget 架构下 |
| `d11b8831f` prctl PDEATHSIG/NNP、de_thread | `syscall/task/ctl.rs`、`execve.rs` de_thread | tgoskits 已实现 |
| `38507b5bb` 等 netns | `namespace/net.rs` 每进程 NetNamespace | tgoskits 已实现 |
| `2d02c38ea` `/proc/[pid]/stat` starttime | `task/stat.rs` 字段存在但**未填充**（恒 0） | tgoskits 仍有同源缺口（详见 process-fs 文档 §5.1） |
| `5b65b41dc`/`4de00b5cc` `/proc/[pid]/exe` | `pseudofs/proc.rs` Symlink + VFS follow | tgoskits 用符号链接结构，天然正确 |
| `96da9d267` madvise 提示字 | `syscall/mm/mmap.rs` 接受全部提示字 | tgoskits 已实现 |
| `8fdee91bc` epoll_pwait sigsetsize | `syscall/io_mpx/epoll.rs` 条件校验 | 一致 |
| `7fc3d5a6a` copy_file_range→ENOSYS | 真实实现 + async 模型 | tgoskits 无此 workaround 需求 |
| `84f4b89b9` MS_SLAVE/pivot_root | `syscall/fs/mount.rs` | tgoskits 已实现 |
| `0bc32ced9` ext4 journal credits | `fs/rsext4/src/jbd2` | 不同设计（无 credit 机制，无同类问题） |
| `b0b121ca6` 内核 strace | `kernel/src/tracepoint`、`perf`、`uprobe`、ax-tracepoint | 替代方案，更通用 |
| （x-kernel 未涉）seccomp | `task/seccomp.rs` classic-BPF 解释器 | tgoskits 独有 |
| （x-kernel 未涉）cgroup v2 | `components/ax-cgroup` + `kernel/src/cgroup` | tgoskits 独有 |
| （x-kernel 未涉）clone3/pidfd_open/setns(PidFd) | `syscall/task/clone.rs`、`fs/pidfd.rs` | tgoskits 独有 |

## 阅读建议

- 对照阅读：每篇文档均给出"x-kernel 为何需要修 / tgoskits 现状 / 更完善的实现细节"三段式结构。
- 验证口径：所有结论以 `origin/main`（`67beb1724` 合并 `#2402`）代码为准，代码位置均已复核。
- 扩展边界：每篇文档末尾给出明确的非目标与后续增量，避免把"能力更全"误读为"全部可用"。