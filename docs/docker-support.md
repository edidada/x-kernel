# X-Kernel Docker 支持规划（离线预构建 + disk.img 注入）

## 1. 文档状态

本文规划在 X-Kernel guest（aarch64 QEMU + Debian rootfs 的 `disk.img`）内运行
Docker 引擎（`dockerd` + `containerd` + `runc`）的落地路径。

核心约束与目标主张：

1. **完全离线**：Docker 二进制与容器镜像全部在构建机/CI 侧预构建并固化，通过
   `disk.img` 注入进 guest；guest 首次启动与后续运行都不需要任何网络。QEMU 即使
   以 `NET=n` 启动也必须能够完成验收。
2. **预构建优先**：不把 Docker 的安装动作留给 guest 运行时去下载，也不从源码在
   guest 内构建；只把“一次准备好的产物”注入。
3. **能力门控**：内核侧未实现的语义（network namespace、seccomp、overlayfs 等）
   必须显式降级或报错，不能伪装成功，遵循 `docs/container-architecture.md` 的
   防伪隔离原则。

本文是跨子系统规划，不是某个 crate 的现状说明。各阶段落地时需同步更新受影响
crate 的 `docs/design.md`、`docs/security.md` 与公共 API rustdoc，并遵循
`docs/ai/skills/module-docs/SKILL.md`。

## 2. 目标与非目标

### 2.1 目标

1. guest 内可通过预构建产物离线启动 `dockerd`/`containerd`，并离线运行容器
   （至少 `hello-world`、`busybox`、`alpine`）。
2. 建立“预构建 → 注入 disk.img → guest 离线启用”的可重复管线，消除运行时网络
   依赖，同时让开发者在无网环境也能完整复现。
3. 明确内核 ABI 与文件系统的最小使能清单，按依赖顺序落地，不一次性追求完整
   OCI/Docker 兼容。
4. 与 `docs/container-architecture.md` 的 cgroup/namespace Phase 计划对齐，Docker
   作为这些内核能力的第一个真实 OCI 用户态验收方。

### 2.2 非目标

1. 第一阶段不做 Docker 默认 bridge 网络（`--iptables`、veth、NAT），统一使用
   `--network=host`；network namespace 实例化完成后再评估。
2. 第一阶段不使用 overlayfs 存储驱动，使用 `vfs`；overlayfs 是否落地单独决策。
3. 不承诺 Kubernetes/systemd 兼容；当前 rootfs 是 openrc 系。
4. 不在 guest 内构建 Docker 源码，不依赖 guest 包源/网络安装依赖。
5. 不做 rootless Docker；`dockerd` 以 root 运行，user namespace 映射留待
   `container-architecture.md` 后续 Phase。

## 3. 现状盘点（事实基线）

以 `platforms/kplat-aarch64/qemu_defconfig` 为基准（`KFEAT_FS=y`、`KFEAT_FS_EXT4=y`、
`KFEAT_FS_CGROUP2=y`、`KFEAT_NET=y`、`KFEAT_TIPC=y`、`KFEAT_VSOCK=y`）。

