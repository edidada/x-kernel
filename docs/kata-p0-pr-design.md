# Kata P0 内核原语合并 PR 设计

## 1. 文档定位

本文是 `docs/kata-containers-gap-analysis.md` 第 5.1～5.4 节的落地设计。四块能力合并为
一个 PR，按独立 commit 交付，但共享同一套所有权、生命周期和错误语义约束：

- 5.1 cgroup v2：cpu、cpuset、memory、io controller；
- 5.2 namespace：`CLONE_NEWUSER`、`CLONE_NEWCGROUP`、`CLONE_NEWTIME` 及 `setns`；
- 5.3 capability：真实 capability sets、bounding/ambient、`capget/capset`、exec 重算；
- 5.4 hotplug：memory block 与 CPU online/offline 的 sysfs/uevent 契约。

设计目标是最佳结构而非最小改动：不为兼容保留 `euid == 0` 的能力代理，不暴露“可写但
不生效”的 cgroup 文件，不把 hotplug 状态塞进 sysfs。四块能力都必须有唯一事实来源，
跨子系统只通过窄接口交互。

### 1.1 关键决策

| 编号 | 决策 | 理由 |
|------|------|------|
| D1 | capability 状态属于 `kcred::Cred`，授权判断只查 `cap_effective` | 消除 50 余处 `euid == 0` 代理，避免逐点改造 |
| D2 | user namespace 拥有 uid/gid map，`Cred` 持有 `Arc<UserNamespace>` | capability 是 namespace 相对的，二者不能分离 |
| D3 | cgroup controller 状态归 `kcgroup`，执行策略归资源所有者 | 保持 `mm`、`ksched`、块层不反向依赖 cgroup |
| D4 | memory 记账由新 crate `mm/memacct` 拥有，页面带不透明 owner token | 页面生命周期在 `mm`，记账不能靠调用点自觉 |
| D5 | CPU 权重/带宽由 `task/ksched` 的调度组实体实现 | 逐任务 nice 映射不具备 cgroup 层级语义 |
| D6 | cpuset 通过 `ktask` 亲和性掩码强制，而非仅保存参数 | `cpuset.cpus` 必须有可观察效果 |
| D7 | hotplug 设备模型归 `drivers/devices/hotplug`，sysfs 只做投影 | 遵守 driver/sysfs 边界 |
| D8 | 不支持的组合返回明确 errno，禁止静默降级 | 与 `kns` 现有安全契约一致 |

## 2. 总体结构与依赖方向

```
kcred (Cred, CapSet, UserNamespace)
  ^                    ^
  |                    |
kprocess ---------> kns (NsProxy, NsFd)
  ^                    ^
  |                    |
ksyscall ---------> kcgroup (hierarchy, membership, controllers)
                       |        |         |
                       v        v         v
                  ksched    memacct    kblock
                               ^
                               |
                        memspace/pagecache

kdevice <--- hotplug ---> kcpu_id_map / kruntime / memspace
   ^
 sysfs (projection only)
```

规则：

1. 依赖只能沿箭头方向。`mm` 不依赖 `kcgroup`，`ksched` 不依赖 `kcgroup`，`sysfs`
   不依赖 hotplug 策略。
2. 跨层对象用 `Arc` 共享，但状态修改必须回到所有者提供的 API，不允许跨 crate 直接
   改字段。
3. `kcgroup` 只保存控制器的策略参数和层级关系；运行时计数、调度实体、页记账对象
   分别由 `ksched`、`memacct`、块层拥有，`kcgroup` 持有其句柄。
4. 所有 namespace 对象都必须可被 `NsFd` 表达；不能被 `setns` 打开的 namespace 不
   允许通过 `unshare` 创建。

## 3. Capability 设计

### 3.1 数据模型

`process/kcred` 新增：

```rust
pub struct CapSet(u64);                 // 位 i 对应 capability i
pub const CAP_LAST_CAP: u8 = 40;

pub struct Cred {
    // 现有 uid/gid/groups/securebits 字段保持不变
    user_ns: Arc<UserNamespace>,
    cap_inheritable: CapSet,
    cap_permitted: CapSet,
    cap_effective: CapSet,
    cap_bounding: CapSet,
    cap_ambient: CapSet,
}
```

`u64` 足够覆盖 0..=63；`CAP_LAST_CAP` 之上置位在 `capset`/`prctl` 入口拒绝，内部
不引入第二套边界。`CapSet` 提供 `contains`、`insert`、`remove`、`intersection`、
`is_subset`，全部为值语义，避免散落位运算。

`Cred::root()` 的初始值是：bounding/permitted/effective 全置位，inheritable 与
ambient 为空。`Cred::new(uid, gid)` 只在显式构造非 root 凭据时使用，五个集合全空。
`prepare()` 必须整体复制五个集合与 `user_ns`，不允许漏字段。

`is_privileged()` 保留名字但改为 `has_cap(CAP_SETUID) && has_cap(CAP_SETGID)`；
`has_sys_admin`、`has_sys_resource`、`has_sys_tty_config`、`has_cap_chown`、
`has_cap_fowner` 全部改为对应 capability 查询。这是唯一的授权收敛点，禁止在调用点
继续比较 euid。

### 3.2 user namespace 相对性

capability 只在所属 user namespace 及其后代内有效。新增：

```rust
impl UserNamespace {
    pub fn new_child(
        parent: &Arc<Self>,
        owner_euid: Uid,
        parent_could_setfcap: bool,
    ) -> Arc<Self>;
    pub fn is_ancestor_of(&self, other: &UserNamespace) -> bool;
}

impl Cred {
    pub fn has_cap_in(&self, ns: &UserNamespace, cap: Capability) -> bool;
}
```

`has_cap_in` 的规则与 Linux `ns_capable` 一致：`self.user_ns` 必须是 `ns` 或其
后代，且 `cap_effective` 包含该位。`has_cap` 等价于在 `self.user_ns` 中检查。
init user namespace 的 owner 是 root，初始凭据在其中拥有全部 capability。

