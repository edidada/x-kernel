# Kata Containers on X-Kernel：能力差距分析与 User Namespace 实现设计

> 状态：分析/设计文档（合并版），核实于 2026-11-03/06。
> 由原 `docs/kata-containers-gap-analysis.md`（第一部分）与原
> `docs/USER-NAMESPACE-GAP.md`（第二部分）合并而成，内容保持原状，交叉引用已更新。
> 结论先行：Kata 是需求本体，已在 X-Kernel 上最小可运行（PR #1208）；距生产可用差
> 五项内核能力（第一部分 §5）。本设计的 scope 是其中 namespace/userns 一项：
> **不做 SUID 过渡，L0 并入 L2 P0，直接按 L2 单线路推进，首个验收场景是 Kata rootless**。
> Chromium 桌面只是 userns 能力的顺带受益者，`--no-sandbox` 是 L2 落地前的桌面临时绕过，不是需求本体。

# 第一部分：Kata Containers 现状与能力差距

## 1. 文档定位与结论摘要

本文用 PR #1208 的实测结果对照上游 Kata Containers 的运行时需求，回答两个问题：

1. Kata 能否在 X-Kernel 上把 OCI 容器真正跑起来？
2. 距离「可用运行时」还差哪些内核能力？

结论分两层：

- **能跑通：已验证**。PR #1208 内核作为 Kata guest，连续两次完整跑通 OCI 容器，
  `ctr` 退出码均为 `0`；回退该修复后同一用例无输出、退出码 `135`（SIGBUS）。
  容器内 `/bin/busybox`、`/bin/demo.sh` 均来自 9p 共享目录，动态链接、`execve`
  与脚本读取全部走本 PR 新增的 9p `read_folio` 路径。
- **达生产可用：尚不可以**。缺口集中在 cgroup v2 控制器、namespace 创建、
  capability bounding set、内存/CPU 热插拔、virtio-fs 五项通用内核能力上，
  而不是 Kata 侧适配问题。另外 PoC 阶段存在若干本地补丁，未进入 PR，
  属于必须收敛的差距项（见第 6 节）。

本文只描述现状与差距，不重复 `docs/container-architecture.md` 的目标架构设计；
后者给出补齐这些能力的架构路径。文中「已验证」均指本机 PoC 环境中的实测，
不外推到其他架构、其他 Kata 版本或其他运行时（见第 7 节）。

## 2. 验证环境与拓扑

实测在 x86_64 上进行：QEMU 作为 VMM，X-Kernel 作为 guest kernel，kata-agent 通过
vsock/ttrpc 与宿主侧 shim 通信，containerd shim v2 驱动完整生命周期。

```text
ctr / containerd
   └─ containerd-shim-kata-v2 (Go, 9p 补丁版)
        └─ QEMU (Kata 3.x, /etc/kata-containers/configuration.toml)
             ├─ kernel: target/xkmake/kplat-x86_64/release/kernel.bzimg (X-Kernel)
             ├─ rootfs: ext4 镜像，内含 gnu 构建的 kata-agent
             ├─ vsock: vhost-vsock → kata-agent ttrpc
             └─ 9p (virtio-9p, mount tag "kataShared") → 容器 rootfs / 日志
```

关键环境事实：

- guest 侧 PID 1 是 `entry/src/init.sh`：挂载 9p、启动 `kata-agent`。
- 容器 rootfs 不走 virtio-fs 或 bind mount，而由 shim 补丁降级为
  「9p 共享目录 + symlink」；agent 侧同样降级（宿主无 `CAP_SYS_ADMIN`，
  无法 bind mount / loop mount）。
- 复测使用的 PR 内核为正常构建产物（非 unit-test 内核），SHA-256
  `1951adc1a1ef06fa935cfcd939913ebfb94367006203f2e899652f0e0293b341`。
- QEMU 去掉 `virtio-rng-pci`，只保留 serial/blk/vsock/9p 四个 virtio 设备，
  用于隔离本 PR 关心的路径。

这套拓扑本身已偏离 Kata 默认部署：默认路径依赖 virtio-fs、cgroup 资源限制、
多命名空间隔离，PoC 通过打补丁把这些逐项降级，才让容器跑起来。因此实测结果
证明的是「X-Kernel 能承载 Kata 的核心 ABI 路径」，不是「Kata 默认配置可直接用」。

## 3. PR #1208 实测结果

### 3.1 连续两次跑通 OCI 容器

PR head 为 `3bd03649fb52ff2407fb90648ebdbe9dcd1a2f10`（`debin/9p-read-folio`），
xreview round 4 判定「可合并」，0 个待修改项。

复测命令：

```bash
timeout 120 ctr -n k8s.io run --rm --cgroup "" \
    --runtime io.containerd.kata.v2 \
    --rootfs /root/.tmp/busybox-rootfs kata-sec \
    /bin/busybox sh /bin/demo.sh
```

同一命令连续执行两次，均成功，输出一致：

```text
HELLO_FROM_KATA
Linux 0ea900d952e0 10.0.0 10.0.0 x86_64 Linux
bin
dev
etc
lib
proc
run
sys
xklogs
KATA_ON_XKERNEL_OK
```

`ctr` 退出码两次均为 `0`。`uname` 的 version/release 都是 `10.0.0`，即 X-Kernel
的 UTS 值而非 Linux 内核版本，说明容器确实运行在 X-Kernel 上。

覆盖到的用户态路径：

| 能力 | 证据 |
| --- | --- |
| sandbox 生命周期 | shim `Starting VM`、`Start logging QEMU`；agent `ttRPC server started`；`create_sandbox` 成功 |
| 容器创建 | agent 记录 `process command: ["/bin/busybox","sh","/bin/demo.sh"]` → `create process completed` → `created container!` |
| 动态链接 | busybox 为动态 ELF，共享库由 9p `read_folio` 按需缺页读入 |
| `execve` | `/bin/busybox` 从 9p 共享目录 exec，脚本解释器路径同样解析成功 |
| 脚本读取 | `/bin/demo.sh` 经 9p `read_folio` 读入并由 busybox shell 执行 |
| 生命周期收尾 | `wait_process` → `signal_process` → 正常退出，agent 侧无报错 |

### 3.2 9p read_folio：SIGBUS 的根因修复

**对照实验**：回退 9p inode 改动（等价于修复前状态）后，同一容器命令无任何输出，
以退出码 `135`（SIGBUS）结束；恢复本 PR 后退出码为 `0`。这是本节最强的因果证据：
SIGBUS 不是偶发，而是 9p regular file 没有 `read_folio` 的确定性后果。

根因链条：

1. 修复前 9p regular file inode 没有挂 `AddressSpaceOperations::read_folio`。
2. `execve` 与动态链接器 `mmap` 共享库时触发缺页，folio 永远不进入 uptodate 状态。
3. 缺页无法完成 → 进程收到 SIGBUS（135）。

PR 的修复方式：

- 9p regular file inode 改用 `VfsInode::new_file_with_address_space()` 创建，
  从而携带 address space 操作表。
- 新增 `read_folio`：缺页时按 folio 偏移 zero-name walk 出独立 fid，依次
  `TLOPEN`、`TREAD`；尾部按 folio 大小零填充，短读返回 `VfsError::Io`。
- 每个缺页使用独立 fid，避免并发缺页在同一 fid 上竞争 offset 状态。

实测中 `/bin/busybox` 与 `/bin/demo.sh` 都在 9p 共享目录内，因此动态装载、
`execve`、脚本读取三条路径都真实穿过了新增的 `read_folio`，不是被 page cache
或本地 ext4 掩盖的假通过。

需要明确边界：该修复解决的是**读路径**。9p 仍无 `writepages` / `write_begin`，
可写 `MAP_SHARED` 映射不支持，写入语义另有独立缺陷（见 5.5 节）。

### 3.3 中断与启动期观测

guest 启动期间 `Unhandled IRQ 4097` 计数为 `0`。该观测的意义在于排除
「virtio 设备中断未正确注册」这一整类失败模式：

- 本 PR 之前上游工作已补齐 virtio-pci INTx，4097 是相关 virtio 设备的中断线；
  若中断未被处理，9p / vsock / blk 的 I/O 会表现为超时或挂起，而不是干净的
  SIGBUS 或成功。
- 计数为 0 说明本次跑通不是靠重试或轮询绕过中断路径，I/O 路径是健康的。
- 复测刻意去掉 `virtio-rng-pci`，把设备集合收敛到 serial/blk/vsock/9p 四个，
  使这一结论不被无关设备的噪声干扰。

同时记录两项非阻塞的启动期噪声，它们不改变本次判定，但属于可观测的成熟度差距：

- 单次 Kata 启动到可用状态约 2 分钟，远慢于上游 Kata 的秒级预期。
- guest 日志出现 `memory block size error: NotFound`，与 5.4 节的热插拔 sysfs
  缺失同源。

也就是说，「能跑通」的结论建立在一条干净的 I/O 路径上，但**启动耗时与内存模型
告警**仍提示这套组合距生产体验有距离。

## 4. 上游 Kata Containers 的能力基线

Kata 的设计前提是「guest 内核等价于一套 Linux 内核」，shim 与 agent 只做编排，
不做内核能力探测降级。因此上游 Kata 默认路径对 guest 内核有一组硬性要求：

| 需求 | Kata 侧用法 | 缺失后果 |
| --- | --- | --- |
| cgroup v2 控制器 | rustjail 按 OCI `linux.resources` 写 `cpu.max`、`memory.max`、`cpuset.cpus` 等 | 控制器不存在时 agent 侧 `unwrap()` panic，sandbox 创建失败 |
| namespace 创建 | agent 为容器 `unshare` net/user/cgroup/ipc/uts/pid/mount 各 ns | `EOPNOTSUPP` 直接失败；Kata 默认要求除 mount 外新建全部 ns |
| capability bounding set | `PR_CAPBSET_DROP` 收紧容器 capability 边界 | 调用失败 → 容器 capability 无法收敛，属安全缺口 |
| 内存/CPU 热插拔 sysfs | `OnlineCpuMem` 走 `/sys/devices/system/memory/*/online` 与 cpu hotplug | `ENOENT` → shim 判定 sandbox 异常并拆除 |
| virtio-fs | 默认容器 rootfs 共享方案 | 只能降级 9p，性能与语义均退化 |
| PID namespace + `/proc` | 容器内挂载独立 `/proc` | 容器看到 guest 的 `/proc`，`/proc/version` 等读取失败 |

此外还有一批「默认开启但不一定阻塞启动」的能力：SELinux 状态探测、
`/proc/<pid>/ns/*` 的 pin 语义、`sethostname`、无 CAP_SYS_ADMIN 场景下的
mount 回退等。它们在 PoC 中被逐个绕过或打补丁，构成第 6 节的差距项。

判断标准因此不是「容器能启动」，而是「Kata 默认 configuration.toml + 默认 agent
行为能否不加 `XK_SKIP_*` 开关地完成 `CreateSandbox` 到 `WaitProcess`」。按这个
标准，当前 X-Kernel 尚未达标。