| 能力 | 现状 | 对 Docker 的影响 |
|------|------|------------------|
| rootfs 获取 | `make rootfs` 下载预构建 `x-kernel-debian-openrc-aarch64.img` 存为 `disk.img` | 注入管线以此为基座 |
| uapp 注入 | `make uapps` 用 `debugfs -w` 向 `disk.img` 注入自定义文件并生成 `/etc/profile.d/99-autostart.sh`（见 `xtask/uapp/`） | Docker 注入可复用同一 debugfs 机制 |
| cgroup v2 | `cgroup2fs` 在 initial mount namespace 自动挂载 `/sys/fs/cgroup`，`kcgroup` 有 hierarchy/membership/pids controller | dockerd 启动的基本前提（Phase 1 已具备） |
| mount 系统调用 | `mount`/`umount2` 已实现，`kvfs` 提供 `MntNamespace` 与 bind mount | 容器 rootfs/挂载点准备所需 |
| AF_UNIX | `knet` 已支持 unix stream/dgram socket | dockerd↔containerd↔shim 的 socket IPC 可行 |
| clock_gettime 等时间 | `ktimer` 已提供 | runc/dockerd 正常运行所需 |
| `CLONE_NEWNS/NEWUTS/NEWIPC` | clone 时解析并传入 `kns::NsProxy`（`core/ksyscall/src/task/clone.rs`） | 可支撑容器基础 namespace |
| `CLONE_NEWPID` | 解析但返回 `ENOSYS`（注释明确“not yet fully supported”） | runc 创建容器必需 → 必须启用 |
| `CLONE_NEWNET/NEWUSER/NEWCGROUP/NEWTIME` | 返回 `ENOSYS` | 与 `--network=host`、root 运行、cgroup namespace 策略一致时可先不解锁 |
| `unshare`/`setns` | 未实现（dispatch 走默认 `Unimplemented syscall`） | runc 的 namespace 设置/加入需要 → 需实现 |
| `pivot_root` | 未实现 | runc 切换容器 rootfs 必需 → 需实现（无有效 fallback） |
| `sysfs` | 无（`fs/filesystems/` 只有 9p、anon_inodefs、bpffs、cgroup2fs、devfs、fat、kext4、memfs、pipefs、procfs） | containerd/runc 依赖 `/sys`（设备、cgroup 之外的 sysfs 面）→ 需最小实现 |
| `tmpfs` | 无；`memfs` 有 shmem 基础 | `/dev`、`/dev/shm`、`/run`、`/var/lib/docker` 挂载需要 → 需把 memfs 扩展为可挂载 tmpfs |
| `devpts` | 无（devfs 已有基础 /dev 节点能力） | 容器 `PTY` 功能；第一阶段可降级，明确记录 |
| `overlayfs` | 无 | Docker 默认 storage driver 不可用 → 第一阶段用 `vfs` |
| seccomp | 无 | dockerd 默认加载 default seccomp profile → 用 `seccomp=unconfined` 降级 |
| network namespace 实例 | `knet` 为全局状态，无实例化 | → `--network=host` 规避 |
| `/proc` | procfs 已存在；runc 依赖的 mountinfo/self/fd 等细节需按清单核对 | 与 `CLONE_NEWPID` 一起补 |
| 容器测试基础 | `uapps/mini-oci`（受限 run、静态 bundle）、`uapps/cgroup-test` | 可作为 runc 之前的过渡验证 |

结论：内核侧距离“runc 能跑通一个离线 busybox 容器”的最小集还有明确缺口，按依赖
顺序依次是：`CLONE_NEWPID` 启用 → `unshare`/`setns` → `pivot_root` → 最小 `sysfs`/
可挂载 `tmpfs` → `/proc` 细节核对；Docker 引擎侧则是把 `vfs` 存储、host 网络、
seccomp unconfined 三条降级路径写入 daemon.json。

## 4. 总体策略：三层离线闭环

```text
 L0 预构建资产（有网环境一次性完成，版本固定）
     ├─ Docker/containerd 各组件 aarch64 deb 包闭包（不依赖 guest 包源）
     ├─ 容器镜像 docker save 产物（hello-world / busybox / alpine，arm64）
     └─ 校验和、版本清单、来源记录（reproducible）
              │ make docker-assets / CI 产物
              v
 L1 注入 disk.img（宿主侧，debugfs，无需 root）
     └─ /opt/xk-docker/pkgs/*.deb
        /opt/xk-docker/images/*.tar
        /etc/init.d/xk-docker-setup（openrc 首启动服务）
        /etc/docker/daemon.json（离线降级配置）
              │ make dockerfs → disk.img 每次构建刷新
              v
 L2 guest 离线启用（openrc）
     └─ 首次启动：dpkg -i 本地 deb（离线）→ docker load 镜像（离线）→ 启动 containerd/dockerd
        └─ 验收：NET=n 的 QEMU 下 docker run hello-world 成功
```

关键判定：**注入完成即可离线**。guest 没有任何阶段访问网络；甚至 `make run NET=n`
也应通过验收。