`UserNamespace` 增加 `uid_map`、`gid_map`（可原子安装的 `IdMapExtent` 列表）、
`setgroups_allowed`、`owner_euid` 和 `parent_could_setfcap`。映射写入遵循 Linux
约束：只能写一次、每行三段、区间不重叠、写 uid_map 需要目标 namespace 的
`CAP_SYS_ADMIN` 与父 namespace 的 `CAP_SETUID`，并额外经过 `verify_root_map`
（映射 host root 需要 `CAP_SETFCAP`）。写 gid_map 同样需要目标 namespace 的
`CAP_SYS_ADMIN`；无父 namespace `CAP_SETGID` 时只能写单 extent 自映射，且此前
必须已写 `setgroups=deny`。映射查询提供 `map_uid_to_parent` /
`map_uid_from_parent`，供 `/proc/pid/uid_map` 与后续 idmapped mount 使用。

### 3.3 exec 重算

exec 安全元数据必须区分“文件没有 capability”与“文件带有一个 permitted 为空的
capability 属性”，后者仍会清除 ambient 并抑制 setuid-root 的权限恢复。因此使用
`Option` 保留属性是否存在：

```rust
pub struct FileSecurity {
    pub setuid: Option<Uid>,
    pub setgid: Option<Gid>,
    pub file_caps: Option<FileCapabilities>,
}

pub struct FileCapabilities {
    pub permitted: CapSet,
    pub inheritable: CapSet,
    pub effective: bool,
    pub root_id: Uid,
}

pub struct ExecSecurity {
    pub no_new_privs: bool,
    pub file: FileSecurity,
}

pub fn apply_exec(&mut self, security: &ExecSecurity) -> KResult<()>;
```

Linux `handle_privileged_root` 不用 inode owner 判断；它只检查文件是否带
capability，以及提议的 effective ID 是否“set-user-ID 成为 namespace root”
（`new.euid` 是 root 且旧 real ID 不是 root）。因此 `FileSecurity` 不需要 owner
字段，也不能把这项判断错误地挂到 `S_ISUID` 或 inode owner 上。
`kexec` 在最终可执行文件上采集元数据：`setuid` 仅在 `S_ISUID` 置位时存在，
`setgid` 仅在 `S_ISGID && S_IXGRP` 同时成立时存在；`security.capability` 缺失映射为
`None`，属性损坏返回 `EINVAL`，不能降级为“无 capability”。inode 元数据是全局
（initial user namespace）值，`FileSecurity` 中的 `setuid`、`setgid` 与
`FileCapabilities::root_id` 都使用这一表示；`apply_exec` 在需要时把它们映射进凭据
自己的 user namespace。`root_id` 由 `UserNamespace::owns_root_id` 判定是否属于该
namespace 的 root。挂载点的 `nosuid` 与 user namespace 不匹配由 VFS/exec 采集层在
构造前处理。

`Cred::apply_exec` 严格按 Linux `cap_bprm_creds_from_file` 的顺序执行，并在任何错误
返回前不提交状态：

1. 快照旧凭据，校验 ambient 不变量（ambient ⊆ permitted ∩ inheritable），违反时
   返回 `EPERM`。
2. 提议的新凭据继承旧 real/effective/saved/fs ID、inheritable 与 bounding；
   permitted 从空集开始，`has_fcap` 只由属性是否存在决定。
3. 属性存在且 `root_id` 属于当前 user namespace 时：
   `permitted = (old.bounding ∩ file.permitted) ∪ (old.inheritable ∩
   file.inheritable)`，`effective = file.effective`。若 `file.effective` 为真但
   `file.permitted` 不是该 permitted 的子集，返回 `EPERM`。
4. 应用文件 set-ID 位：把文件 setuid owner 从 initial namespace 映射到凭据
   namespace 后赋给 `new.euid`，setgid group 同样赋给 `new.egid`；映射不存在时该
   位无效（等价于 Linux 的无效 kuid）。
5. 特权 root 恢复：若未设置 `SECBIT_NOROOT`，且不是“有 file caps 且
   `new.euid == 0 && old.ruid != 0`”（`__is_suid(make_kuid(new.user_ns, 0))`），
   则当 `new.euid == 0` 或旧 real UID 为 0 时把 permitted 设为
   `old.bounding ∪ old.inheritable`；当 `new.euid == 0` 时把 `effective` 置真。
6. `is_setid = new.euid != old.ruid || new.egid != old.rgid`，在 NNP 降级之前计算。
7. 若 `no_new_privs` 且（`is_setid` 或 permitted 相对旧 permitted 增长），则
   `new.euid = new.ruid`、`new.egid = new.rgid`，并把 permitted 与旧 permitted
   求交。NNP 不是“忽略 set-ID/file caps”的开关：文件身份和文件 capability 已经
   应用，只是不能再获得超出旧 permitted 的权限。
8. `new.suid = new.fsuid = new.euid`，
   `new.sgid = new.fsgid = new.egid`。
9. 若 `has_fcap || is_setid`，清空 ambient。
10. `new.permitted |= new.ambient`。
11. `new.effective = if effective { new.permitted } else { new.ambient }`；这里不是
    “置零”，无 file caps 时保留 ambient 正是 Linux 的行为。
12. 无条件清除 `SECBIT_KEEP_CAPS`，保留 `KEEP_CAPS_LOCKED`；随后整体提交。

set-ID 语义通过“修改提议的 effective ID”实现，不为文件系统引入真实的持久特权
对象；因此 `S_ISUID` 只影响本次 exec 计算出的凭据，不会把 inode 权限带到后续操作。
X-Kernel 当前没有 LSM，`PER_CLEAR_ON_SETID` 的 LSM 部分不实现；不得用空 LSM hook
假装完成该步骤。

`SecureBits` 保持 8 个独立位：`NOROOT`、`NOROOT_LOCKED`、`NO_SETUID_FIXUP`、
`NO_SETUID_FIXUP_LOCKED`、`KEEP_CAPS`、`KEEP_CAPS_LOCKED`、
`NO_CAP_AMBIENT_RAISE`、`NO_CAP_AMBIENT_RAISE_LOCKED`。锁定位置位后，对应可写位
的修改返回 `EPERM`。`NO_SETUID_FIXUP` 只跳过 `set*uid`/`set*gid` 引发的
`cap_emulate_setxuid` 分支，不跳过 exec 重算，也不改变 ambient 的清除规则。

### 3.4 系统调用与 procfs

`core/ksyscall/src/task/ctl.rs`：

- `PR_CAPBSET_READ` 只校验 capability 编号合法（否则 `EINVAL`），返回
  `cap_bounding` 中对应位；
- `PR_CAPBSET_DROP` 要求调用者在当前 user namespace 中持有 `CAP_SETPCAP`（否则
  `EPERM`），capability 编号非法返回 `EINVAL`，随后无条件清除 bounding 位并返回
  0。permitted/effective/ambient 中仍持有该 capability 不构成失败条件；Linux 允许
  先丢 bounding 位，再由后续 `capset` 决定何时放弃已持有的权限；