## 5. 能力差距清单

下面按「是否阻塞默认 Kata 路径」排序。P0 表示不加 PoC 补丁就无法启动或
会触发 agent 失败，P1 表示能启动但功能、安全或性能语义不完整。

| 编号 | 差距 | 优先级 | 影响面 |
| --- | --- | --- | --- |
| 5.1 | cgroup v2 控制器缺失 | P0 | 资源限制、agent 稳定性 |
| 5.2 | namespace 创建不完整 | P0 | 隔离模型 |
| 5.3 | `PR_CAPBSET_DROP` 未实现 | P0 | 安全边界 |
| 5.4 | 内存/CPU 热插拔 sysfs 缺失 | P0 | sandbox 扩容与生命周期 |
| 5.5 | 无 virtio-fs，9p 不成熟 | P1 | 性能与写语义 |
| 5.6 | 容器 `/proc` 与 PID ns 视图 | P1 | 容器内可观测性 |

### 5.1 cgroup v2 控制器

**现状**：X-Kernel 只有 `cgroup2fs` 骨架和 unified hierarchy，没有
cpu / cpuset / memory / io 控制器。

**实测表现**：

- rustjail 按 OCI spec 取控制器时，`controller_of::<CpuSetController>().unwrap()`
  直接 panic，sandbox 创建中断。
- PoC 用 `XK_SKIP_CGROUP_RESOURCES=1` 跳过整段资源设置，并用 `--cgroup ""`
  绕开 cgroup 路径，容器才能启动。
- 缺 memory controller 时，`cgroups/fs` 的 `get_pids` 会走到
  `Option::unwrap()` panic；PoC 改为返回空列表（见第 6 节）。

**影响**：`--cpus`、`--memory`、`--pids-limit`、IO 限速全部不可用；
Kubernetes 侧 requests/limits 无法落地。这不仅是「功能缺失」，还会让
agent 以 panic 形式失败，属于默认路径的硬阻塞。

**补齐方向**：按 `docs/container-architecture.md` 第 10 节的 controller 闭集
协调接口实现 cpu/cpuset/memory/io/pids，并保证 `cgroup.controllers`、
`cgroup.subtree_control` 的可见性与可写性符合 cgroup v2 语义。
验收标准是 rustjail 在**不加任何 `XK_SKIP_*`** 的情况下完成资源写入。

### 5.2 namespace 创建

**现状**：`unshare(CLONE_NEWNET)`、`CLONE_NEWUSER`、`CLONE_NEWCGROUP` 返回
`EOPNOTSUPP`；PoC 用 `XK_SKIP_NS_CREATE=1` 只保留 `CLONE_NEWNS`。

**实测表现**：

- agent 创建容器时按 Kata 默认要新建 net/user/cgroup/ipc/uts/pid 等 namespace，
  第一个不支持的 flag 就让调用失败。
- 即使跳过创建，`/var/run/sandbox-ns/{ipc,uts}` 的打开也会返回 `ESRCH`
  （sandbox ns 文件未被正确创建或 pin），PoC 只能在子进程内新建同名 ns 兜底。
- 由于容器没有独立 PID namespace，容器内看到的是 guest 的 `/proc`，
  `cat /proc/version` 失败（见 5.6 节）。

**影响**：容器之间、容器与 guest 之间没有网络/用户/cgroup 隔离。
这是「能不能算容器」的核心语义缺口，不能靠运行时降级长期掩盖。

**补齐方向**：`process/kns` 需要为每个 ns 类型实现完整的 create/attach 生命周期，
并让 `unshare`、`setns`、`clone(CLONE_NEW*)` 三条入口语义一致；
`/proc/<pid>/ns/*` 必须在 lookup 时真正 pin 住命名空间引用（PR #1208 已修
ipc/uts 相关部分，但 net/user/cgroup 的实例化仍缺）。

### 5.3 capability bounding set

**现状**：`PR_CAPBSET_DROP` 未实现。PoC 用 `XK_SKIP_CAPS=1` 跳过整个
bounding set 校验。

**实测表现**：agent 收紧容器 capability 边界时调用 `PR_CAPBSET_DROP` 失败，
后续校验必然不通过；跳过之后容器进程保留了比 OCI spec 更宽的 capability 集合。

**影响**：这是安全缺口而非功能缺口。上游 Kata 依赖 bounding set 把
「容器不该拥有的 capability」永久移除，即使进程后来获得 root 也无法取回。
缺失意味着容器逃逸面扩大，不能作为「PoC 可接受」的长期状态。

**补齐方向**：实现 `PR_CAPBSET_DROP` / `PR_CAPBSET_READ`，语义与 Linux 一致：
只能单向丢弃、丢弃后不可恢复、跨 `execve` 保持；并与 `process/kcred` 的
capability 集合模型对齐，而不是在 syscall 层做一个假的成功返回。
验收标准是 OCI runtime 的 capability 测试集在不加 `XK_SKIP_CAPS` 时全绿。

### 5.4 内存 / CPU 热插拔 sysfs

**现状**：没有 `/sys/devices/system/memory/*/online`，也没有 CPU hotplug sysfs。
PoC 用 `XK_SKIP_ONLINE=1` 跳过 `OnlineCpuMem`。

**实测表现**：

- agent 的 `OnlineCpuMem` 读取上述路径返回 `ENOENT`，shim 据此判定 sandbox
  异常并拆除整个 sandbox，而不是降级继续。
- guest 启动日志出现 `memory block size error: NotFound`，与同一处缺失同源。

**影响**：Kata 的 sandbox 扩容路径（给运行中的 VM 加内存/CPU）不可用。
对静态规格的短生命周期容器影响有限，但对 Kubernetes 的垂直扩缩容、
`resize` 类操作是硬阻塞。注意这一项与 5.1 的 memory controller 不同：
前者是「热插拔设备模型」，后者是「资源限额」，需要分别补齐。

**补齐方向**：建立 memory block 设备模型与 `online` 属性，CPU 侧提供
`/sys/devices/system/cpu/cpuN/online`；两者都要与 ACPI/virtio 热插拔事件
联动，而不是只暴露一个可写文件。短期至少应让 `OnlineCpuMem` 在「无新设备」
时返回成功而非 `ENOENT`，避免 shim 误判拆除 sandbox。

### 5.5 virtio-fs 与 9p 成熟度

**现状**：没有 virtio-fs 驱动，容器 rootfs 只能走 9p。PR #1208 补齐的是
9p 的**读**路径，写路径与若干边界语义仍有已知缺陷。

已实测确认的部分：

- 读路径可用：`read_folio` 让动态链接、`execve`、脚本读取正常工作，
  对照修复前是 SIGBUS(135)。
- 无 `writepages` / `write_begin`：可写 `MAP_SHARED` 映射不支持，
  依赖共享写映射的程序会失败。

仍待修的 9p 缺陷（PoC 期间观察，未在本 PR 范围内）：

| 缺陷 | 影响 |
| --- | --- |
| 递归删除会跨越挂载点 | 容器清理可能误删 guest 文件 |
| 写入会卡住 | 容器 rootfs 写操作不可靠 |
| boot 非确定 | 同一镜像偶发启动失败 |
| 负 lookup 缓存缺失/不准 | 反复 stat 不存在的路径变慢或结果不一致 |
| 第二次 mount 后 readdir `EINVAL` | 多挂载场景不可用 |

**影响**：性能上，9p 无 page cache 写回，容器 rootfs 读写远慢于
virtio-fs/ext4；语义上，写映射和删除安全性不达生产要求。因此即使
cgroup/ns/cap 全部补齐，virtio-fs 仍是「可用」的必要条件。

**补齐方向**：优先实现 virtio-fs（含 DAX 可选路径），把 9p 降级为调试/兼容
通道；在 virtio-fs 落地前，至少补 `writepages`/`write_begin` 并修复上表
五项，避免把 9p 当作容器 rootfs 的长期方案。

### 5.6 容器 /proc 与 PID namespace 视图

**现状**：容器没有独立 PID namespace，agent 挂载 `/proc` 时看到 guest 的
`/proc` 已存在，于是认为「已经挂载」，容器内实际复用了 guest 的 procfs。

**实测表现**：容器内 `cat /proc/version` 失败；`ps`、`/proc/self` 等语义
也与容器预期不符。容器内列出的顶层目录（`bin`、`dev`、`proc`、`sys` 等）
来自 9p 共享的 rootfs，但 `/proc` 的内容指向 guest。

**影响**：容器内可观测性缺失，依赖 `/proc` 的工具链（监控、调试、
部分语言运行时）行为异常。这一项与 5.2 的 PID namespace 缺失是同一个根因的
两个表现：没有独立 pid ns，就没有正确的 procfs 视图。

**补齐方向**：在 PID namespace 实例化的基础上，让容器 mount `/proc` 时绑定到
该 pid ns 的 procfs 实例，而不是复用 guest 的全局实例；同时保证
`/proc/<pid>/ns/*` 的 pin 语义对容器内进程同样成立。
验收标准是容器内 `/proc/version` 可读、`/proc/1/` 指向容器 init 而非 guest init。

## 6. PoC 本地补丁：未进 PR 的临时差距

以下改动只存在于 PoC 本地，**没有进入 PR #1208**。它们让容器跑起来了，
但每一条都对应一个尚未收敛的内核差距，必须单独跟踪，不能当作已解决。

### 6.1 `sethostname()` 的 `EFAULT` 被忽略

x-kernel 上 `sethostname()` 返回 `EFAULT`。PoC 在 rustjail 的
`do_init_child` 中直接忽略该错误，容器 hostname 沿用 guest 的 UTS 值
（实测容器内 `uname` 显示 `0ea900d952e0`，而非 Kata 期望注入的容器名）。

差距本质：UTS namespace 的 hostname 写入路径对用户缓冲区的拷贝/校验有误，
或 UTS ns 未正确实例化。正确修法是修内核的 `sethostname` 实现与 UTS ns 归属，
而不是让用户态吞掉错误。

### 6.2 `/var/run/sandbox-ns/{ipc,uts}` 的 `ESRCH` 兜底

rustjail 打开这两个 sandbox ns 文件时返回 `ESRCH`，PoC 改为在子进程内
新建同名 namespace。这掩盖了 sandbox ns 文件未被正确创建/持久化的问题：
上游依赖这些文件在容器创建时 `setns` 进入同一组 ns，兜底后每个容器可能
拿到不同的 ns 实例。

### 6.3 缺 memory controller 时 `Manager::get_pids` 返回空列表

`cgroups/fs` 的 `get_pids` 在缺少 memory controller 时走到
`Option::unwrap()` panic。PoC 改为返回空列表以避免 panic。这属于
「把崩溃降级为静默错误」，真正的修复是补齐 controller（5.1）。

### 6.4 其余 PoC 开关