## 5. L0：预构建资产固化

### 5.1 来源与版本固定

- Docker 官方 deb（arm64，Debian bookworm）：
  `docker-ce`、`docker-ce-cli`、`containerd.io`、`docker-buildx-plugin`、
  `docker-compose-plugin`；依赖闭包用干净 bookworm 容器里 `apt-get install
  --download-only` 生成，全量拷出。
- 容器镜像：`docker pull --platform linux/arm64 hello-world busybox alpine`，
  `docker save` 为 tar。
- 一律记录版本号 + sha256 + 下载 URL 到 `tools/docker-pkgs/manifest.json`。

### 5.2 存放位置与可重复性

```text
tools/docker-pkgs/
├── manifest.json        # 版本、sha256、来源 URL、生成命令
├── aarch64/
│   ├── debs/            # *.deb 闭包（gitignore 或单独制品库）
│   └── images/*.tar     # docker save 产物
└── build-deb-closure.sh # 清空容器中生成 deb 闭包的复现脚本
```

产物进入 CI 制品或制品库，不入 git（体积大）。`manifest.json` 入库保证可审计。
首次准备在**有网**环境执行一次；之后所有开发者/CI 直接复用产物，完全离线。

## 6. L1：注入机制设计

### 6.1 复用 uapp 框架

现有 `xtask/uapp` 已支持 debugfs 注入任意文件与 autostart 生成，是最小改动路径。
新增 uapp `uapps/docker-offline/`。`enabled = false`，默认不参与 `make uapps` 的
`all` 选择；显式 `--select` 可选中默认关闭的 uapp，这是对 `xtask/uapp`
`select_uapps` 的一处小改动（“默认关闭、显式启用”的 opt-in 清单模式）：

```toml
[package]
name = "docker-offline"
description = "Inject prebuilt offline Docker engine + images into disk.img"
enabled = false
order = 90

[prepare]
commands = [ "sh prepare.sh" ]

[[install]]
src = "out/pkgs"
dest = "/opt/xk-docker/pkgs"
mode = "0644"

[[install]]
src = "out/images"
dest = "/opt/xk-docker/images"
mode = "0644"

[[install]]
src = "out/etc-docker/daemon.json"
dest = "/etc/docker/daemon.json"
mode = "0644"

[[install]]
src = "out/xk-docker-setup"
dest = "/etc/init.d/xk-docker-setup"
mode = "0755"

[[autostart]]
name = "xk-docker-setup"
command = "rc-service xk-docker-setup start"
background = true
check_alive = false
```

要点：

- `prepare.sh` 运维 `tools/docker-pkgs`：缺产物时快速失败，并用
  `sha256sum -c SHA256SUMS` 先行校验再暂存，避免半注入/损坏产物；
- 注入过程不写 dpkg 数据库（那留给 guest 首次启动的 `dpkg -i` 完成），保证 dpkg
  状态一致、依赖关系完整建立；
- openrc 脚本 `/etc/init.d/xk-docker-setup` 作为普通文件注入；autostart（profile.d
  login 钩子，后台非阻塞）触发 `rc-service xk-docker-setup start`。脚本幂等
  （完成标记 `/var/lib/xk-docker/setup.done`）并全程写日志：`dpkg -i` 失败才致命，
  服务启动/镜像载入失败仅记录，不阻塞登录。

### 6.2 替代方案对比（决策记录）

| 方案 | 做法 | 优点 | 缺点 | 结论 |
|------|------|------|------|------|
| A. guest 侧 dpkg -i | 注入 deb + 首启动执行 `dpkg -i`、`docker load` | 无需宿主 root/qemu-user；dpkg 状态一致；postinst（建用户、配置）在真实环境执行；与 uapp 理念一致 | 首次启动多花 10~30s | **默认路径（推荐）** |
| B. 宿主 chroot 预安装 | loop 挂载 ext4，`qemu-aarch64-static` chroot 里真实安装后卸载 | 首启动零安装 | 宿主需 root + binfmt/qemu-user-static；CI 变脆；不走 debugfs 通道 | CI 出“golden image”时可选，非默认 |
| C. 宿主解包后 debugfs 直写 | `dpkg-deb -x` 到 staging，再 debugfs 写入 | 启动最快 | dpkg 数据库不一致、postinst 不执行，依赖/符号链接易错 | 拒绝 |