- `PR_CAP_AMBIENT_IS_SET/RAISE/LOWER/CLEAR_ALL` 操作 `cap_ambient`，遵守
  `NO_CAP_AMBIENT_RAISE` 与 `ambient ⊆ permitted ∩ inheritable`；
- `PR_GET_SECUREBITS`/`PR_SET_SECUREBITS` 操作完整 `SecureBits`；
- `PR_SET_KEEPCAPS` 只改 `KEEP_CAPS`，与 securebits 视图保持一致，不再出现两个
  互相矛盾的状态源；
- `sys_capget`/`sys_capset` 实现 `_LINUX_CAPABILITY_VERSION_3`：两个 `u32` 数据字
  拼成 64 位集合。`sys_capset` 只允许修改调用线程自身，并按 Linux `cap_capset`
  在提交前一次性校验：
  - 新 inheritable ⊆ 旧 inheritable ∪ 旧 bounding；
  - 若调用者没有 `CAP_SETPCAP`，新 inheritable 还必须 ⊆ 旧 inheritable ∪ 旧
    permitted；
  - 新 permitted ⊆ 旧 permitted；
  - 新 effective ⊆ 新 permitted；
  - 新 ambient = 旧 ambient ∩ 新 permitted ∩ 新 inheritable。
  任一条件失败返回 `EPERM`，不产生部分修改。上述不变量由
  `Cred::set_capability_sets` 统一执行，syscall 层不得只做一半检查。

`sys_capset` 修改的是当前线程凭据；`capget` 读取 `Cred`，不得再返回 `u32::MAX`。
`/proc/<pid>/status` 增加 `CapInh`、`CapPrm`、`CapEff`、`CapBnd`、`CapAmb`（16 位
十六进制）与 `NoNewPrivs`，数据来自目标进程当前 `Cred`。Linux 的 status 没有
`SecureBits` 字段，不得为了“可读”而新增一个没有内核消费者的伪接口；securebits
只通过 `PR_GET_SECUREBITS` 暴露。

新增 `process/kcred/docs/capabilities.md`，列出声明的 capability 集合、每个
`has_cap_*` 的消费者、exec 状态机和锁定语义；同步 `docs/security.md` 与
`docs/design.md`。

## 4. Namespace 设计

### 4.1 统一创建契约

`NsProxy::clone_for_child` 与 `prepare_unshare` 不能再各自维护一份“支持列表”。
引入单一描述：

```rust
pub struct NamespacePlan {
    pub user: Option<Arc<UserNamespace>>,
    pub cgroup: Option<Arc<CgroupNamespace>>,
    pub time: Option<Arc<TimeNamespace>>,
    pub mnt: Option<Arc<MntNamespace>>,
    pub uts: Option<Arc<UtsNamespace>>,
    pub ipc: Option<Arc<IpcNamespace>>,
    pub pid: Option<Arc<PidNamespace>>,
    pub net: Option<Arc<NetNamespace>>,
}
```

flag 解码、`EINVAL`/`EPERM` 判定只在 `core/ksyscall/src/task/{namespace,clone,clone3}.rs`
做一次；`kns` 只负责按 `NamespacePlan` 物化对象并提交。`clone_for_child` 与
`prepare_unshare` 共用 `NamespacePlan::build(flags, ctx)`，从结构上消除 clone 与
unshare 的支持集合漂移。

`UnshareContext` 的 `privileged: bool` 改为 `cred: Arc<Cred>`。每个 namespace 的
授权使用 `cred.has_cap_in(parent_user_ns, CAP_SYS_ADMIN)`；`CLONE_NEWUSER` 本身不需要
`CAP_SYS_ADMIN`，但禁止在非 descendant user namespace 中嵌套创建。`setns` 同样按
目标 namespace 的属主检查 `CAP_SYS_ADMIN`。

### 4.2 `CLONE_NEWUSER`

`kns` 调用 `UserNamespace::new_child(current_user_ns, current_euid,
parent_could_setfcap)`。新 namespace 初始映射为空，调用者立即获得该 namespace 内的
全部 capability（Linux 语义），但 `Cred.user_ns` 的切换必须在 `prepare_unshare` 的
commit 阶段与 capability 重算一起完成，不能先切 namespace 再补 capability。

`clone(CLONE_NEWUSER)` 不暂停子任务。子进程与 Linux 一样立即可运行；空映射只表示
ID 在父子 namespace 之间暂时无法翻译，不表示子进程未就绪，也不构成阻塞用户态执行
的理由。父进程或子进程随后通过 `/proc/<pid>/{uid_map,gid_map}` 安装映射，安装成功
不触发任何任务唤醒，因为没有任务在等待映射。

`/proc/<pid>/{uid_map,gid_map,setgroups}` 由 `procfs` 从目标任务
`real_cred().user_ns()` 读取，写路径使用打开文件时的 `file->f_cred`（而不是当前
任务凭据）调用 `kcred` 的映射 API，与 Linux `file_ns_capable()` 一致。映射写入前先
校验目标有父 namespace、writer 位于目标 namespace 或其父 namespace；`uid_map` /
`gid_map` 各自只能成功写入一次。空输入、越界、重叠、超过最大 extent 数均返回
`EINVAL` 且保持可重试；权限、父 namespace 覆盖不足或重复写入返回 `EPERM`。
`setgroups` 与映射写入解耦：它不需要 capability，只按 Linux 状态机接受
`allow`/`deny`，`gid_map` 写入后不能再改。

### 4.3 `CLONE_NEWCGROUP`

`CgroupNamespace` 的创建语义是“把调用者当前 cgroup 作为视图根”，不是新建层级根。
新增：

```rust
impl CgroupNamespace {
    pub fn clone_for(cgroup: Arc<Cgroup>) -> Arc<Self>;
    pub fn initial() -> Arc<Self>;
    pub fn root(&self) -> &Arc<Cgroup>;
}
```

`kcgroup` 提供 `current_cgroup()`（只读、可在任意上下文调用），`kns` 在
`prepare_unshare`/`clone_for_child` 中调用它并构造 `clone_for`。`NsFd` 增加
`Cgroup(Arc<CgroupNamespace>)`，`with_namespace` 增加 cgroup 分支，`setns` 允许
切换到目标 cgroup namespace；切换只改变视图根，不迁移任务成员关系。