| 开关/补丁 | 掩盖的差距 |
| --- | --- |
| `XK_SKIP_CGROUP_RESOURCES=1` | cgroup v2 控制器缺失（5.1） |
| `XK_SKIP_NS_CREATE=1` | `unshare` flag 不支持（5.2） |
| `XK_SKIP_CAPS=1` | `PR_CAPBSET_DROP` 未实现（5.3） |
| `XK_SKIP_ONLINE=1` | 热插拔 sysfs 缺失（5.4） |
| `selinux::is_enabled()` 读 `/proc/mounts` 失败按 disabled 处理 | procfs mount 信息不完整 |
| Go shim 用 symlink 代替 bind mount | 宿主无 `CAP_SYS_ADMIN`，属环境限制 |

判定：这些补丁是**验证手段**，不是交付物。PR #1208 的边界应严格限定在
9p `read_folio` 与 procfs ns pin 等内核修复；上述用户态绕过必须拆成独立
issue，逐项用内核实现替换。

## 7. 未验证范围与结论边界

本次实测有明确的覆盖边界，引用结论时必须同时引用这些限制：

**已验证**

- containerd + Kata shim v2 + QEMU + X-Kernel guest 的完整 OCI 路径：
  `CreateSandbox` → `CreateContainer` → `StartContainer` → `WaitProcess`。
- 容器内动态 ELF（busybox）、`execve`、shell 脚本执行，退出码 `0`。
- 9p `read_folio` 读路径，以及修复前后 SIGBUS(135) / `0` 的对照。
- guest 启动期 `Unhandled IRQ 4097` 计数为 `0`。

**未验证**

- **dockerd 自身完全没有验证**。宿主没有 Docker CLI/daemon，因此只验证了
  containerd + Kata shim 的 OCI 路径；Docker → containerd → Kata 的链路、
  Docker 特有的 label/annotation 处理均未覆盖。
- 未验证 Kubernetes CRI 路径（CRI-O / containerd CRI plugin）、Pod 级
  sandbox 复用、多容器共享 sandbox。
- 未验证 cgroup 资源限制的真实生效（因为整段被 `XK_SKIP_CGROUP_RESOURCES`
  跳过），也没有性能基准数字。
- 未验证多架构：只在 x86_64 上跑通，aarch64 未测。
- 未验证长时间稳定性、并发多 sandbox、异常路径（OOM、kill -9、shim 崩溃）。
- 未验证 SELinux/AppArmor 等 LSM 路径，PoC 中 SELinux 直接按 disabled 处理。

**结论边界**

「Kata 能在 X-Kernel 上跑通容器」这一结论仅适用于上述已验证范围。
它足以证明 X-Kernel 的 syscall/VFS/9p/vsock 路径可以承载 Kata 的核心 ABI，
但不足以推断 Docker、Kubernetes 或生产负载可用。

## 8. 结论与后续优先级

**判定一：Kata 能在 X-Kernel 上把容器跑起来——已验证可行。**
PR #1208 内核作为 guest，连续两次完整跑通 OCI 容器，`ctr` 退出码 `0`；
容器内动态二进制、`execve`、脚本均经 9p `read_folio` 正常工作；
回退该修复即退回 SIGBUS(135)。这是一条可复现的因果链，不是偶然成功。

**判定二：Kata 能在 X-Kernel 上作为生产运行时——目前不行。**
卡点不是 Kata 适配，而是五项通用内核能力：cgroup v2 控制器、namespace 创建、
capability bounding set、内存/CPU 热插拔、virtio-fs。此外第 6 节的 PoC
本地补丁仍是未收敛的差距，不能计入 PR 成果。

建议的推进顺序：

| 阶段 | 目标 | 验收标准 |
| --- | --- | --- |
| P0-a | 补齐 cgroup v2 cpu/cpuset/memory/io 控制器 | 去掉 `XK_SKIP_CGROUP_RESOURCES` 后 sandbox 正常创建，`--cpus`/`--memory` 生效 |
| P0-b | 补全 `unshare`/`setns`/`clone` 的 ns flag | 去掉 `XK_SKIP_NS_CREATE` 后容器拥有独立 net/user/cgroup/ipc/uts/pid ns |
| P0-c | 实现 `PR_CAPBSET_DROP` | 去掉 `XK_SKIP_CAPS` 后 OCI capability 测试通过 |
| P0-d | memory/CPU 热插拔 sysfs | 去掉 `XK_SKIP_ONLINE` 后 `OnlineCpuMem` 成功，sandbox 不被拆除 |
| P1-a | virtio-fs 驱动 + 9p 写路径修复 | 容器 rootfs 默认走 virtio-fs；9p 可写 `MAP_SHARED` 与递归删除安全 |
| P1-b | 容器 `/proc` 绑定 pid ns | 容器内 `/proc/version` 可读，`/proc/1` 指向容器 init |
| P1-c | 用户态补丁回收 | 第 6 节所有 `XK_SKIP_*` 与 rustjail 兜底全部删除 |

其中 P0-a 到 P0-d 是「默认 Kata 配置能否启动」的门槛，建议作为下一阶段
的合并目标；P1 决定「是否好用、是否安全」。在这七项完成前，PR #1208 的
定位应表述为「打通了 Kata 在 X-Kernel 上的最小可运行路径」，而不是
「X-Kernel 已支持 Kata Containers」。

## 附录 A 复现步骤

```bash
# 1. 构建 X-Kernel；.config 需开启 KFEAT_FS_9P / VIRTIO_9P / VSOCK / VIRTIO_SOCKET
cp platforms/kplat-x86_64/qemu_defconfig .config
make defconfig
make build
# 产物：target/xkmake/kplat-x86_64/release/kernel.bzimg

# 2. 制作 guest rootfs：ext4 + /usr/bin/kata-agent（gnu 构建，已打 PoC 补丁）
#    + entry/init.sh（挂 9p、启动 kata-agent）

# 3. /etc/kata-containers/configuration.toml 指向上述 kernel/rootfs，
#    dial_timeout=45，create_container_timeout=300

# 4. 运行容器
timeout 120 ctr -n k8s.io run --rm --cgroup "" \
    --runtime io.containerd.kata.v2 \
    --rootfs /root/.tmp/busybox-rootfs kata-sec \
    /bin/busybox sh /bin/demo.sh
```

对照实验：回退 9p regular file inode 的 address space 改动（等价于 PR 修复前），
重跑第 4 步，应观察不到任何输出且退出码为 `135`；恢复后退出码回到 `0`。

注意：附录步骤依赖 PoC 本地补丁（`XK_SKIP_*`、shim 的 symlink 降级）与宿主
环境（无 `CAP_SYS_ADMIN`、无 loop mount、memlock 64 KiB），不属于上游
Kata 的标准部署流程。标准部署应在第 5、6、8 节列出的差距补齐后，
用默认 `configuration.toml` 直接运行。
---

# 第二部分：User Namespace 缺口与完整实现设计（L2）

> 本部分 scope 仅覆盖第一部分差距清单 §5.2（namespace 创建）中的 user namespace；
> cgroup（§5.1）、capability bounding set（§5.3）、热插拔（§5.4）、virtio-fs（§5.5）
> 是独立工作流，不在此设计范围内，但阶段划分（§6.8/第一部分 §8）需要对齐。

> 结论先行：本需求的驱动力是 Kata rootless；Chromium 桌面只是 userns 能力的顺带受益者，其 `--no-sandbox` 是 L2 落地前的桌面临时绕过，不是需求本体。

## 0. 一页结论

### 0.1 两个问题（原三层中的 L1 已否决）

| 层 | 问题 | 当前表现 | Linux 基线 | 影响 |
|---|---|---|---|---|
| **L0 ABI**（并入 L2 P0） | 不支持/未开放 user namespace 时返回什么 errno | `clone(CLONE_NEWUSER)` → `ENOSYS`；`unshare(CLONE_NEWUSER)` → `EOPNOTSUPP` | `CONFIG_USER_NS=n` 时返回 `EINVAL` | Chromium 的探测代码只接受 `EPERM/EUSERS/EINVAL/ENOSPC`，`ENOSYS` 直接 `PCHECK` FATAL |
| **L2 完整 userns** | 真正的 user namespace 语义 | 只有 identity + parent，无映射、无 `Cred.user_ns`、无能力域 | clone/unshare/setns、uid_map/gid_map、分层 capability、VFS 投影等 | 决定 Chromium namespace sandbox、rootless 容器和长期安全模型 |

> 原方案中的 L1（SUID 回退实测）已否决：SUID helper 是 deprecated 路径，为一套注定被 L2
> 替换的机制投入实测、验收和文档是沉没成本。过渡期桌面统一使用 `--no-sandbox`
> （安全降级，见 §5.1），移除条件是 L2 P2 ABI 开放。

### 0.2 三条不能混淆的边界

1. **这不是 A16 的能力授权缺口。** A16 已合并（`597594e57`），X11/uid-1000 Chromium 的 capset 路径已经跑通；本问题发生在更早的“创建 user namespace”探测阶段。
2. **这不是 A18 的 ptrace 缺口。** `ptrace: ENOSYS` 影响 crashpad/调试，不阻止 Chromium 启动；本问题是 `clone(CLONE_NEWUSER)` 的 errno。
3. **L0 不是 user namespace 支持。** `EINVAL` 只是让应用知道“内核不提供（或尚未开放）该特性”，它是 L2 完成前的正确 ABI 表现；任何文档、提交信息、发布说明都不应把 `EINVAL` 或 `--no-sandbox` 称为“支持 userns”。
4. **不做 SUID 过渡。** `chrome-sandbox` 是 deprecated 路径；本项目的过渡配置只有 `--no-sandbox`（显式安全降级），不投入任何 SUID 验证工作。

### 0.3 决策

- **已决策（2026-11-06）**：直接立 L2，按 §6.8 阶段推进。L0 的 errno 对齐并入 P0/P2：
  最小语义闭环（P2）完成前，`clone`/`unshare(CLONE_NEWUSER)` 必须返回 `EINVAL`
  ——这同时就是 L0 的目标状态，不需要单独项目。
- **过渡期桌面**：使用 `--no-sandbox`（明确标注安全降级）；移除条件是 L2 P2 ABI 开放。
  不做 SUID helper 验证。
- **红线**：不要只把 `clone` 改成成功却不提供映射和 `/proc` 文件；那会制造“已隔离”的假象，比返回错误更危险。

## 1. 复现与证据

### 1.1 现场

测试对象：aarch64 x-agentos rootfs，1280x800，Xorg + jwm，Chromium `142.0.7444.59-r0`。

原始 `/usr/local/bin/x11-session` 在最后一行以 `su kiosk` 启动 Chromium，参数包含 `--kiosk --incognito`，**没有** `--no-sandbox`。启动后 Chromium 在 sandbox 探测阶段 FATAL：

```text
[61:61:FATAL:sandbox/linux/services/credentials.cc:131]
Check failed: . : Function not implemented (38)
```