默认路径 A 同时保留 deb 可卸载性与可审计性。

### 6.3 Makefile 目标

```text
make docker-assets    # 只准备/校验 tools/docker-pkgs（有网环境或复用制品）
make docker-check     # 离线校验 SHA256SUMS
make dockerfs         # 离线注入 disk.img（等价 uapp install --select docker-offline）
```

`dockerfs` 先经 `prepare.sh` 的 `sha256sum -c` 校验再注入，防止损坏产物静默进入
guest。

## 7. L2：guest 离线启用设计

### 7.1 首启动 setup 服务（openrc）

注入 `/etc/init.d/xk-docker-setup`（随 uapp 注入的完整实现见
`uapps/docker-offline/docker/xk-docker-setup`），流程要点：

```text
start()
  ├─ mark: /var/lib/xk-docker/setup.done 存在则直接 no-op（幂等）
  ├─ dpkg -i /opt/xk-docker/pkgs/*.deb         # 离线，闭包已含依赖；失败致命
  ├─ rc-service containerd/docker start         # 尽力而为，缺失/失败仅记日志
  ├─ docker load /opt/xk-docker/images/*.tar    # 尽力而为，失败仅记日志
  └─ touch 完成标记
```

全流程写 `/var/log/xk-docker-setup.log`。触发方式：uapp 的 autostart（profile.d
login 钩子，后台非阻塞）执行 `rc-service xk-docker-setup start`。

注意 docker-ce deb 的 maintainer 脚本只会创建 systemd unit；openrc 下
`containerd`/`docker` 两个 init 脚本在 guest 联调阶段（阶段 2）随注入补齐并回归
`rc-update`，阶段 0 版本通过日志暴露缺失而不假装成功。

### 7.2 dockerd 离线降级配置（daemon.json）

```json
{
  "storage-driver": "vfs",
  "iptables": false,
  "bridge": "none",
  "log-driver": "json-file",
  "max-concurrent-downloads": 1,
  "registry-mirrors": [],
  "features": { "containerd-snapshotter": false }
}
```

配合启动参数中的运行降级：

```text
dockerd --iptables=false --bridge=none --ip-forward=false --ip-masq=false \
        --exec-opt native.cgroupdriver=cgroupfs \
        --security-opt seccomp=unconfined
```

每个降级项都必须显式写配置并记录原因（见第 9 节风险表），不允许靠默认值隐式
碰运气。`daemon.json` 也属于注入的预构建产物。

### 7.3 运行期降级矩阵

| 维度 | 第一阶段取值 | 原因 | 后续解锁条件 |
|------|--------------|------|--------------|
| 网络 | `--network=host`、bridge=none、iptables=false | `knet` 无 netns 实例、无 bridge/veth/NAT | `container-architecture.md` 网络 Phase |
| 存储 | `storage-driver=vfs` | 无 overlayfs | overlayfs 落地并验证后切换 |
| seccomp | `seccomp=unconfined` | 内核无 seccomp | seccomp(2) 实现后按 profile 收紧 |
| cgroup | cgroupfs driver + pids 即可 | `kcgroup` Phase 1 | CPU/memory controller 就绪后暴露相应文件（能力门控） |
| user namespace | 关闭 rootless、UID 映射 | 无完整 user ns | `container-architecture.md` Phase 2+ |
| manifest 存储 | 本地 dir，不用 registry API | 离线 | registry 离线镜像（registry:2 + 预置仓库）为可选增强 |

### 7.4 自定义镜像离线闭环

开发者在有网环境构建自己的测试镜像后：

```text
docker save -o tools/docker-pkgs/aarch64/images/<name>.tar <image>
make dockerfs
```

即可把新镜像固化进下一个 `disk.img`，完整闭环无需 guest 网络。