`cgroup2fs` 继续只消费 `current_user_process().cgroup_ns()?.root()`，不感知创建
过程。`CgroupNamespace` 的 Drop 必须释放对视图根 `Cgroup` 的引用；若该 cgroup 已
被删除，视图根仍保持 tombstone 语义，直到最后一个 namespace 释放。

### 4.4 `CLONE_NEWTIME`

`TimeNamespace` 从占位 ID 升级为：

```rust
pub struct TimeNamespace {
    id: NamespaceId,
    monotonic_offset: TimeSpec,
    boottime_offset: TimeSpec,
}
```

`unshare(CLONE_NEWTIME)` 与 `clone(CLONE_NEWTIME)` 创建零偏移的新 namespace；
`clock_gettime(CLOCK_MONOTONIC/CLOCK_BOOTTIME)` 在返回前加上当前任务的 offset。
`CLOCK_REALTIME` 不受 time namespace 影响。`NsFd` 增加 `Time` 变体，
`/proc/<pid>/ns/time` 与 `time_for_children` 分别渲染活动与子进程槽位，`setns`
允许切换。offset 的写入通过 `/proc/<pid>/timens_offsets`，格式为
`<clockid> <sec> <nsec>`，只允许 `MONOTONIC` 与 `BOOTTIME`，且必须在 namespace
无其他成员时写入。

### 4.5 `clone3(CLONE_INTO_CGROUP)`

`clone3` 当前把 `cgroup` 字段硬编码为 0。改为：校验 fd 是 cgroup2fs 目录、
目标 cgroup 在当前 cgroup namespace 视图内、调用者对其有 `MAY_WRITE`；子任务在
`TaskCharge::reserve_child` 时直接以目标 cgroup 为父 lineage。失败必须在子任务
发布前回滚，不能留下已计费但未发布的成员。

### 4.6 文档与测试

`kns/src/types.rs`、`nsproxy.rs`、`error.rs`、`clone.rs` 中“NEWNET/NEWPID 不支持”
的陈述全部删除或改写。新增覆盖：三种 namespace 的 `unshare`/`clone`/`setns` 成功
路径、`/proc/pid/ns/*` 可读、`uid_map` 单次写入、`CLONE_NEWCGROUP` 的视图根、
time offset 对时钟的影响、`clone3` 非法 cgroup fd 回滚。

## 5. Cgroup v2 设计

### 5.1 从单文件 pids 特化到控制器框架

当前 `process/kcgroup/src/lib.rs` 把 pids 的状态、计费、subtree 激活和迁移写死在
`Cgroup` 内。新增 cpu/cpuset/memory/io 后必须先把“控制器生命周期”抽出来，否则
`TaskCharge` 的 `Vec<Arc<PidsController>>` 与 `controller_lineage()` 会把 pids 假设
扩散到四处计费路径。

拆分为：

```
process/kcgroup/src/
  lib.rs            导出与 Cgroup/CgroupHierarchy
  hierarchy.rs      树、生命周期、事务
  membership.rs     TaskCharge/TaskMembership/迁移
  namespace.rs      CgroupNamespace
  controller/
    mod.rs          ControllerKind、ControllerSlot、ChargeSet
    pids.rs
    cpu.rs
    cpuset.rs
    memory.rs
    io.rs
```

核心抽象：

```rust
pub enum ControllerKind { Pids, Cpu, Cpuset, Memory, Io }

pub struct ControllerSlot<T> {
    state: RwLock<ControllerState<T>>,   // Inactive | Active(T)
    subtree_enabled: AtomicBool,
}

pub trait Controller: Send + Sync {
    fn kind(&self) -> ControllerKind;
    fn fork_charge(&self, child: &Cgroup) -> ChargeGuard;
    fn migrate_in(&self, tasks: &[TaskId]) -> KResult<()>;
    fn migrate_out(&self, tasks: &[TaskId]);
    fn deactivate(&self, subtree: &Cgroup);
}
```

`Cgroup` 用 `ControllerSet` 持有五个 `ControllerSlot`，不再为每个控制器写一份
`RwLock<Option<Arc<...>>>`。`controller_lineage()` 改为返回
`Vec<Arc<dyn Controller>>` 或按 `ControllerKind` 索引的固定数组；`TaskCharge`
持有 `ChargeSet`，`commit`/`Drop` 对每个 controller 调用 `fork_charge`/rollback。
pids 保留现有 CAS 计费，但通过 trait 接入，语义不变。

`set_subtree_enabled` 的校验只写一次，由 `ControllerSlot` 复用：必须先通过
`has_available_controller`，非根节点有成员时返回 `EBUSY`，子节点仍委派时父节点
不能关闭。控制器激活时从 `subtree_task_count()` 建立计数锚点；未激活时只保存
参数，不暴露文件。

`cgroup2fs` 的 `CONTROLLERS` 表扩展为五项，每个 `ControllerAdapter` 声明
`name`、`is_available`、`is_active`、`set_subtree_enabled` 和文件清单。`state.rs`
不再硬编码 `pids.current`/`pids.max`，而是遍历已激活控制器的文件声明；文件可见性
与 `cgroup.controllers`/`cgroup.subtree_control` 由同一份声明推导，避免出现
“可委派但无文件”或“文件常驻但控制器不可用”。

### 5.2 CPU controller

`cpu.weight` 不能映射为逐任务 nice：同一 cgroup 内的任务数变化会改变整组份额，且
嵌套 cgroup 的权重不会逐层相乘。正确实现是 CFS 的组调度实体，落在 `task/ksched`：

```rust
pub struct SchedGroup {
    parent: Option<Arc<SchedGroup>>,
    weight: AtomicU64,             // 1..=10000，默认 100
    // 每个 CPU 一个组实体，位于该 CPU 的 FairClass 组树中
    entities: PerCpu<GroupEntity>,
}

struct GroupEntity {
    vruntime: i64,
    weight: i64,
    parent: Option<*const GroupEntity>, // 由组树生命周期保证
    children: RbTree<GroupEntity>,
    tasks: RbTree<SchedTask>,
    is_throttled: bool,
}
```

`FairClass` 从“单一任务红黑树”改为“组实体树 + 叶子任务树”。选择顺序：从根组实体
开始，取 `min_vruntime` 的子实体；子实体是组则继续下降，是任务则返回。组实体的
`vruntime` 按自身权重推进，任务在所属组内按任务权重推进；这样每一层都按
`cpu.weight` 分配份额，嵌套语义与 Linux 一致。