`38` 是 `ENOSYS`。同一镜像上加 `--no-sandbox` 后，完整 kiosk 页面可以渲染，说明 X11、GPU/llvmpipe、输入和页面加载链路本身没有阻塞；问题被缩小到 sandbox 选择。

### 1.2 证据强度与边界

已经确认：

- 去掉 `--no-sandbox` 会 FATAL，报错稳定指向 `credentials.cc:131`。
- `clone(CLONE_NEWUSER)` 在 x-kernel 当前实现中返回 `ENOSYS`。
- x-kernel 的 `unshare(CLONE_NEWUSER)` 返回 `EOPNOTSUPP`，与 `clone` 也不一致。
- `/usr/lib/chromium/chrome-sandbox` 存在，所有者为 `root:root`，模式 `4755`，是 aarch64 setuid PIE。

**尚未确认**：

- 完整 userns 实现后，Chromium 的 namespace sandbox 是否无需其他修补即可工作。

因此本文不写“完整实现后 Chromium 一定可用”；这是 P5 的验收内容。SUID 路径已按 2026-11-06
决策否决，不再作为验证对象。

## 2. 当前实现盘点

| 位置 | 当前状态 | 缺口 |
|---|---|---|
| `process/kcred/src/namespace.rs` | `UserNamespace { id, parent }`，只有 initial 单例 | 无 owner/level、无 uid/gid map、无创建子 namespace 的构造函数 |
| `process/kcred/src/credentials/model.rs` | `Cred` 有 uid/gid、groups、securebits、caps，但**没有 `user_ns`** | 凭据不知道自己属于哪个 user namespace |
| `Cred::has_cap()` | 只看 effective set | 没有 `ns_capable(target_ns, cap)` 的分层判断 |
| `core/ksyscall/src/task/clone.rs` | `NEWUSER` 在未实现集合中，返回 `ENOSYS`；校验顺序在 `CLONE_THREAD` 检查之前 | 既不符合“内核未启用 userns”的 `EINVAL`，也挡住 Linux 的 `NEWUSER|THREAD → EINVAL` |
| `core/ksyscall/src/task/namespace.rs` | `unshare(NEWUSER)` 走到 `prepare_unshare()`，返回 `EOPNOTSUPP` | 未建模隐式 `CLONE_THREAD|CLONE_FS`；未做单线程检查 |
| `process/kns/src/nsproxy.rs` | `NsProxy` 无 user 槽位；`prepare_unshare()` 明确拒绝 `NEWUSER` | 模型上这是对的：user ns 属于 `Cred`，不属于 `NsProxy`；缺的是 cred 侧实现和 `setns` 的 cred 安装 |
| `process/kns/src/nsfd.rs` | 有 `NsFd::User(Arc<UserNamespace>)` | 实际只能表示 initial；`with_namespace()` 拒绝 `User` |
| `fs/filesystems/procfs/.../root.rs` | `/proc/<pid>/ns/user` 硬编码 `initial_user_namespace()` | 没有 per-process user ns，也没有 inode 唯一性 |
| procfs | **没有** `uid_map`、`gid_map`、`setgroups` 文件 | Chromium 的映射写入没有落点 |
| `fs/kvfs/src/mount.rs` | `MntNamespace` 保存 `user_ns`，root/clone 都用 initial | 未按创建者设置 owner，也未用于挂载权限判断 |
| `fs/kvfs/src/types.rs` | `Metadata.uid/gid` 是裸 `u32` | 没有 kuid/kgid 类型，也没有 VFS 投影 |
| `core/ksyscall/src/task/credentials.rs` | `setgroups()` 只检查全局 `CAP_SETGID` | 没有 `setgroups=deny` 和 `userns_may_setgroups()` 语义 |

补充：`core/ksyscall/docs/design.md` 仍写 `NEWUSER/NEWCGROUP/NEWTIME` 返回 `ENOSYS`，`core/ksyscall/docs/security.md` 与 `process/kns/src/types.rs` 仍把 `NEWNET`/`NEWPID` 列为 unsupported；这些注释与当前代码不一致，L0 若改 errno 必须同步修正，避免下一轮误判。

## 3. Linux 语义基线

以下以 Linux v6.12 为参照；实现时以本仓库的目标内核版本为准，但语义边界不应缩水。

### 3.1 对象与生命周期

- 每个 `Cred` 通过 `cred->user_ns` 指向一个 `UserNamespace`；一个 task 的 user ns 随 `Cred` 替换，而不是随 `NsProxy` 替换。
- 新 user ns 由 `clone(CLONE_NEWUSER)` 或 `unshare(CLONE_NEWUSER)` 创建，父 namespace 是创建者的当前 user ns。
- `UserNamespace` 至少需要：parent、level、owner（创建者的 euid，父 namespace 视角）、group（创建者的 egid）、uid_map、gid_map、`setgroups` 允许标志；owner/group 必须是父 namespace 中已映射的 ID。
- `create_user_ns()` 在 chroot 中创建会返回 `EPERM`；嵌套超过内核限制返回 `ENOSPC`。
- 新 namespace 一旦发布，uid_map/gid_map 只能成功写入一次；写入失败不应留下半安装状态。
- namespace 的引用计数必须覆盖 `Cred`、`NsFd`、`/proc/<pid>/ns/user`、子 namespace 的 parent 链；不能因为 task 退出就提前释放 parent。

### 3.2 UID/GID 映射

- 新 userns 创建后，`Cred` 仍保存同一个全局 kuid/kgid，数值不变；但这些 ID 在新的 namespace 里暂时没有 inside 映射，用户态显示为 overflow ID（默认 65534）。
- `uid_map` / `gid_map` 的每行是 `inside outside count`；最多 5 个 extent；必须按 inside 排序；不能重叠、不能回绕；`(u32)-1` 非法；count 不能为 0。

> **§10 修正**：Linux 的 `UID_GID_MAP_MAX_EXTENTS` 是 340，不是 5；5 只是 base-array 快速路径。输入 extent 不要求按 inside 排序，只要互不重叠；内核按输入顺序逐项检查，超过 5 项后才在内部排序。读回顺序对 ≤5 个 extent 是输入顺序，对 >5 个是按 inside 排序。详见 §10.2。
- 写入约束：一次性、offset 0、单次写小于一页；父级 outside 区间必须已经在父 namespace 中映射；安装时把父级 ID 翻译成全局 kuid/kgid。
- 有特权的写者需要目标 namespace 的 `CAP_SYS_ADMIN`（通过 `file_ns_capable()`），并且需要对父 namespace 的 `CAP_SETUID` / `CAP_SETGID`。
- 非特权创建者有一条例外：单 extent、只映射自己的 euid/egid。gid 的例外还要求 `/proc/<pid>/setgroups` 已经写 `deny`。
- `setgroups` 文件只接受 `allow` / `deny`：`deny` 必须在 `gid_map` 之前写，且不可逆；`deny` 后 `setgroups()` 返回 `EPERM`；`allow` 不能重新打开。
- `setgroups()` 还要求 gid_map 非空且 `setgroups` 处于 allow；只有 gid_map 没有 allow 也不行。

Chromium 的 `SetGidAndUidMaps()` 先写 `deny`，再写 `gid_map`，最后写 `uid_map`，写入内容是 `uid uid 1` / `gid gid 1`，不是 `0 uid 1`。因此实现不能只支持“映射到 root”。

### 3.3 capability 是分层的

- 进入新 user ns 时，内核给该 `Cred` 安装该 namespace 内的 full capability set；但这份能力只对**该 namespace 及其后代**有效，对父 namespace 和 initial namespace 无效。
- 判断不是“看 effective set 有没有这一位”，而是 `ns_capable(target_ns, cap)`：目标必须是当前 user ns 或其后代，并且当前 effective set 含该位。
- 因此 `Cred::has_cap()` 的扁平语义只能用于“目标就是当前 namespace”的场景；任何针对文件、挂载、网络、IPC、PID、进程的检查都必须传入对象所属的 user ns。
- `CAP_SYS_ADMIN` 在新 userns 中允许创建嵌套 userns、挂载允许 `FS_USERNS_MOUNT` 的文件系统等，但不允许操作宿主全局资源。
- 新 userns 的 full caps 不自动给 `CAP_SETUID` 越过父 namespace 的映射权限；映射写入仍按 3.2 的父级规则检查。
- 这套模型会改变所有特权检查的调用方式，是本项目做完整 userns 时最大的一处横切改动，不是给 `Cred` 加一个字段就结束。

### 3.4 clone / unshare 的约束

- `clone(CLONE_NEWUSER)` 允许非特权调用；创建者不能处于 chroot 中，父级必须能映射创建者的 euid/egid。
- `clone(CLONE_NEWUSER | CLONE_THREAD)` 在 Linux 上返回 `EINVAL`；user namespace 必须与线程组共享，不能在一个线程里单独换 namespace。
- `unshare(CLONE_NEWUSER)` 在进入 `check_unshare_flags()` 之前会隐式加上 `CLONE_THREAD | CLONE_FS`；因此：
  - 调用者必须是单线程，否则 `EINVAL`；
  - `fs_struct` 会被复制，调用者不再与别人共享 fs；

> **§10 修正**：`NEWUSER` 确实隐式加上 `CLONE_THREAD | CLONE_FS`，但 `unshare_fs()` 只在 `CLONE_NEWNS` 已设置或 `fs->users != 1` 时复制 `fs_struct`；私有 fs 不会被无条件复制。`NEWUSER` 单独不会改变 mount namespace 的 owner，只有 `NEWNS` 才创建新 mount namespace。详见 §10.4.3。
  - 即使显式只传 `CLONE_NEWUSER`，也不能跳过这两条。
- `unshare` 还要保证“要么全部生效，要么全部不生效”：namespace、fs、cred 的提交顺序必须可回滚；不能出现新的 `Cred.user_ns` 已安装、但 `/proc` 或映射还没准备好的中间态。
- 未启用 `CONFIG_USER_NS` 的内核，`clone` / `unshare` 返回 `EINVAL`，不是 `ENOSYS`。

### 3.5 setns 到 user namespace

`userns_install()` 的拒绝条件必须逐条实现：

- 目标就是当前 user ns：`EINVAL`（不允许靠“重进同一个 ns”重新获得 full caps）。
- 当前线程组非空：`EINVAL`（user ns 必须线程组共享）。
- `current->fs->users != 1`：`EINVAL`（避免共享 fs 的进程进入不同 user ns）。
- 对目标 user ns 没有 `CAP_SYS_ADMIN`：`EPERM`。
- 由于 capability 只能向下覆盖，实际可进入的只有当前 namespace 的后代；进入祖先会被 `EPERM` 挡住。
- 成功后替换的是 `Cred.user_ns` 和该 ns 的 capability 集，**不改变** uid/gid 数值；挂载 namespace 仍保留它原来的 owner，不会因为 cred 切换而改属主。
- 因此 `NsProxy` 不需要新增 user 槽位；`setns(user_fd)` 应走 `apply_setns` 的 cred 分支，而不是 `with_namespace()`。