## 8. 内核能力使能清单（按依赖顺序）

> 命名与实现位置遵循 `docs/container-architecture.md`，crate 边界不应被本规划改变。
> 每一项落地前先对照该文档的“能力门控 ABI 暴露”原则：没有执行语义就不暴露。

### 8.1 `CLONE_NEWPID` 启用（必需）✅ 已落地（增量 1）

- 现状：`core/ksyscall/src/task/clone.rs` 曾对 NEWPID 返回 `ENOSYS`。
- 已交付：
  - `kns::NsProxy::clone_for_child` 在父 active PID namespace 下创建新
    `PidNamespace` 并挂为 child `pid_ns_for_children`（其余 namespace 照旧共享）；
  - `kprocess::fork_process_runtime` 按 `allocate_in(new_ns)` 分配 child leader
    `PidHandle`，普通 fork/线程克隆按父进程 active namespace 分配——handle 携带
    root-visible 唯一编号与各 ancestor namespace 可见编号；
  - `getpid`/`getppid`/`gettid` 按调用者 active namespace 渲染（`Process::pid`/
    `Thread::tid` 保持 root/global 内部键，不参与用户可见改动）。
- 尚未交付（后续增量）：`/proc` 按 namespace 过滤、`wait`/`kill`/signal 的
  namespace 视图、`setns`、procfs `/proc/self/status` 细节清单。
- 验证：`make UNITTEST=y UNITTEST_CRATE=kprocess,kns,ksyscall run` 126 过 0 败
  （含新增 7 个 NEWPID 相关测试）；`make build`/`make clippy` 通过。

### 8.2 `unshare`/`setns`（必需）✅ `unshare` 已落地（增量 2）；`setns` 待 namespace fd

- 已交付 `unshare(2)`：
  - `kns::NsProxy` 复用 `clone_for_child` 构造调用进程的新 namespace bundle；
  - `kprocess::ProcessRuntime::unshare_namespaces`：支持 `NEWNS`/`NEWUTS`/`NEWIPC`/
    `NEWPID`（new children），`NEWNS` 在共享 fs_struct 时按 Linux 返回 `EINVAL`；
  - `core/ksyscall` 新增 `sys_unshare`（`Sysno::unshare` 已在 dispatch 接线）。
- `setns`：syscall 面已就位（参数校验 + 明确 `EOPNOTSUPP`）；实际加入需
  `/proc/[pid]/ns/*` namespace fd，随 procfs 增量完成。
- 验证：`kprocess` 新增 4 个 unshare 单测（NEWPID 后续子进程、NEWUTS 复制
  hostname、NEWNS 共享 fs 拒绝、未实现类型拒绝）；相关 crate 130 过 0 败。

### 8.3 `pivot_root`（必需）✅ 已落地（增量 3）

- `kvfs::MntNamespace::pivot_root`：把命名空间可见 root 换成 `new_root` 挂载，把原
  root 挂到 `put_old`；校验（`new_root` 为已注册挂载根、`put_old` 为未挂载目录等）
  全部在首次修改前完成；栈保留语义对齐 Linux。
- syscall：`posix/fs sys_pivot_root`（含调用者 fs root/pwd 重定向，对应 Linux
  `chroot_fs_refs`）已接入 `Sysno::pivot_root`；`umount2(MNT_DETACH)` 同步开放
  （`detach_tree`，无 busy 检查），支撑 OCI 的 `umount2(old_root, MNT_DETACH)`。
- 验证：kvfs 新增 4 个 pivot 单测（root 交换 + 旧 root 迁移到 put_old、普通目录/
  已挂载目录/当前 root 拒绝路径）；四个相关 crate 313 过 0 败；`build`/`clippy` 通过。
- 未交付：`MS_MOVE`、shared propagation 组（`make_mount_shared` 等）；`umount2` 的
  `MNT_FORCE`/`MNT_EXPIRE`/`UMOUNT_NOFOLLOW` 仍显式拒绝。

### 8.4 最小 `sysfs`（必需）