`kcgroup` 的 `CpuController` 持有 `Arc<SchedGroup>`；`set_subtree_enabled` 创建子组，
迁移时 `SchedGroup::attach(task)` / `detach(task)` 在同一 runqueue 锁内完成，禁止
先改 cgroup 成员再改调度组。`cpu.weight` 写入直接更新组权重，下一次 enqueue 生效；
读取返回当前值。

`cpu.max` 采用每 cgroup 的带宽预算：

- `quota`/`period` 解析为纳秒，`quota = max` 表示不限；
- 每个 `SchedGroup` 在每个 CPU 上维护 `runtime_remaining` 与 `period_deadline`；
- 调度 tick 与 context switch 调用 `SchedGroup::charge_runtime(delta)`，沿祖先链
  扣减；任一祖先耗尽则整棵子树进入 throttled；
- 周期边界由 `ksched` 的定时器回调重置预算并唤醒 throttled 组；
- `cpu.stat` 输出 `usage_usec`、`user_usec`、`system_usec`、`nr_periods`、
  `nr_throttled`、`throttled_usec`。

`cpu.max` 的写值必须是 `"<quota|max> <period>"`；quota 小于 1ms、period 超出
`[1ms, 1s]` 返回 `EINVAL`。`cpu.weight.nice` 作为派生只读视图，避免两套事实来源。

`cpu.stat` 的 `usage_usec` 从任务累计运行时间沿组树汇总，不做定期采样，保证单调。
带宽定时器与组树共享 `ksched` 的时钟源，不引入第二套时间基准。

### 5.3 Cpuset controller

cpuset 的语义是“任务可运行的 CPU/内存节点集合”，与 `sched_setaffinity` 是两层
约束，不能互相覆盖。`ktask` 增加两个掩码：

```rust
pub struct TaskInner {
    requested_cpus: KCpuMask,   // sched_setaffinity 写入
    effective_cpus: KCpuMask,   // requested ∩ 所有祖先 cpuset.effective
    mems: NodeMask,
}
```

`set_task_affinity` 只改 `requested_cpus`，随后与 cpuset 有效集求交写入
`effective_cpus`；runqueue 放置与 idle-pull 继续只读 `effective_cpus`。cpuset 写入
或任务迁移时，重新计算交集并调用 `ktask::set_effective_cpus`；若交集为空，写入
返回 `ENOSPC`（与 Linux 一致），任务保持原掩码，不能把任务饿死。

`CpusetController` 保存：

```rust
struct CpusetState {
    cpus: KCpuMask,          // 本层 cpuset.cpus，空表示继承父
    mems: NodeMask,          // 本层 cpuset.mems
    effective_cpus: KCpuMask,
    effective_mems: NodeMask,
}
```

`effective = parent.effective ∩ (own == empty ? parent.effective : own)`。根 cgroup
的 effective 是 `kcpu_id_map` 的 present 掩码与 online 节点掩码。写入校验：own 必须
是父 effective 的子集、不能为空、不能包含 offline CPU；`cpuset.cpus.effective` 与
`cpuset.mems.effective` 只读展示计算结果。

`cpuset.mems` 的强制执行依赖 NUMA 分配策略。若系统只有一个在线节点，任何非空
`mems` 都等价于全约束，语义自然成立；多节点时，`mm` 的分配路径读取任务的
`mems`，在节点内选择页，缺页与页缓存分配使用同一策略。没有 NUMA 支持时不得
宣称多节点隔离，`cpuset.mems` 只接受 `0`。

`cpuset.cpus.partition` 与 `cpuset.cpus.exclusive` 不实现，不出现在文件表中；
Kata 不使用它们，暴露空壳会违反 D8。`cpuset.effective_cpus` 的旧别名不再提供。

### 5.4 Memory controller

memory 是四块里唯一需要页面级所有权的部分。逐调用点记账不可维护，新增
`mm/memacct` crate，作为 cgroup memory 的唯一事实来源：

```rust
pub struct MemcgId(u32);                 // 0 表示未归属

pub struct Memcg {
    id: MemcgId,
    parent: Option<Arc<Memcg>>,
    usage: AtomicU64,                    // 含子 cgroup
    peak: AtomicU64,
    max: AtomicU64,                      // u64::MAX 表示不限
    high: AtomicU64,
    low: AtomicU64,
    swap_current: AtomicU64,
    swap_max: AtomicU64,
    events: MemcgEvents,
    stats: MemcgStats,
}

pub fn create_memcg(parent: Option<&Arc<Memcg>>) -> Arc<Memcg>;
pub fn charge_frame(frame: PhysAddr, bytes: u64, memcg: &Arc<Memcg>) -> KResult<()>;
pub fn uncharge_frame(frame: PhysAddr, bytes: u64);
pub fn current_memcg() -> Option<Arc<Memcg>>;
pub fn set_current_memcg(id: MemcgId);
```

`PageMeta` 增加 `memcg: AtomicU32`，默认 0。`mm/memspace` 在匿名页和文件页第一次
映射时调用 `charge_frame`，在 `dealloc_frame`/页缓存淘汰时调用
`uncharge_frame`。charge 沿 `parent` 链累加 `usage`，任一祖先超过 `max` 则回滚
整条链并触发内存回收；uncharge 反向扣减。共享文件页只归属首个 charge 的 cgroup，
直到页被回收，符合 Linux 的 page-cache 归属语义。

当前 cgroup 通过不透明 token 传递：`TaskInner` 增加 `memcg: AtomicU32`，
`kcgroup` 在迁移时写入，`ksched` 在 context switch 时调用
`memacct::set_current_memcg`。`mm` 只依赖 `memacct`，不依赖 `ktask`/`kcgroup`，
依赖方向保持 D4。

`memory.max` 超限时按以下顺序处理：先调用已注册的 shrinker 回收文件页；仍超限则
调用 `memacct` 的 OOM 回调，由 `kprocess` 选择并杀死 cgroup 内任务；没有可杀任务
时 charge 返回 `ENOMEM`。`memory.high` 只触发直接回收与统计，不杀进程；`memory.low`
作为回收保护提示传给 shrinker。

文件表与语义：