### 3.6 VFS 与 /proc 的投影

- 文件系统内部保存的是全局 kuid/kgid（Linux 的 `kuid_t` / `kgid_t`），不是某个 namespace 的 inside ID。
- syscall 边界负责翻译：用户传入的 uid/gid 用当前 `Cred.user_ns` 的 map 转成全局 ID；`stat()`/`getdents()` 等返回时用调用者的 map 转回 inside ID；没有映射就返回 overflow ID（默认 65534）。
- 文件权限、`chown`、`setuid` 位清理、`mknod` 等检查要针对 inode 所属的 user ns（Linux 的 `sb->s_user_ns` / inode 的属主语义），不能只看当前进程的 effective set。
- 没有 `s_user_ns` 或等价概念，就无法正确判断“这个文件属于哪个 namespace 的 root”；x-kernel 当前 `Metadata.uid/gid` 是裸 `u32`，这是完整实现的硬前提。
- `/proc/<pid>/uid_map`、`gid_map`、`setgroups` 必须按目标进程的 user ns 展示，并执行各自的读写权限；`/proc/<pid>/ns/user` 必须返回该进程真实的 ns，而不是 initial 单例。
- `/proc/<pid>/status` 的 Uid/Gid、`/proc/<pid>/stat` 的字段、ptrace/信号/进程可见性检查也要使用同一套 user ns 规则；否则会出现“stat 显示 65534，但权限仍按全局 root 算”的裂缝。

### 3.7 namespace 所有权与限制

- mount、net、ipc、pid、uts、cgroup、time namespace 都有自己的 owner user ns；对它们的操作要检查目标 ns 的 owner，而不是检查 current 的 user ns 是否 root。
- 创建这些 namespace 通常要求当前 user ns 的 `CAP_SYS_ADMIN`；创建出来的 namespace 归当前 user ns 所有。
- `/proc/<pid>/ns/*` 的 `NS_GET_USERNS` / `NS_GET_PARENT` 等 ioctl 也依赖 owner 关系；完整实现应把它们纳入同一套模型，而不是只修 `/proc/.../ns/user` 的显示。
- x-kernel 已有 `MntNamespace.user_ns`，但 root 和 clone 都填 initial；这只能算“预留字段”，不是所有权实现。
- 内核还有嵌套深度和每用户 namespace 数量限制（6.12 在父级 level 达到 32 时返回 `ENOSPC`；另有 ucounts 限制）。x-kernel 即使暂不实现 sysctl，也必须有明确的计数和失败路径，不能无限创建。

> **§10 修正**：精确检查是 `parent->level > 32` 才返回 `ENOSPC`；`parent->level == 32` 会成功，子级 `level == 33`。计数在 chroot 和映射检查之前预留，任何后续失败都必须回滚。详见 §10.2。
- 未映射 ID 的 overflow 值默认 65534；若实现 `overflowuid`/`overflowgid` sysctl，需保证 VFS、procfs、stat 使用同一来源。

### 3.8 Chromium 的实际调用序列

Chromium 142 的选择逻辑可以压缩成：

```text
CanCreateProcessInNewUserNS()
  ├─ true  → namespace sandbox
  └─ false → chrome-sandbox 路径非空 ? SUID sandbox : LOG(FATAL) "No usable sandbox!"
```

`CanCreateProcessInNewUserNS()` 先 `fork`，子进程 `clone(CLONE_NEWUSER | SIGCHLD)`；失败时调用 `CheckCloneNewUserErrno()`。该函数只允许 `EPERM`、`EUSERS`、`EINVAL`、`ENOSPC`，其他 errno 走 `PCHECK`，`ENOSYS` 就是当前 FATAL 的来源。

`clone` 成功也不代表探测成功。子进程还要：

1. 写 `/proc/self/setgroups` 为 `deny`（如果该文件存在）；
2. 写 `/proc/self/gid_map`；
3. 写 `/proc/self/uid_map`；
4. 用 `capset` 清空能力；
5. 再 `unshare(CLONE_NEWUSER)` 一次，验证嵌套可用；
6. 正常退出。

父进程只有看到子进程退出码为成功，才返回 true。也就是说：**只让 `clone` 成功、但没有 map 文件，探测仍然是 false**，只是不再 FATAL，随后落到 SUID 路径。

SUID 路径（`chrome-sandbox`）依赖一长串 syscall（chroot、`clone(CLONE_FS)`、`prctl`、
`setresuid/setresgid`、`waitid`、`clone(CLONE_NEWPID|NEWNET)`），且是 deprecated 机制。
本项目已于 2026-11-06 决策**不投入 SUID 验证**：Chromium 在 L2 完成前的过渡期统一
`--no-sandbox`；SUID 细节仅在此记录背景，不再展开验收标准。

## 4. 行为矩阵

| 操作 | Linux（userns 可用） | Linux（`CONFIG_USER_NS=n`） | x-kernel 当前 | L2 目标 |
|---|---|---|---|---|
| 非特权 `clone(CLONE_NEWUSER)` | 成功，子进程进入新 ns | `EINVAL` | `ENOSYS` | 成功 |
| `clone(CLONE_NEWUSER\|CLONE_THREAD)` | `EINVAL` | `EINVAL` | `ENOSYS`（先命中未实现检查） | `EINVAL` |
| 单线程 `unshare(CLONE_NEWUSER)` | 成功 | `EINVAL` | `EOPNOTSUPP` | 成功 |
| 多线程 `unshare(CLONE_NEWUSER)` | `EINVAL` | `EINVAL` | `EOPNOTSUPP`（未建模隐式 THREAD） | `EINVAL` |
| 读 `/proc/self/uid_map` | 显示映射 | 文件不存在或空 | 文件不存在 | 正确显示 |
| 写 `uid_map`（非特权自映射） | 成功一次 | 不适用 | 无文件 | 成功一次，重复写 `EPERM` |
| 写 `gid_map`（未 deny setgroups） | `EPERM` | 不适用 | 无文件 | `EPERM` |
| `setgroups` 在 deny 后 | `EPERM` | 不适用 | 只受全局 `CAP_SETGID` 限制 | `EPERM` |
| 新 ns 中 `getuid()` | 按 map 显示 | 不适用 | 不适用 | 按 map 显示 |
| 新 ns 中 `stat()` 未映射 ID | 65534 | 不适用 | 原样 `u32` | 65534 |
| `setns` 到同一 user ns | `EINVAL` | 不适用 | 全局 `CAP_SYS_ADMIN` 后拒绝 `NsFd::User` | `EINVAL` |
| `setns` 到祖先 user ns | `EPERM` | 不适用 | 同上 | `EPERM` |
| `/proc/<pid>/ns/user` | 指向真实 ns | 不存在或 initial | 永远 initial | 指向真实 ns |
| 子 userns 对父资源做 `CAP_*` | 无权限 | 不适用 | 无此模型 | 无权限 |

注：x-kernel 的 `setns` 当前先要求全局 `CAP_SYS_ADMIN`，再在 `NsProxy::with_namespace()` 拒绝 `NsFd::User`；因此非 root 调用会先得到 `EPERM`，root 调用得到 `EOPNOTSUPP`。L2 必须把权限检查改成目标 namespace 的 `CAP_SYS_ADMIN`，而不是保留全局门槛。

## 5. 方案与决策

### 5.1 过渡方案：`--no-sandbox`

在 `x11-session` 的 Chromium 参数里加 `--no-sandbox`，直到 L2 P2 ABI 开放。

- 优点：零内核改动，立即恢复桌面；D6 阶段已经用它完成过端到端验证。
- 代价：关闭 Chromium 的进程沙箱，渲染器直接以完整用户权限运行；这是安全降级，不是修复。
- 约束：作为**唯一记录在案的过渡配置**使用；禁止作为产品终态、作为“user namespace 已支持”的证据；移除条件是 L2 P2 ABI 开放，届时必须从镜像和脚本中移除。

### 5.2 目标方案：完整 user namespace（L2）

按 Linux 语义实现 `Cred.user_ns`、ID 映射、分层 capability、VFS/proc 投影、namespace 所有权、`setns` 和限制。L0 的 errno 对齐（`ENOSYS`/`EOPNOTSUPP` → `EINVAL` 及非法组合语义）并入本方案的 P0/P2，不单独成项。

- 优点：唯一能同时解决 Chromium namespace sandbox、rootless 容器和长期权限模型的方案；不依赖 deprecated SUID helper。
- 代价：横切 `kcred`、`kns`、`ksyscall`、`kvfs`、`procfs`、`knet`、`kpid`、`kipc`、mount 和 exec；需要大量负向安全测试；不是“一两天补一个 syscall”。
- 适用：产品目标是长期安全、rootless 或多租户隔离。
- 关键约束：不能先让 `clone` 返回成功再逐步填坑；在最小语义闭环完成前，宁可继续返回 `EINVAL`。

### 5.3 已否决：errno + SUID 回退（原方案 B / L0+L1）

曾考虑“改 errno 为 `EINVAL` 让 Chromium 落到 SUID 路径”作为快速过渡。**否决理由**：

- SUID helper 是 deprecated 路径，攻击面大于 namespace sandbox；
- x-kernel 未验证过 helper 依赖链，实测/验收/文档成本沉没在一套注定被 L2 替换的机制上；
- 只改 errno 不验证 SUID，会把 `credentials.cc` 的 FATAL 换成 `No usable sandbox!` 的 FATAL，并不能恢复桌面。

过渡需求由 §5.1 覆盖。

| 维度 | `--no-sandbox` | L2 完整 userns（含 P0 errno 对齐） |
|---|---|---|
| 内核改动 | 无 | 大 |
| 过渡期桌面可用 | 立即 | P2 ABI 开放后 |
| Chromium 隔离 | 无 | 有，namespace sandbox |
| 长期可维护 | 否 | 是 |
| 是否 userns 支持 | 否 | 是 |

### 5.4 推进路径

1. **P0**：errno 对齐合入（`EINVAL` + 非法组合语义 + 文档同步）；过渡期桌面记录 `--no-sandbox`（顺带缓解 Chromium FATAL，不是验收目标）。
2. **直接立 L2**：把本文第 6 节当作设计输入，按 §6.8 的 P1–P5 顺序推进。**首个验收场景是 Kata rootless 容器**（配合 `docs/container-architecture.md` 与本文第一部分的范围约束）；Chromium namespace sandbox 作为顺带受益场景，在 P5 一并回归。
3. 期间不再评估、验证或记录任何 SUID 相关状态。

## 6. 完整实现设计

### 6.1 数据模型

建议把 user namespace 做成 `kcred` 的强类型对象，而不是 `kns` 的一个旁路：

```rust
pub struct UserNamespace {
    id: NamespaceId,
    parent: Option<Arc<UserNamespace>>,
    owner: Kuid,              // 创建者 euid，父 namespace 视角
    group: Kgid,              // 创建者 egid
    level: u32,
    uid_map: Once<Arc<IdMap>>, // 一次性发布，读路径无锁
    gid_map: Once<Arc<IdMap>>,
    setgroups_allowed: AtomicBool,
}
```