- `containerd`/`runc` 读取 `/sys`（设备树、`/sys/fs/cgroup` 之外的面）。
- 工作：新增 `fs/filesystems/sysfs` 最小实现，挂载 `/sys`，先覆盖
  `/sys/fs/cgroup`（已有 cgroup2fs）、`/sys/class`、`/sys/devices` 只读枚举；
  未实现属性明确返回 `ENOTSUP` 而不是空数据。
- 验收：`ls /sys/class`、`cat /sys/fs/cgroup/cgroup.procs` 可用；无伪数据。

### 8.5 可挂载 `tmpfs`（必需）

- 工作：把 `memfs` 的 shmem 基础扩展为可挂载 tmpfs（`mount -t tmpfs`），覆盖
  `/dev`、`/dev/shm`、`/run`、`/var/lib/docker` 等临时面。
- 验收：容器内存盘挂载、写入、卸载闭环；`df -T` 可见 tmpfs。

### 8.6 `/proc` 细节清单（必需，逐一核对）

runc 对 `/proc/<pid>/...` 依赖较细，至少核对：

- `/proc/self/mountinfo`（OCI 与 dockerd 卷管理）；
- `/proc/<pid>/status`（Uid/Gid/CapMask/NSpid）、`/proc/<pid>/stat`；
- `/proc/self/fd/`（含 socket fd 指向）、`/proc/self/fdinfo`；
- `/proc/<pid>/ns/*`（`setns` 的 fd 目标，Phase 2 再要求真实 identity）；
- `/proc/sys/kernel/hostname` 与 UTS namespace 联动；
- 缺失项必须返回明确 ENOSYS/ENOTSUP，不能伪造。

### 8.7 可延后项（明确记录，不阻塞第一阶段验收）

| 项 | 说明 | 解锁后动作 |
|----|------|-----------|
| `devpts` | PTY 功能缺失时容器内 shell 交互受限 | 用 `-it /dev/console` 或独立 devpts 最小实现 |
| CPU/memory cgroup controller | dockerd 会尝试读写 `cpu.max`、`memory.max` | 按 `container-architecture.md` Phase 3/4 落地；落地前若 dockerd 因缺文件失败，评估以“只读不生效”还是“报错可跳过”处理，遵循能力门控 |
| capget/capset 细节 | `kcred` 已有 credentials 基础 | 结合 dockerd root 运行 + runc 的 ambient caps 校验补齐 |
| network namespace | dockerd 默认 bridge 不可用 | host 网络模式规避 |
| `finit_module`/kmod | Docker 不使用 | 不需要 |

## 9. 分阶段实施计划与验收标准

### 阶段 0：离线注入管线（无内核改动）

- 交付：`tools/docker-pkgs/` 目录与 `manifest.json`、`build-deb-closure.sh`；
  uapp `docker-offline`；`make docker-assets`/`make dockerfs`；openrc 服务与
  daemon.json 注入。
- 验收（纯宿主侧，不涉及运行 Docker）：
  - `debugfs` 列出 `disk.img` 中存在 deb/tar/init 脚本，sha256 与 manifest 一致；
  - 注入前后 `disk.img` 其余内容不变（diff 挂载树或文件清单）；
  - 无 `tools/docker-pkgs` 时 `make dockerfs` 快速失败。

### 阶段 1：runc 最小容器（内核 ABI 最小集）

- 交付：`CLONE_NEWPID`、`unshare`、`setns`（部分）、`pivot_root`、最小 sysfs、
  可挂载 tmpfs、`/proc` 清单核对；`mini-oci` 升级为“runc 直跑 busybox 离线
  bundle”用例。
- 验收（`NET=n` 的 QEMU）：
  - `runc run` 一个预置 busybox bundle，容器内 `mount`/`ps`/hostname 正确；
  - 容器 init 退出、runc 回收、无残留挂载/进程。

### 阶段 2：containerd + dockerd 服务化

- 交付：openrc 启动 `containerd` + `dockerd`（vfs、host 网络、seccomp unconfined、
  cgroupfs driver）；首启动离线 `dpkg -i` 与 `docker load` 闭环。