| 文件 | 语义 |
|------|------|
| `memory.current` | 层级 usage，只读 |
| `memory.peak` | 历史峰值，写入 0 清零 |
| `memory.max` | 硬限，`max` 表示不限；写入即生效 |
| `memory.high` | 软限，触发回收与 `high` 事件 |
| `memory.low` | 回收保护 |
| `memory.swap.current/max` | swap 记账；无 swap 设备时 current 恒 0，max 只接受 `max`/0 |
| `memory.events` | `low high max oom oom_kill` |
| `memory.stat` | `anon file kernel slab shmem pgfault pgmajfault` 等已实现项 |

`memory.stat` 不输出没有真实来源的键（例如没有 LRU 就不输出 `active_anon`）。
`memory.oom.group` 与 `memory.pressure` 本 PR 不实现，也不出现在文件表中。

### 5.5 IO controller

块层目前没有节流点，`io.max` 不能只保存参数。新增 `io/kblkio` crate，并在
`drivers/contracts/block` 暴露一个窄的 QoS 注册口，避免块层反向依赖 cgroup：

```rust
// drivers/contracts/block
pub trait BioQos: Send + Sync {
    fn admit(&self, dev: DeviceNumber, op: BioOperation, bytes: u64) -> KResult<()>;
    fn complete(&self, dev: DeviceNumber, op: BioOperation, bytes: u64);
    fn dispatch_class(&self, dev: DeviceNumber) -> u32;
}

pub fn install_bio_qos(qos: &'static dyn BioQos) -> DriverResult<()>;
```

`BlockDevice::submit_bio` 在 `queue.submit` 前调用 `admit`，在 `end_io` 后调用
`complete`；`admit` 失败时以 `EIO` 结束 bio，不能静默放行。`dispatch_class` 供
块队列在多档权重之间选择下一个请求。

`io.max` 使用每 cgroup、每设备的 token bucket：

```rust
struct IoLimit {
    rbps: Option<u64>, wbps: Option<u64>,
    riops: Option<u64>, wiops: Option<u64>,
}
```

按 1ms 周期补充令牌，读写与字节/IOPS 四个维度分别检查；任一维度不足则计算等待
时间并睡眠，睡眠前释放 bucket 锁。`max` 表示该维度不限。设备号解析接受
`MAJ:MIN`，设备不存在返回 `ENODEV`，格式错误返回 `EINVAL`。

`io.weight` 使用每设备的加权公平调度：每个 cgroup 维护 `weight`（1..=10000，默认
100）和 `deficit`，块层每次选择 `deficit` 最小的 class 派发；派发后
`deficit += weight`，空闲 class 不累积。该算法在设备队列深度大于 1 时仍按权重
分配带宽，不做“先到先服务”。`io.stat` 输出已实现的 `rbytes wbytes rios wios`
与 `dbytes dios`。

`io.max` 与 `io.weight` 的生效对象是 bio 提交任务所属 cgroup；块层通过
`TaskInner.cgroup_token` 解析，不读取进程上下文。后台回写、异步 IO 在提交时捕获
token，避免完成路径归属错误。

Kata 只需要 `io.max` 时也必须在 `cgroup.controllers` 中如实声明 `io`；若底层设备
没有队列调度能力，`io` controller 标记为不可用，而不是暴露无效文件。

## 6. CPU / 内存热插拔设计

### 6.1 边界与分层

sysfs 只做投影，不拥有热插拔状态。新增两个层次：

- `drivers/contracts/kdevice` 增加 `SystemDevice` 模型：`SystemClass::{Cpu, Memory}`、
  稳定的 `SystemDeviceId`、`online` 状态与生命周期事件。系统设备没有 `/dev` 节点，
  因此不复用 `DeviceNodeKind` 的 major/minor 语义，而是独立注册表。
- 新增 `drivers/devices/hotplug`：`HotplugController`、CPU/内存状态机、事件源适配。

```rust
pub trait HotplugController: Send + Sync {
    fn class(&self) -> SystemClass;
    fn present(&self) -> Vec<SystemDeviceId>;
    fn set_online(&self, id: SystemDeviceId, online: bool) -> KResult<()>;
    fn state(&self, id: SystemDeviceId) -> Option<SystemDeviceState>;
}

pub trait HotplugEventSource: Send + Sync {
    fn start(&self, sink: &'static dyn HotplugSink) -> KResult<()>;
}
```

事件源与状态机解耦：ACPI GPE、virtio-mem、测试注入都实现
`HotplugEventSource`，状态机只处理 `Added/Removed/OnlineRequested`。没有事件源时
系统仍可启动，只是不产生新设备；绝不轮询伪造事件。

### 6.2 CPU 热插拔

`arch/kcpu_id_map` 增加 `online_mask`，与 `present_mask` 分离；`for_each_online_cpu`
成为调度器放置的依据。`ksched` 增加：

```rust
pub fn cpu_down(cpu: CpuId) -> KResult<()>;
pub fn cpu_up(cpu: CpuId) -> KResult<()>;
```

`cpu_down` 的顺序：

1. 拒绝 boot CPU 与最后一个 online CPU，返回 `EPERM`/`EBUSY`；
2. 停止该 CPU 的 tick，标记 `cpu_offlining`，不再接受新任务；
3. 通过 `ksched` 的迁移路径把该 CPU runqueue 上的任务迁到其他 online CPU；
4. 迁移该 CPU 上绑定的中断（`kirq` affinity），无法迁移的中断使操作失败；
5. 停用 runqueue、清空 per-CPU 缓存，进入架构 idle 循环；
6. 清 `online_mask`，发布 `SystemDeviceEvent::Offline`。

`cpu_up` 反向执行：设置 `online_mask`、初始化 runqueue、启动 tick、恢复中断目标，
发布 `Online`。CPU 的 present 集合来自 MADT/DT，启动时全部 online；offline 的 CPU
仍在 present 集合中，`/sys/devices/system/cpu/cpuN/online` 可再次写 1。

`cpuN/online` 对 CPU0 只读且恒为 1；不存在的 CPU 返回 `ENODEV`；写入非 0/1 返回
`EINVAL`。`cpuN/present` 与 `cpuN/possible` 只读。

### 6.3 内存热插拔

内存以“块”为粒度，块大小由架构决定（x86-64 为 128 MiB），与 Linux
`memory_block_size_bytes` 一致。`kalloc` 已有 `global_add_memory`，补充
`global_remove_memory(start, size) -> AllocResult`，要求目标范围的所有页都已释放；
任一页仍被占用则返回 `EBUSY`，由上层把块标记为 `online` 不变。

`MemoryHotplugController` 维护：