```rust
pub struct IdMap {
    extents: [IdExtent; 5],   // 已排序
    len: usize,
}

pub struct IdExtent {
    inside: u32,
    outside: u32,
    count: u32,
}
```

- `Cred` 增加 `user_ns: Arc<UserNamespace>`；`root()` 和 initial 进程使用 `initial_user_namespace()`。
- `IdMap` 提供 `inside_to_global` / `global_to_inside`，未命中返回 `None`，由调用方决定 overflow；不要在 map 内部静默夹到 65534。
- `UserNamespace::new_child()` 只做结构创建；映射和 `setgroups` 状态在后续写入，失败不发布。
- `parent` 必须持有强引用；父子链的释放顺序要能被 `Arc` 自然处理，不能靠手工计数。

> **§10 修正**：数据模型放在 `process/kuserns`，由 `kcred`/`kns`/`kvfs`/`kidentity` 复用；`uid_map`/`gid_map` 不能只用 `Once`，因为第二次写入必须返回 `EPERM` 而不是 panic 或静默失败。`owner`/`group` 是全局 `Kuid`/`Kgid`，并需记录 `parent_could_setfcap`。详见 §10.2。

### 6.2 capability 检查收敛到一个入口

新增一个中心函数，所有特权路径都走它。语义必须完整对齐 Linux `cap_capable()`
（security/commoncap.c），不是简单的“祖先判定 + 查表”：

```rust
fn ns_capable(target: &UserNamespace, cap: u32) -> bool {
    let current = current_cred();
    let mut ns = target;
    loop {
        // 目标就是当前 cred 所属 ns：查 effective set
        if Arc::ptr_eq(ns, current.user_ns()) {
            return current.has_cap(cap);
        }
        // 目标层低于当前 cred 所在层：不可能拥有
        if ns.level() <= current.user_ns().level() {
            return false;
        }
        // 规则 ①：直接子 ns 的 owner 在父 ns 中拥有全部能力
        if Arc::ptr_eq(ns.parent(), current.user_ns()) && ns.owner() == current.euid() {
            return true;
        }
        // 规则 ②：沿祖先链上溯；祖先中的能力对后代生效，
        // 最终要么命中 current.user_ns 查表，要么因 level 条件退出
        ns = ns.parent();
    }
}
```

> **实现状态（2026-11-07）**：`Cred::has_cap_in` 已按上述循环实现，
> `UserNamespace` 增加 `level` 并强制 Linux 的 `parent->level > 32 → ENOSPC`；
> 单测覆盖 owner 直系授予、祖先/旁系不可达、间接 owner 对后代的覆盖与深度耗尽。
> cpu.max 已从“权重坍缩”升级为 fair 类的硬摘除（parked 实体保留在就绪树中
> 但不可选中，周期滚动后恢复），与 Linux 的 dequeue 语义一致。

- 两条隐式规则缺一不可：**① namespace owner 对自己的直接子 ns 拥有隐式全能力**
  （`ns->parent == cred->user_ns && uid_eq(ns->owner, cred->euid)`）；
  **② 祖先 ns 中的能力向所有后代传递**（沿 parent 链上溯）。
  只做“ancestor-or-self + effective set”的朴素实现，会导致 root 创建子 userns
  后反而无法管理它。
- 不要保留“默认对 current 检查”的 `capable(cap)` 语法糖；如果保留，它必须等价于 `ns_capable(&initial_user_ns, cap)`，用于真正的全局资源。对当前 ns 的检查显式写 `ns_capable(current.user_ns(), cap)`，避免把子 userns 的 full caps 误当全局 root。
- 对全局资源，传资源所属 ns（通常是 initial），不要传 current；否则嵌套 userns 里的 full caps 会误判为有权。
- 逐步删除特权路径上的扁平 `has_cap()` 调用；保留它只用于“已经确认目标就是当前 ns”的少数地方，并在注释里写明。
- `capget`/`capset` 仍然操作 `Cred` 的 capability 集；变的是“这些位对谁有效”，不是集合本身的存储格式。
- 进入新 userns 时（`set_cred_user_ns()` 语义）除了装 full set，还必须同步清零：
  `securebits` 置默认值、`cap_inheritable`/`cap_ambient` 置空（Linux user_namespace.c:43-60）。
- 前提检查：`Cred` 必须已建模完整五集合（inheritable/permitted/effective/bounding/ambient）
  与 `no_new_privs`；缺任一集合，exec 的 capability 计算都会出错。字段文档化做法可参照
  Asterinas `credentials_.rs`。
- A16 的 capset 测试必须全部保留；新增两组测试：“新 userns full caps 不能操作父资源”的负向测试，
  以及“父 ns owner 对直接子 ns 拥有全能力”的正向测试。

### 6.3 `/proc` 映射文件

在 procfs 的 task node 下新增三个文件，按目标进程的 `Cred.user_ns` 展示：

- `uid_map`：`inside outside count`，最多 5 行，按 inside 排序。
- `gid_map`：同上。
- `setgroups`：`allow` 或 `deny`；只接受这两个词和尾随空白，写入长度小于 8 字节。

> **§10 修正**：`uid_map`/`gid_map` 不是“最多 5 行”；完整规则、open 时捕获目标 ns、`file->f_cred` 权限、340 上限、输入顺序和读回投影见 §10.2 与 §10.4.4。

写入路径：

```text
open /proc/<pid>/uid_map
  → 校验 pid 对应的 user ns
  → 校验 file_ns_capable(CAP_SYS_ADMIN)
  → 解析并校验 5 个 extent
  → 校验父级映射
  → 一次性安装
```

- `map_write` 必须拒绝第二次成功写入、offset 非 0、跨页写、空 map、回绕、重叠、`(u32)-1`。
- 非特权自映射例外要单独测试：单 extent、只映射 owner 的 euid/egid；gid 还要求 `setgroups=deny`。
- `setgroups=deny` 必须在 `gid_map` 之前生效；之后不可逆；`allow` 不能把 deny 改回来。
- `/proc/<pid>/ns/user` 改为返回进程真实 ns，并保证同一 ns 的 inode 身份稳定；不能再硬编码 initial。
- 读投影必须来自真实 map：**禁止在没有真实映射时硬编码输出默认映射行**（例如 `0 0 4294967294`）。
  Asterinas 的 procfs `uid_map.rs` 就是这种反面样本——读文件输出伪造的全映射、权限位标 `u+w`
  却没有写实现，比不提供文件更危险，会制造“已映射”假象。

### 6.4 clone / unshare / setns 的接入点

**clone**

- 把 `NEWUSER` 从“未实现集合”里移出，单独处理；`NEWCGROUP` / `NEWTIME` 仍可保持未实现。
- 先判 `NEWUSER` 相关的非法组合，再判其他检查；不要因为 `NEWUSER` 未实现就吞掉 `EINVAL`。
  按 Linux 语义（参照 Asterinas clone.rs 的校验集合）需要覆盖：
  - `NEWUSER | THREAD → EINVAL`
  - `NEWUSER | FS → EINVAL`
  - `NEWUSER | PARENT → EINVAL`
  - `NEWUSER | PIDFD → EINVAL`
- 非特权允许 `NEWUSER`；其他 namespace 位仍按当前 user ns 的 `CAP_SYS_ADMIN` 检查。
- 子进程创建新 `Cred`，设置 `user_ns = child_ns`，uid/gid 保持原值；父进程不变。
- 任何后续步骤失败都要回滚，不能留下“子进程有 ns、父进程以为失败”的半状态。

**unshare**

- 在 flag 校验前加上 `CLONE_THREAD | CLONE_FS`，再检查 `thread_group_empty(current)`；多线程直接 `EINVAL`。
- `prepare_unshare()` 增加 user ns 分支：创建 ns、复制/私有化 fs、准备 cred；全部成功后一次性提交。
- `NsProxy` 不新增字段；user ns 的提交走 `Cred`。
- 错误映射：`EINVAL` 用于参数/线程/fs 约束，`EPERM` 用于 capability/chroot/owner 映射失败，`ENOSPC` 用于层级/数量限制。

**setns**

- `NsFd::User` 不再走 `with_namespace()`；在 `apply_setns` 里走 cred 安装。
- 按 3.5 的顺序检查 same/thread_group/fs_users/`ns_capable`。
- 成功后只替换 `Cred.user_ns` 和能力集；不要改 uid/gid，也不要改挂载 ns 的 owner。
- 其他 namespace 的 `setns` 仍按各自 owner 的 `CAP_SYS_ADMIN` 检查；这一步可以和 user ns 分开落地，但不能继续用全局 root 门槛。

### 6.5 VFS 与 exec

- 引入 `Kuid` / `Kgid` 新类型（或等价强类型），先把 `Metadata.uid/gid`、inode、`chown`、`stat` 的边界收紧；裸 `u32` 的比较必须被编译器挡住。
- 强类型应带 `INVALID`（`(u32)-1`）常量与 overflow 语义（参照 Asterinas 的 `Uid` wrapper）：
  `INVALID` 是非法映射值，不能参与数值比较或投影；`(u32)-1` 在 map 输入中也必须被拒绝。
- 给 superblock 增加 `s_user_ns`（或文件系统等价的 owner 概念），用于 `chown`、`mknod`、`setuid` 位清理和挂载权限判断。
- syscall 入参用当前 map 转全局；返回用调用者 map 转 inside；未映射返回 65534。
- `setuid`/`setgid`/`setfsuid`/`setfsgid` 要按目标 ID 是否在当前 map，以及 `ns_capable(old->user_ns, CAP_SETUID/CAP_SETGID)` 是否成立来判断；不能只比较数值。
- `exec` 的 setuid/setgid、文件 capability、`no_new_privs` 与 userns 的交互要单独立测试；文件 capability 可以列为非目标，但不能让现有 setuid 语义悄悄失效。
- `ptrace_may_access`、信号权限、`/proc/<pid>` 可见性统一使用 user ns 关系，避免跨 ns 调试/读取泄露。

### 6.6 namespace 所有权

- 建议在 `kns` 引入统一的 namespace 公共 trait（类似 Asterinas 的 `NsCommonOps`）：
  每种 ns 类型提供稳定的 `/proc/<pid>/ns/*` dentry/inode 身份（stashed dentry）、
  `owner_user_ns()`（对应 `NS_GET_USERNS`）、`parent()`（对应 `NS_GET_PARENT`）。
  userns 只是该 trait 的一个实现；避免把 inode 身份、owner 查询分散在各子系统的硬编码里。
  注意安全约束：**占位实现宁可返回 `EPERM`/`unimplemented`，也不返回看似成功的宽松默认值**
  （Asterinas 的 `owner_uid()` 无条件返回 root、`is_same_or_ancestor_of()` 退化为
  指针相等，是反面样本，不能模仿）。
