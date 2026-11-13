# cgroup2fs — 安全与可靠性分析

## Superblock-owner failure

NoMemory from SimpleFs propagates through the mount context without publishing a
root or cgroup node. The initial boot-only mount retains its fatal-boot boundary.
This owner conversion does not make cgroup node, Arc or namespace allocations
fallible. New callers must not wrap the constructor in hierarchy/reclaim locks.

## Fallible directory iterators

Iterator allocation failure propagates NoMemory and drops the name snapshot
and operation reservation. Existing Vec/String snapshot allocations remain
outside the fallible-owner guarantee. Holding a lifecycle reservation does not
authorize holding hierarchy or reclaim-required locks over child_names.

## 信任模型

VFS 复制写入内容并执行 pathname/inode DAC。`cgroup2fs` 仍把所有命令视为不可信输入；
`kcgroup` 负责状态不变量，`kprocess` 负责稳定 task identity 和迁移事务，adapter 使用
VFS DAC、open-file credential 与 mount view 完成授权。cpuset 写入额外经过 cpulist
解析、CPU/NUMA 存在性检查和父层 effective mask 子集检查。

## 外部边界 / 攻击面

- 普通 mount 使用当前进程的 cgroup namespace view root；启动挂载使用 initial view。
- `mkdir`/`rmdir` 名称、控制文件内容（含 cpuset cpulist）和 `cgroup.procs` PID 来自用户态。
- 本 crate 不接触用户指针、MMIO、DMA、FFI、中断或 firmware 数据。

## unsafe 代码清单

无。crate 使用 `#![deny(unsafe_code)]`。

## 内存安全不变量

file closure 通过 `Arc<Cgroup>` 固定目标。per-mount registry 固定目录和控制文件 inode；
删除后的 tombstone 到 unmount 才释放，同名重建不复用旧 identity。controller 文件可见性
从 canonical state 判断。进程枚举必须比较 membership 与 registry task 的同一个
`Arc<PidHandle>`，不能只凭数值 TID。

## 线程安全

node registry 由 sleepable mutex 保护。所有 hierarchy 事务和 operation guard 进入
`kcgroup` transaction lock；调用者不得从 IRQ 上下文进入。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 错误 hierarchy view 被挂载暴露 | 高 | 普通 mount fallback 到全局 root，或启动 mount 创建第二棵 hierarchy | 普通 mount 只用 current view；启动 mount 与 init 共享 initial namespace |
| T-02 | 控制命令注入或歧义 | 中 | 非 UTF-8、NUL、空操作或未知 controller | 完整解析；mutation 前验证所有名称 |
| T-03 | TID 重用展示错误进程 | 中 | 数值 TID 映射到新 task | `kprocess` 对稳定 identity 使用 `Arc::ptr_eq` |
| T-04 | ambient credential 与 fd opener 不一致 | 高 | write 时反向读取 current task，或叠加管理员捷径 | 目录 mutation 使用 VFS credential；command 使用 `VfsFile::f_cred`，无 ambient 管理员检查 |
| T-05 | 从 delegated view 越权迁移 | 高 | source/destination 在 view 外，或只检查 destination | process gate 内验证两端可见，并检查共同祖先 `cgroup.procs` 对 opener 可写 |
| T-06 | 删除/同名重建使旧 fd 操作新节点 | 高 | lookup 重建 inode 或 rmdir 按名称重查 | stable registry、locked victim identity；旧节点操作返回 `ENODEV` |
| T-07 | command 被 offset 拼接或拆成多次 mutation | 中 | 复用 seekable merge 或默认 write_iter 分块 | `CommandFile` 完整复制最多 4096 字节的一次请求并忽略 offset |
| T-08 | `pids.max` 与 unlimited sentinel 冲突 | 中 | 接受超出 PID domain 的 `usize` | 仅接受 `<= 4*1024*1024` 或文本 `max` |
| T-09 | cpuset 写入扩大亲层 CPU 范围 | 高 | 子层提交超出父层 effective mask 的请求 | `kcgroup` 在层级事务内校验 universe 和父层子集；adapter 按各层自身 effective mask 应用 affinity |
| T-10 | 迁移后保留来源 CPU affinity | 中 | 进程迁入更窄 cpuset，但未重新设置 affinity | 迁移成功后立即应用 destination effective cpuset mask |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | 控制文件返回错误 errno | owner API 失败 | 单次写失败 | transaction 保持一致 | 3 | 传播 typed `VfsResult` |
| F-02 | 已删除 task 出现在 `cgroup.procs` | 枚举与退出并发 | identity lookup 失败 | 该项被跳过 | 4 | facade 使用 snapshot 和 identity 校验 |
| F-03 | controller 文件可见性变化 | 激活与 lookup 并发 | 一次 lookup 返回 `ENOENT` | 调用方可重试 | 4 | 每次 lookup/readdir 从 canonical state 判断 |
| F-04 | remove 与旧 fd I/O 并发 | 操作未固定生命周期 | 命令作用于 tombstone | 状态越权修改 | 2 | operation guard 阻止并发 remove，remove 后返回 `ENODEV` |
| F-05 | cpuset 写入只更新文件状态 | mask 已更新但 affinity 未同步 | task 仍运行在旧 CPU 集合 | 隔离策略短期失效 | 3 | cpus 写入成功后遍历本层和后代，按各层 effective mask 更新 affinity；空 mask 跳过 |
| F-06 | cpuset 请求被拒绝 | 索引不存在、越过父层或结果为空 | 返回 `ENODEV`/`EINVAL`/`ENOSPC` | mask 保持原值 | 3 | 校验在存储前完成；失败不刷新后代 |

## 故障管理

解析失败返回 `EINVAL`，未知文件返回 `ENOENT`，DAC 失败返回 `EACCES/EPERM`，已删除
节点返回 `ENODEV`。owner API 的 busy、跨 hierarchy 和计数错误直接传播，不使用 panic
或 fallback 掩盖。

## 隐私分析

模块只展示 cgroup 内 root-namespace PID 和 controller 统计，不读取地址空间或文件内容。
PID namespace 翻译尚未实现，因此 namespace 隔离不能依赖本接口。

## 已知限制

- 非零 `cgroup.procs` ID 仍按 root PID namespace 解析。
- 支持 `pids` 与 `cpuset` controller；`cpu`、`io`、`memory` 未实现。
- `cpuset.mems` 只做层级校验和展示，未接入 NUMA 内存分配策略；不能依赖它限制
  实际内存节点放置。
- cpuset affinity 同步按进程遍历，迁移/控制文件写入路径的调度开销随子树 task 数增长。
- user namespace ID mapping、capability 和 LSM 未完整接入；delegation 当前依赖 inode
  owner/mode、mount view 与 VFS DAC。

## 审计清单

- 新 controller 是否同时接入 state 文件声明和 controller registry。
- 新 write 路径是否使用 `CommandFile`、opener credential、统一 mode 和完整输入校验。
- 是否避免直接访问 scheduler、procfs registry 或 credential internals。
- task 展示是否继续验证稳定 identity。
- 普通 mount 与启动 mount 是否分别保持 current/initial view 边界。