```rust
struct MemoryBlock {
    id: SystemDeviceId,
    start: PhysAddr,
    size: u64,
    state: MemoryState,   // Offline | Onlining | Online | Offlining
    present: bool,
}
```

online：校验范围在 direct map 内且不与现有块重叠，建立线性映射，调用
`global_add_memory`，成功后置 `Online`；失败回滚映射与分配器。offline：先把块内
页迁移或回收；当前没有页迁移器，因此只接受全空闲块，否则 `EBUSY` 并保持
`Online`。这是真实语义，不是静默成功。

sysfs 文件：

```
/sys/devices/system/memory/memoryN/{phys_index,state,online,block_size_bytes}
/sys/devices/system/memory/online
/sys/devices/system/memory/possible
```

`state` 输出 `online`/`offline`/`going-offline`；`online` 写入触发上述状态机；
`phys_index` 是块序号，`block_size_bytes` 为十六进制块大小。`memoryN` 目录只在
present 时存在，removed 后目录与属性一起失效，由 `kdevice` 生命周期事件驱动
sysfs  tombstone，而不是预先创建空目录。

### 6.4 事件源

本 PR 实现两个事件源骨架：

- `acpi-hotplug`：消费 ACPI GPE，读取 `_EJ0`/`_STA`。现有 `drivers/platform/acpi`
  没有 AML 解释器，本 PR 只实现 QEMU 的固定热插拔寄存器路径，并在 `docs/design.md`
  中明确不支持任意 AML；无法识别的 GPE 记录日志并忽略。
- `virtio-mem`：实现 virtio-mem 设备的 plug/unplug 请求，块粒度与 6.3 对齐。

若平台没有上述事件源，`/sys/devices/system/{cpu,memory}` 仍展示启动时的真实
拓扑，`online` 可读写已存在设备；不会出现“写入成功但没有硬件”的假热插拔。

### 6.5 验收

- offline 一个非 boot CPU 后，该 CPU 不再被调度，任务全部迁移，中断不丢失；
  online 后恢复，`cpuN/online` 与 `online_mask` 一致。
- 内存块 online 后 `global_add_memory` 可见且可分配；offline 全空闲块成功，
  有占用页时返回 `EBUSY` 且状态不变。
- sysfs 目录随 present 集合增删，属性权限为 0644（只读项 0444），写入校验
  在状态机内完成，不在 sysfs 层复制。
- 无事件源启动、事件源重复注册、CPU0 offline、最后一块内存 offline 均有测试。
## 7. 并发、生命周期与安全不变量

### 7.1 锁顺序

全仓库遵守以下偏序；箭头左侧先持有，右侧后持有，禁止反向：

| 路径 | 顺序 |
|------|------|
| cgroup 成员迁移 | 进程 `cgroup_target` gate → `CgroupHierarchy::transaction` → `Cgroup` 节点 `RwLock` → controller 原子量 |
| cgroup 委派 | `transaction` → `Cgroup` 节点锁 → `subtree_enabled` 原子量 |
| 调度迁移 | task 锁 → runqueue 锁 → `SchedGroup` 组实体锁 |
| namespace | 进程 `nsproxy` gate → 目标 namespace 锁；`setns` 不持有源 namespace 锁 |
| capability | task 锁 → `Cred`（`Cred` 内部无锁，整体替换） |
| 热插拔 | `SystemDeviceRegistry` 锁 → 设备状态锁；事件回调在两者之外执行 |
| 内存记账 | `Memcg` 原子量 → shrinker 锁；回收/OOM 回调不持有 `Memcg` 锁 |

`CgroupHierarchy::transaction` 是唯一可睡眠的 cgroup 全局锁；持有它时不得获取
runqueue 锁、不得等待 IO、不得调用 reclaim。调度器侧需要组合 cgroup 与 runqueue
时，先在 `transaction` 下完成所有可能失败的校验与资源预留，释放后再进入 runqueue
做不可失败的提交。

### 7.2 两阶段迁移

`Cgroup::migrate_group` 与 `kprocess::migrate_process_cgroup_with` 统一为：

1. **校验**：目标 cgroup 存在、未删除、在调用者 cgroup namespace 视图内、
   调用者对其有 `MAY_WRITE`、目标 controller 有可用容量；全部失败在改动前返回。
2. **预留**：为每个 controller 预留 charge、为目标 `SchedGroup` 预留组实体；
   预留失败回滚已预留项。
3. **提交**：在 `transaction` 下切换 `TaskMembership`，更新 `TaskInner.cgroup_token`、
   `memcg`，然后逐任务在 runqueue 锁内 attach 调度组。提交阶段每一步都必须不可失败。
4. **发布**：释放 `transaction` 后发布 cgroup 事件、唤醒等待者。

失败返回 `EXDEV`（跨 cgroup namespace 视图）、`EBUSY`（委派冲突）、`ENOSPC`
（cpuset 交集为空）、`EAGAIN`（pids 限额）。禁止“部分任务迁移成功、部分失败”。

### 7.3 生命周期

- cgroup：`remove_child` 只标记 `REMOVING`，最后一个任务迁出且最后一个 namespace
  视图释放后进入 `REMOVED`；tombstone 保留到挂载点卸载，防止 inode 复用后指向
  新 cgroup。
- memcg：`Memcg` 由 `Arc` 持有；cgroup 删除后仍存活到最后一个被记账页释放，
  避免 uncharge 访问已释放对象。
- user namespace：`UserNamespace` 由 `Cred`、`NsFd`、子 namespace 共同持有；
  `uid_map`/`gid_map` 成功写入一次后不可变；`setgroups` 独立受控，`deny` 在
  `gid_map` 写入前可重复执行，`allow` 不能撤销 `deny`，`gid_map` 写入后
  `setgroups` 不再可写。
- time namespace：offset 只在没有其他成员时可写；`time_for_children` 与新子进程
  共享同一 `Arc`，不复制 offset。
- 热插拔：设备 `Removing` 后不再接受新引用；sysfs 节点持有弱引用，最后一个引用
  释放后目录失效。CPU offline 与 memory offline 都先发布 `GoingOffline`，成功后再
  发布 `Offline`，失败时发布 `Online` 恢复事件。

### 7.4 安全边界

- 权限判断唯一入口是 `Cred::has_cap_in(ns, cap)`，不允许再出现 `euid == 0` 的
  特权判断。用户 namespace 的 root 只在本 namespace 及其子 namespace 内拥有
  capability，不能获得初始 namespace 的权限。