- 验收（`NET=n`）：
  - `docker version` 客户端/服务端均正常；
  - `docker run --rm hello-world` 输出成功；
  - `docker run -it busybox sh` 基础命令可用（PTY 缺失时记录降级）；
  - 重启后 setup 服务幂等（标记文件已存在，不重复安装）。

### 阶段 3：离线开发闭环与回归

- 交付：`docker save` 新镜像 → `make dockerfs` 再验证的教程；把 cgroup/namespace
  相关用例接入外部 Starry Test Harness（按 `docs/ai/skills/test-harness/SKILL.md`）。
- 验收：从“有网准备一次产物”到“完全离线跑新镜像”全链路可复现；内核 ENOSYS 日志
  无新增异常面。

### 阶段 4（可选增强，单独立项）

- bridge/veth/iptables 或 knet netns 实例化；overlayfs；CPU/memory controller；
  devpts；registry 离线镜像。每项按 `container-architecture.md` 对应 Phase 单独
  设计评审。

## 10. 验证矩阵

统一命令链（与 `docs/ai/skills/build-workflow/SKILL.md` 一致）：

```bash
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
make build
make clippy
make docker-assets
make rootfs          # 需要一次有网步骤或复用缓存镜像
make dockerfs
make run NET=n       # 关键：无网卡启动
```

guest 内验收：

```bash
rc-service docker status
docker version --format '{{.Server.Version}}'
docker run --rm hello-world
docker images --format '{{.Repository}}:{{.Tag}}'
ls /sys/fs/cgroup/cgroup.procs
```

性能基线（记录，不设硬门槛）：首次启动 setup 耗时、`docker run` 冷启动延迟、
guest 内存占用增量、`disk.img` 体积增量。

## 11. 风险与开放问题

1. **deb 依赖闭包**：docker-ce/containerd.io 的依赖可能随 Debian 版本漂移。
   对策：闭包在干净 bookworm 容器内一次性生成并固化版本，不追踪 latest。
2. **磁盘空间**：需核实 `disk.img` 当前容量与剩余空间；不足时在注入管线中
   扩展 ext4（e2fsresize）再注入。
3. **debugfs 限制**：大文件、硬链接、保留权限位写入需实测；落入 deb 时以
   `dpkg -i` 为准、注入仅充当搬运工，可规避权限语义问题。
4. **dockerd 对 cgroup 文件的最小需求未知**：是计划中最大的不确定性。阶段 2
   用 `--log-level=debug` 抓取 dockerd/containerd 实际打开的文件清单，作为
   Phase 1 完成后的第一份输入；按“先补真实消费的，不预铺全部 controller 文件”
   实施。
5. **PTY/交互路径**：`docker exec -it` 依赖 devpts；第一阶段记录降级并给出非
   TTY 验收用例。
6. **openrc vs systemd**：docker-ce deb 仅带 systemd unit，openrc init 脚本必须
   随注入提供并回归 `rc-update`。
7. **许可证**：dockerd(Moby)/containerd/runc 均为 Apache-2.0；预构建 deb 与镜像
   随制品分发需在 NOTICE 中记录来源与版本（遵重各自 LICENSE）。
8. **镜像可重复性**：`docker save` 产物含随机层 ID 与时间戳，`manifest.json` 记录
   sha256 而非长生 ID；CI 缓存策略避免同 URL 不同内容的静默漂移。

## 12. 与既有文档的关系

- `docs/container-architecture.md`：内核 namespace/cgroup 长期路线；本规划是其
  首个 OCI 用户态验收方，阶段 1 的 namespace/`/proc` 工作必须回填该文档的
  对应阶段范围。
- `docs/ai/skills/build-workflow/SKILL.md`：构建/验证命令基线。
- `docs/ai/skills/code-guidelines/SKILL.md`：内核代码与 review 约定。
- `docs/ai/skills/test-harness/SKILL.md`：guest 回归用例接入方式。
- `uapps/README.md` 与 `xtask/uapp/`：注入管线实现参照。