- 每个 namespace 创建时记录 owner user ns；`MntNamespace.user_ns` 从“预留”变成真正参与判断的字段。
- 创建 mount/net/ipc/pid/uts/cgroup/time namespace 时，owner 设为当前 `Cred.user_ns`。
- 操作目标资源时检查目标 ns 的 owner：例如向某个 net ns 加接口、向某个 mnt ns 挂载、向某个 pid ns `setns`。
- 已有的 `NsProxy` 克隆逻辑要显式传播 owner；不能因为调用者是 root 就默认 owner 是 initial。
- `/proc/<pid>/ns/*` 的 identity、`NS_GET_USERNS`、`NS_GET_PARENT` 与 owner 保持一致。
- 先做 mount + pid + net 三条最常用的链路，再补 ipc/uts/cgroup/time；不要一次性铺开却没有负向测试。

### 6.7 安全不变量

实现和 review 时必须逐条检查：

1. 进入子 userns 不会获得对父/initial 资源的任何 capability。
2. 任何 `clone`/`unshare` 成功进入 userns 的路径，都已经有可用的 `Cred.user_ns`、映射和 `/proc` 投影；不接受“先成功、后补文件”。
3. 任何特权检查都能回答“目标对象属于哪个 user ns”，不再默认当前进程的 effective set 就是全局权威。
4. 未映射 ID 只以 overflow 呈现，绝不静默截断或回退到全局 ID。
5. `setns` 不能进入祖先；`setgroups` 不能从 deny 恢复；`gid_map` 不能绕过 deny。
6. 多线程进程不能单独换 user ns；共享 fs 的进程不能 `setns` 到不同 user ns。
7. 新 namespace 不能通过 mount/net/pid 操作反向影响 owner 之外的资源。
8. 失败路径不留下部分 namespace、部分映射或部分 cred。

### 6.8 分阶段落地

| 阶段 | 内容 | 可交付状态 |
|---|---|---|
| P0 | errno 对齐合入（`EINVAL`、非法组合、文档同步）；过渡期桌面记录 `--no-sandbox` | ABI 语义与 Linux `CONFIG_USER_NS=n` 一致；`ENOSYS` FATAL 消失 |
| P1 | 数据模型、`Cred.user_ns`、`ns_capable()`、内部测试 | 内部可用；**不暴露** `clone/unshare` 成功 |
| P2 | `uid_map`/`gid_map`/`setgroups`、最小 clone/unshare 垂直切片 | 可创建、可映射、可读 `/proc`；负向测试通过后才开 ABI |
| P3 | VFS/proc 投影、superblock owner、stat/chown/setuid | 普通文件操作在映射下正确 |
| P4 | `setns`、mount/net/pid/ipc owner、exec/ptrace 权限 | namespace 之间不可越权 |
| P5 | Kata rootless 场景验收（主）、Chromium namespace sandbox 回归（顺带）、压力/长跑 | 可以对外宣称支持 |

P1/P2 的顺序很重要：先把“cred 知道自己在哪、检查知道目标在哪”打通，再让 syscall 返回成功；反过来会制造一段时间的假隔离。

### 6.9 明确非目标

以下不应被塞进第一版，也不应成为“userns 已支持”的隐含承诺：

- idmapped mounts、所有文件系统的 `FS_USERNS_MOUNT`（注：Kata 若强依赖 idmapped mounts，
  属于 P5 之后的能力扩展，不阻塞内核侧 userns 闭环；需求边界见第一部分 §5.5 与 §8）；
- 文件 capability、LSM/AppArmor/SELinux 的完整 userns 策略；
- cgroup namespace 委派、rootless cgroup v2 全功能；
- time namespace、keys、BPF 的 userns 语义；
- rootless 容器运行时的网络、存储、镜像层等用户态部分（由 Kata/containerd 侧负责，
  内核只提供 userns 原语）；
- 为了 Chromium 单独开的后门或“只对 chrome 生效”的特殊分支；
- 重写 SUID helper。

这些可以后续做，但必须在文档和提交信息里保持“未实现”，不能让调用方误以为已经覆盖。

## 7. 测试与验收

### 7.1 P0（errno 对齐）

- 非特权 `clone(CLONE_NEWUSER)`：返回 `EINVAL`，且当前进程的 `Cred` 不变。
- `clone(CLONE_NEWUSER | CLONE_THREAD)`：返回 `EINVAL`，不能被“未实现”分支吞成 `ENOSYS`。
- `clone` 的其他非法组合同样返回 `EINVAL`：`NEWUSER | CLONE_FS`、`NEWUSER | CLONE_PARENT`、
  `NEWUSER | CLONE_PIDFD`。
- 单线程 `unshare(CLONE_NEWUSER)`：返回 `EINVAL`，不产生任何 namespace。
- 多线程 `unshare(CLONE_NEWUSER)`：返回 `EINVAL`，不能返回 `EOPNOTSUPP`。
- 带 `--enable-logging=stderr` 启动 Chromium：不再出现 `credentials.cc:131` 的 `Function not implemented`；过渡期使用 `--no-sandbox`，不应看到任何 `ENOSYS` FATAL。
- 单元测试：`tests_clone.rs` / `tests_namespace.rs` 同步更新，明确 `EINVAL` 与 `ENOSYS` 的边界。

### 7.2 L2（完整 userns）

功能正向：

- 非特权 `clone`/`unshare` 创建 userns；`getuid()`/`getgid()` 按 map 显示。
- `/proc/self/uid_map`、`gid_map`、`setgroups` 可读写；`setgroups=deny` 后 `gid_map` 可写，`setgroups()` 返回 `EPERM`。
- 新 ns 内 full caps 可创建嵌套 userns、可挂载允许的 fs；对父资源操作失败。
- `stat()` 对映射 ID 显示正确；未映射 ID 显示 65534。
- `setns` 进入后代 userns 成功；进入同一/祖先失败；多线程/共享 fs 失败。

Kata rootless 主场景（P5 验收核心）：

- 非 root 用户经 Kata/containerd 创建容器：`clone/unshare(CLONE_NEWUSER)` 成功，
  `uid_map`/`gid_map` 按 rootless 映射安装，容器内 root 投影为宿主非特权 ID。
- 容器内进程对宿主资源无能力越权（mount、netns、ptrace 负向测试）。
- 与 `docs/container-architecture.md`、第一部分差距清单（§5）与 PoC 补丁清单（§6）逐项对账，
  明确本设计覆盖/不覆盖的条目。

Chromium namespace sandbox（顺带回归）：

- Chromium 在隐藏 `chrome-sandbox` 或 `--disable-setuid-sandbox` 时仍能选择 namespace sandbox，页面正常。

负向安全：

- `uid_map` 第二次写 `EPERM`；offset 非 0、跨页、空、回绕、重叠、`(u32)-1` 均拒绝。

> **§10 修正**：同一个 fd 顺序写第二次时 `ppos != 0`，Linux 返回 `EINVAL`；关闭后重新 open、在 offset 0 写一个已经安装的 map 才返回 `EPERM`。`count >= PAGE_SIZE` 就拒绝，不是“跨页”才拒绝。乱序但互不重叠的 extent 是合法的，不能写测试断言拒绝。详见 §10.4.4。
- 未 deny 的 `gid_map` 非特权写入 `EPERM`。
- `setns` 到祖先 `EPERM`；`clone(NEWUSER|THREAD)` / 多线程 `unshare` `EINVAL`。
- 子 userns 的 `CAP_SYS_ADMIN` 不能 mount 宿主资源、不能改宿主 netns、不能 ptrace 父 ns 进程。
- 超过嵌套/数量限制返回 `ENOSPC`/`EUSERS`，不泄漏半成品。

### 7.3 回归

- A16 capset 全量测试；setuid/setgid、文件权限、`/proc` 读取、ptrace 现有测试。
- 桌面：Xorg + jwm + Chromium kiosk 冷启动 3 次；长跑至少 30 分钟；无 `--no-sandbox`。
- X11/输入/GPU 不受影响；`/proc/<pid>/status` 与 `stat` 结果一致。
- 非 root、非多线程、多线程三种调用上下文分别覆盖；特别是 Chromium 的 fork 后单线程假设。

### 7.4 完成定义

- **P0 完成**：不支持/未开放 userns 时 `clone`/`unshare` 返回 `EINVAL`；Chromium 不再因 `ENOSYS` FATAL；过渡期桌面以 `--no-sandbox` 运行并记录在案。P0 不等于 userns 支持。
- **L2 完成**：第 7.2 的正向和负向测试全过；**Kata rootless 容器可用**（主验收）；Chromium namespace sandbox 回归通过（顺带）；`--no-sandbox` 过渡配置移除；A16/桌面无回归；文档不再把 `EINVAL` 或 `--no-sandbox` 写成 userns。

## 8. 待决事项

1. Kata rootless 的最小内核依赖集是什么？（是否要求 idmapped mounts、cgroup ns 委派？）
   需与第一部分差距清单（§5.1 cgroup、§5.3 bounding set、§5.5 virtio-fs）对账后冻结 P5 范围。
2. VFS 是否接受引入 `Kuid/Kgid` + `s_user_ns` 的横切改造？如果不接受，L2 不能算完整。
3. namespace 数量/嵌套限制是否实现 sysctl，还是先固定上限？固定上限也要有明确错误码和测试。
4. 现有 mount namespace 的 owner 如何迁移？root/clone 都填 initial 的历史行为是否需要兼容开关？
5. P0 的 errno 对齐在哪个内核版本合入？过渡期 `--no-sandbox` 的镜像/脚本改动由谁跟进？

## 9. 文件索引

- `x-desktop-main/process/kcred/src/namespace.rs`：`UserNamespace` 定义。
- `x-desktop-main/process/kcred/src/credentials/model.rs`：`Cred`、`has_cap()`。
- `x-desktop-main/process/kns/src/nsproxy.rs`：`NsProxy`、`prepare_unshare()`、`with_namespace()`。
- `x-desktop-main/process/kns/src/nsfd.rs`：`NsFd::User`。
- `x-desktop-main/core/ksyscall/src/task/clone.rs`：clone flag 校验。
- `x-desktop-main/core/ksyscall/src/task/namespace.rs`：unshare/setns。
- `x-desktop-main/core/ksyscall/src/task/credentials.rs`：`setgroups()`。
- `x-desktop-main/fs/filesystems/procfs/src/task_nodes/root.rs`：`/proc/<pid>/ns/user`。
- `x-desktop-main/fs/kvfs/src/mount.rs`、`types.rs`：mount owner、`Metadata.uid/gid`。
- `x-desktop-main/process/kns/docs/design.md`：现有 userns 非目标说明。
- `x-desktop-main/docs/container-architecture.md`：rootless/ID mapping 的架构约束。

## 附录 B：Kata e2e 实测状态（2026-11-07）