- `setns` 校验目标 namespace 的 owner user namespace 与调用者是否有
  `CAP_SYS_ADMIN`；`CLONE_NEWUSER` 不要求 `CAP_SYS_ADMIN`，但禁止在非祖先
  user namespace 中嵌套创建。
- `PR_CAPBSET_DROP` 只要求 `CAP_SETPCAP` 与合法 capability 编号；目标仍在
  permitted/ambient 中并不阻止清除 bounding 位。缺 capability 返回 `EPERM`，
  编号非法返回 `EINVAL`，不得静默忽略。
- `capset` 只能收紧 permitted/inheritable，effective 必须是新 permitted 的子集；
  任何非法组合返回 `EPERM`，且不改动原 `Cred`。
- `uid_map`/`gid_map` 写入使用 `file->f_cred`；目标是初始 namespace 外的新
  namespace 时，还要求 writer 在目标 namespace 有 `CAP_SYS_ADMIN`。普通写入
  需要父 namespace 的 `CAP_SETUID`/`CAP_SETGID`；无该 capability 时只允许
  单 extent 的属主自映射（gid_map 还要求已 `setgroups=deny`）。uid_map 映射
  host root 时额外需要 `CAP_SETFCAP`。映射行必须三段、无重叠、单次写入。
- sysfs 热插拔写入要求 `CAP_SYS_ADMIN`；只读属性不因可写而放宽权限。
- `/proc/<pid>/status` 的 capability 字段按目标进程的 `Cred` 输出，不泄露内核
  指针或未映射的 host uid。

### 7.5 错误语义

| 场景 | 错误 |
|------|------|
| 未知 namespace flag / 非法 cpu.max 格式 / 非法 cpuset 掩码 | `EINVAL` |
| 缺少 capability、CPU0 offline、最后 online CPU offline | `EPERM` |
| cgroup 委派冲突、内存块仍有占用页、目标 cgroup 有任务 | `EBUSY` |
| 设备/CPU/内存块不存在 | `ENODEV` |
| cpuset 交集为空 | `ENOSPC` |
| pids 限额耗尽 | `EAGAIN` |
| 功能在该平台确实不存在 | `EOPNOTSUPP`，且不出现在文件表 |

任何路径不得用 `Ok(())` 掩盖未实现语义；`EOPNOTSUPP` 只用于平台能力缺失，
不用于本 PR 范围内的功能。

## 8. 实施顺序与验收

### 8.1 提交划分

1. **设计文档与测试骨架**：本文件、crate `docs/design.md` 更新、失败用例先行。
2. **capability 内核语义**：`kcred` 五集合与 user namespace、`ksyscall` 的
   `capget/capset/prctl`、exec 六步重算、`procfs` 状态字段。该部分不依赖 cgroup，
   可独立验收。
3. **namespace 创建**：`NamespacePlan`、`CLONE_NEWUSER/NEWCGROUP/NEWTIME`、
   `setns`、`clone3(CLONE_INTO_CGROUP)`；删除所有重复的 flag 拒绝层。
4. **cgroup 框架拆分**：把 `kcgroup/src/lib.rs` 拆为 hierarchy/membership/
   namespace/controller 模块，先把 pids 迁入 `ControllerSet`，行为不变。
5. **cgroup 控制器**：cpuset → cpu → io → memory。每个控制器同时提交
   `cgroup2fs` 文件声明、`cgroup.controllers`/`subtree_control` 和测试；memory 最后，
   因为它依赖 `mm/memacct`。
6. **热插拔**：`kdevice::SystemDevice` → `drivers/devices/hotplug` → CPU 状态机 →
   memory 状态机 → sysfs 投影 → 事件源。
7. **同步文档**：`docs/design.md`、`docs/security.md`、各 crate `docs/`、rustdoc。

### 8.2 验收矩阵

| Gap | 验收 |
|-----|------|
| 5.1 cgroup controllers | `cgroup.controllers` 列出 pids/cpu/cpuset/io/memory；`subtree_control` 可逐级委派；`cpu.weight` 改变组份额；`cpu.max` 触发 throttle；`cpuset.cpus` 限制实际运行 CPU；`memory.max` 触发回收/OOM；`io.max` 限制吞吐 |
| 5.2 namespace | `unshare(CLONE_NEWUSER/NEWCGROUP/NEWTIME)` 成功且语义真实；`/proc/pid/ns/*` 可 setns；`uid_map/gid_map` 单次写入；time offset 影响 MONOTONIC/BOOTTIME；`clone3` 无效 cgroup fd 回滚 |
| 5.3 capability | `capget/capset` 往返一致；`PR_CAPBSET_DROP` 生效；ambient 真实升降；exec 对 setuid/file caps 正确重算；`/proc/pid/status` 五个 Cap 字段与 SecureBits 正确 |
| 5.4 hotplug | CPU offline/online 真实迁移任务与中断；memory 块 online/offline 与分配器一致；sysfs 目录随 present 变化 |

所有验收在 QEMU 中执行，单元测试覆盖状态机与错误路径，集成测试覆盖
`unshare`/`setns`/`clone3`/cgroup 写入。性能敏感路径（每页记账、调度组选择、
bio 提交）必须有基准或至少计数器，避免“功能正确但不可用”。

### 8.3 风险与应对

| 风险 | 应对 |
|------|------|
| 调度器组实体改造影响所有 fair 任务 | 先做组树单测与 `nice` 回归，再切默认路径；保留逐任务权重作为叶子层 |
| 每页原子记账成本高 | 先按页精确记账，基准后若需批量化，用 per-CPU delta + 限额同步刷新，不能牺牲限额准确性 |
| 页归属元数据增加内存开销 | `PageMeta` 只增加 4 字节 token；用编译期 size 断言和 `page-meta` 单测锁定 |
| CPU offline 中断迁移不完整 | 以“可迁移中断全部迁移，否则 offline 失败”为硬约束，不做丢中断的假成功 |
| ACPI AML 能力不足 | 只支持 QEMU 固定热插拔路径并在文档声明；通用 AML 留给后续 PR，不用启发式解析 |
| io 调度与块层队列深度耦合 | QoS 接口只暴露 admit/complete/dispatch_class，先实现 `io.max`，`io.weight` 在队列调度测试通过后启用 |
| 内存回收与 OOM 回调重入 | 回调在 `Memcg` 锁外执行；OOM 选择使用任务快照，杀进程后由下一次 charge 重试 |