实测宿主：kylin212（128C/502G，KVM + vhost-vsock，宿主 cgroup v1），guest 内核为本分支
x86_64 构建（KFEAT_FS_9P / VIRTIO_9P / VSOCK / VIRTIO_SOCKET），guest rootfs 为
`/root/.tmp/xk-kata-rootfs.img`（init 用 musl 静态工具预挂 9p 到 `/mnt/kata-shared`，
PoC agent 将 `/run/kata-containers/shared/containers` 软链过去），shim 为 PoC 补丁版
（bind EPERM 时复制 rootfs、cgroup 创建失败时 noop、`XK_KATA_DEGRADE_ROOTFS=1` 强制
复制布局），容器 rootfs 为 `/root/.tmp/busybox-rootfs`。

### B.1 已通过的阶段

1. QEMU 启动、guest 内核启动、`XK_9P_OK`（init 侧 9p 挂载成功）。
2. kata-agent 启动，sandbox 创建，ttrpc 通道建立。
3. `CreateContainer`：rootfs 共享、挂载、`/proc`、tmpfs、cgroup 视图、容器进程
   `execve`（serial 出现 `process command: ["/bin/busybox", ...]`、`created container!`）。
4. `online_cpu_mem`（nb_cpus=0）+ `online_memory` 返回成功。

### B.2 当前卡点

已定位到 **vsock 连接生命周期/流控**。`ctr run` 在 `StartContainer` 阶段：
此前各 RPC（create_sandbox、get_guest_details、create_container、online_cpu_mem、
update_mounts）均正常；之后 shim 的 ttrpc 客户端在 `sendReq(StartContainerRequest)`
的 `handler()` 内阻塞到 60s context deadline。

guest 侧实测两种形态：
1. **单连接**：agent 只 accept 一条连接并完成 5 个 RPC；`start_container` 的帧从未
   到达 guest（RX 事件计数停在 update_mounts）。host 线程栈落在 Go netpoller：
   写调用返回 EAGAIN 后等待可写事件（`strace` 后无写重试、`/proc/<pid>/task/*/wchan`
   中仅一个线程在 `do_epoll_wait`），符合 Linux vsock `virtio_transport_has_space`
   判定为 0 的行为。
2. **重连**：client 换新源端口重连（如 2266→2267），新连接已进入 guest accept 队列
   且 65B 的 `start_container` 帧已到达 guest RX，但 agent 只 accept 了旧连接，
   新连接未被 accept，agent 无对应 RPC 日志。

启动期（agent listen 之前，t≈0.5s）的 REQUEST 被 RST 属预期行为，与卡点无关。
另：host 侧 vsock `Send-Q` 始终为 0、`ss --vsock` 显示 ESTAB。

对照 Linux `virtio_transport_common.c` 已修复两处并验证有效：
1. **tx poll waker**：此前只注册 rx/connect waker，credit 等待后的 poll 写者永不
   被唤醒；现按 Linux `sk_write_space`/credit-update 语义同时唤醒阻塞与 poll 写者。
2. **关闭连接复用**：Linux 在完整 SHUTDOWN 时 `vsock_remove_sock()`，使对端复用
   同一源端口的新连接可以成功；现在 REQUEST 命中 Closed 连接时先移除再重建。

剩余问题假设：多次 per-RPC 新连接后，listener 的 accept 路径在第 6+ 条连接上
不再向 agent 交付（accept 队列 push 仅 1 次、无失败日志、无 reject 日志），需要
继续审计 knet listener 的 accept 队列与 poll 唤醒（或 agent accept 循环）。
另：agent 的 SIGCHLD reaper 在 guest 报 `waitpid ... ECHILD`，说明内核在子进程
退出/回收路径上还有一处语义缺口，需单独复现。

### B.3 发现的内核缺口（按处理状态）

1. **auxv 不完整（已修复，已提交）**：guest 内 rust-coreutils 因 rustix 取 auxv
   失败而 panic。按 Linux `fs/binfmt_elf.c` 补齐 `AT_HWCAP/AT_HWCAP2/AT_CLKTCK`；
   按 `kernel/sys.c prctl_get_auxv` 实现 `prctl(PR_GET_AUXV)`；新增
   `/proc/<pid>/auxv`。修复后同一用例 guest serial 中 rustix panic 归零。
2. **vsock tx 唤醒缺失（已修复，已提交）**：见 B.2。
3. **vsock 关闭连接复用（已修复，已提交）**：见 B.2。
4. **vsock 流控/accept 交付（未解决）**：
   - 单连接时 host 发送被判无 credit（EAGAIN 后长期不可写），需对齐
     `virtio_transport_has_space` 的 `peer_buf_alloc - (tx_cnt - peer_fwd_cnt)`
     账本，核对 guest 发出的每个包是否携带正确的 `buf_alloc`/`fwd_cnt`；
   - 重连时 agent 未 accept 第二条已入队连接，需对照 Linux `vsock_accept()` 与
     ttrpc server 的 accept 循环，核对 listener 唤醒/队列入队语义。
5. **SIGCHLD reaper ECHILD（未解决）**：agent 收到 SIGCHLD 后 `waitpid` 返回
   ECHILD，需复现子进程退出/回收路径。
6. **memory hotplug sysfs**：agent 报 `memory block size error: NotFound` /
   `hotplug memory error: ENOENT`，`/sys/devices/system/memory/*` 语义待对齐。
7. **userns 下 setgroups=deny**：Go `SysProcAttr.Credential` 使 fork/exec EPERM；
   PoC 在 shim 侧绕过，Linux 语义本身正常。
8. CPU online 真实 hotplug 转换返回 `EOPNOTSUPP`（设计如此）；本次 `nb_cpus=0`
   未触发。

### B.4 复现步骤（宿主就绪后）

```bash
# 1) 构建 x86_64 内核（含 9P）
cp platforms/kplat-x86_64/qemu_defconfig .config
sed -i 's/# KFEAT_FS_9P is not set/KFEAT_FS_9P=y/;
        s/# KFEAT_DRIVER_VIRTIO_9P is not set/KFEAT_DRIVER_VIRTIO_9P=y/' .config
make defconfig && make build
# 2) /etc/kata-containers/configuration.toml
#    path   = <静态 qemu wrapper（剥离 rng、写 serial 日志）>
#    kernel = <上面的 kernel.bzimg>
#    image  = xk-kata-rootfs.img；shared_fs = virtio-9p
# 3) patched shim：degradeToSymlink/noopCgroup + XK_KATA_DEGRADE_ROOTFS=1
# 4) ctr -n k8s.io run --rm --cgroup "" \
#      --runtime io.containerd.kata.v2 \
#      --rootfs /root/.tmp/busybox-rootfs kata-sec \
#      /bin/busybox sh -c "echo KATA_ON_XKERNEL_OK"
```

### B.5 2026-11-07 深挖结论与剩余缺口

本轮用 host kprobe（`virtio_transport_stream_has_space` /
`virtio_transport_send_pkt_info` / `vhost_transport_do_send_pkt`）、guest RX 探针
（virtqueue pop 时序）与带分步日志的 shim（`XKS sendReq ... begin/done`）完成了
端到端时间轴对齐：

- host credit 充足（`has_space` 稳定返回 0x20000），sendmsg 不会因 credit 阻塞；
- shim 侧 `update_mounts` 之后 ~37s 才发出 `start_container`，随后 +60s 重试一次
  （ttrpc/gRPC deadline），说明延迟在 **vhost→guest RX 投递**；
- guest 侧 RX 队列深度 8 与补投均正常（每次 pop 后立即 re-add 并 kick），但
  vhost 在一段静默期后收不到 guest 的新 buffer，报文挂 ~37s 后才被送入 guest
  used ring（随后 guest 立即弹出并投递到**已知连接**，帧本身不丢）；
- 已落地 `dc73f4f3b`（peer_free 饱和减法、poll OUT credit、RESPONSE 在入队后发送、
  重复 REQUEST 带现有 credit、accept 队列剪枝、64KiB credit 水位等 Linux 对齐修复）。

剩余缺口（按优先级）：

1. **vhost RX 静默期投递延迟（~37s，未解决）**：已确认 burst 期间
   `vhost_vsock_handle_rx_kick` 每次都即时到达并触发 `do_send_pkt`（kick 通路
   本身正常）。下一步需在**安静宿主**上抓「shim enqueue（sp）→ 送达（guest pop）」
   之间 ~37s 空窗内的 sp/do/rk 全序列，重点核对：
   - `vhost_vsock_alloc_pkt` 是否因无 guest buffer 失败（`do_send_pkt` 重试次数）；
   - 失败后 vhost 是否正确 `vhost_poll_queue` 重挂 RX work，等待下一次 guest kick；
   - 实测已排除 guest 侧：宿主 `vsock_stream_sendmsg` 对 start_container 请求
     立即返回全长（103B）；guest 侧 `OwningQueue` 的 avail/last_used 恒差 8
     （始终保有 8 个 RX buffer）且以 ~13ms 周期持续轮询 used ring；host 侧
     `vhost_transport_do_send_pkt` 在静默期被持续触发（约 15-18 次/秒）却始终
     没有向 guest used ring 发布条目。即 host→guest 数据在 vhost 内被卡住，
     与 credit、RX buffer、guest 中断/轮询均无关。
   - 已落地的 guest 侧缓解（`747890139`）：不协商 `RING_EVENT_IDX`（老 vhost 的
     event-index 抑制易丢 kick）+ RX 空轮询 16 次后补 ringing doorbell。缓解后
     create_sandbox/create_container/online_cpu_mem/update_mounts 更稳定通过，
     但「静默后投递下一包」仍会卡住。
   - 剩余可选路径（需换宿主环境验证）：升级宿主内核（≥5.x，vhost-vsock 修复
     较多）；或改用 hybrid-vsock（vhost-user-vsock + unix socket）绕开内核
     vhost 模块。kylin212 为共享 CI 机（load>30、存在 D 状态孤儿 qemu），时序
     数据在空闲窗口复测更可靠。
2. **僵尸进程 `/proc/<pid>/ns/mnt` ENOENT（已修复，`0501c3105`）**：根因是
   `do_exit()` 最后一线程退出时立即释放 nsproxy，而 Linux 在 `release_task()`
   （reap）才释放；未 reap 的僵尸因此无法再应答 `/proc/<pid>/ns/*`。修复为把
   nsproxy 释放推迟到全部 reap 路径（waitable reap / detached reap / autoreap /
   wait4 消费），并新增单测 `test_zombie_keeps_namespaces_until_reap`。
3. **本地构建 shim 在 kylin212 glibc-2.35 loader 下段错误**：以
   `CGO_ENABLED=0` 静态构建绕过（已部署 `/opt/kata/real-shim`，含 XKS 分步日志，
   日志写 `/root/.tmp/shim-step.log`）。
4. agent SIGCHLD reaper 的 ECHILD（wait4/waitid 与子进程退出顺序）。
5. memory hotplug sysfs 语义对齐 agent 期望（block_size_bytes / state / online）。
6. 上述通过后，再执行第 7 节的 P5 验收清单。
