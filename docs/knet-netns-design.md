# knet 网络命名空间（netns）实例化设计

## 1. 文档状态

本文是 knet 网络命名空间实例化的实施设计，对应 `docs/docker-support.md` §3 现状盘点中
的 Phase-2 项目（"network namespace 实例化 —— knet 为全局状态"），也是 `docker run` 的
最后一个内核前置（libnetwork 默认沙箱创建需要 `unshare(CLONE_NEWNET)`）。

实施时须同步更新：

- `net/knet/docs/design.md`（架构章节新增 netns 一节）
- `net/knet/docs/security.md`（netns 作为隔离边界的分析）
- 公共 API rustdoc（`knet::netns`、`Socket::socket_type` 等）

## 2. 目标与非目标

### 2.1 目标

1. `clone3`/`clone`/`unshare` 支持 `CLONE_NEWNET`：新网络命名空间**真实隔离**——
   独立的设备表、地址/路由表、监听表、socket 集；新 ns 内只有一个 `lo` 设备
   （127.0.0.1 已配置、状态 DOWN，与 Linux `netns` 初始状态一致）。
2. `setns` 支持 `net` 类型（P3，见 §9 分期；P0–P2 不做）。
3. procfs `/proc/<pid>/ns/net` 报告**真实** ns 标识（inode 数值 = ns id），
   `stat()` 可区分不同 ns——libnetwork/runc 以此判断 ns 唯一性。
4. 现有全局行为完全保留：init ns 的设备（eth0/lo）、地址、路由、socket 行为
   与今天一致；无 netns 感知的旧进程全部落在 init ns。
5. `docker run`（`--network=host` 降级路径 + 默认沙箱创建）在 guest 内成功。

### 2.2 非目标

1. 跨 ns 的设备迁移（`RTM_NEWLINK` 的 `NETNS_FD` 移动）、vrf、bridge 设备。
2. 每 ns 独立的物理 NIC 直通：物理设备永远属于 init ns（RX 中断路径不变）。
3. `setns` 之外的 ns 生命周期管理（ns 销毁的引用计数只保证不 UAF，不做 GC 通知）。
4. vsock、AF_UNIX 的 ns 化（见 §7.6/§7.7 的明示降级）。
5. IPv6（knet 现状仅 IPv4）。

## 3. Linux 参考语义（对齐基准）

- `struct net` 是每 ns 的设备/路由/proc/net 顶点；`lo` 总是存在于每个新 ns
  （index 1，地址 127.0.0.1/8，IFF_UP 未置位）。
- `CLONE_NEWNET`：需要 `CAP_SYS_ADMIN`；新 ns 与父 ns 完全独立，之后二者各自
  演化（内核源码 `create_new_namespaces` → `copy_net_ns`）。
- socket 创建时绑定其所在 ns（`sk_net`），bind/connect/listen 的一切查找只看
  自己的 ns；`SO_BINDTODEVICE` 只能看到本 ns 设备。
- netlink（rtnetlink）的 dump/查询只见本 ns；`RTM_GETLINK` 等以 ns 的设备表为准。
- AF_UNIX **在 Linux 中是 per-netns 的**（`unix_find_socket_by*` 比较 ns），
  但容器内 connect 挂载进来的 `/var/run/docker.sock` 又确实能通——实施时以本地
  Linux 6.8 源码（`/root/codes/linux/net/unix/af_unix.c`）核对确切语义后决定
  AF_UNIX 的处理（见 §7.7）。
- 对照实现时遇到语义分歧，以 `/root/codes/linux` 本地源码为准，禁止凭记忆断言。

## 4. 现状盘点（knet 全局单例）

`net/knet/src/lib.rs` 三个全局单例（`LazyInit`）：

| 单例 | 定义 | 内容 | 引用面 |
|---|---|---|---|
| `LISTEN_TABLE` | `stack/listen_table.rs` | INET 监听端口表 | socket bind/listen |
| `SOCKET_SET` | `stack/wrapper.rs` | smoltcp socket 集 | TCP/UDP 传输层 |
| `SERVICE` | `stack/service.rs:97` | `Mutex<Router>` + smoltcp `Interface` + ingress/批处理 + 定时器 | 几乎所有 INET 路径 |

`Router`（`stack/router.rs:275`）：`devices: Vec<Box<dyn NetDevice>>`、
`ipv4_addrs`、`RouteTable`、rx/tx 队列。设备索引（`dev: usize`）被地址/路由引用。

引用面统计：`SERVICE|LISTEN_TABLE|SOCKET_SET` 共约 107 处（15 个文件），
分布：poller、link、netlink（rtnetlink）、control、transport（raw/udp）、
socket/general、stack（ingress/listen_table）、lib。

syscall 侧：`core/ksyscall/src/task/clone.rs` 对 `CLONE_NEWNET` 返回 `ENOSYS`；
`unshare`/`setns` 已有分发（`Sysno::unshare`/`Sysno::setns` → `posix`），
net 标志同样被拒。

## 5. 总体设计

### 5.1 核心对象

```rust
// net/knet/src/netns.rs（新文件）
pub struct NetNamespace {
    id: NetNsId,                     // u64 单调分配，>0；init ns 固定 1
    /// 本 ns 的协议栈。init ns 即现有全局 Service（整体迁移，不改行为）；
    /// 新 ns 为 loopback-only Service。
    service: Mutex<Service>,         // 或 RwLock，视现有锁粒度沿用
    listen_table: Mutex<ListenTable>,
    socket_set: SocketSetWrapper,
}
```

- `NetNsId`：`AtomicU64` 单调分配；**init ns 固定为 1**（procfs inode 直接用它）。
- 注册表：`static NETNS_TABLE: Mutex<HashMap<NetNsId, Arc<NetNamespace>>>`（`Weak`
  值 + init ns 强引用，进程退出释放 Arc 后自动清理）。
- 初始化：`init_network()` 建出的 `Service`/`LISTEN_TABLE`/`SOCKET_SET` 迁入
  `INIT_NET: OnceInit<Arc<NetNamespace>>`。**init ns 行为零变化**——原单例的
  访问路径全部改为 `INIT_NET` 成员（编译器保证迁移完备性：删除原 static）。

### 5.2 新增 `knetns` leaf crate + kiface 解耦

依赖现状：`knet → kprocess` 单向已存在（`current_cred`/`current_fs_context`，
见 `net/knet/src/socket/file.rs:76`、`unix.rs:164`）。但 netns 要求
**kprocess 的 `Process` 持有 ns 标识**，而 `NetNamespace` 对象持有 knet 的
`Service`/`ListenTable`/`SocketSet`——若 kprocess 直接持有 knet 类型即成环
（kprocess → knet → kprocess）。

解法：新增 **`knetns` leaf crate**（`process/knetns/`，仅依赖 `kerrno`/核心
基础 crate，零业务依赖）承载双方共享的最小契约：

```rust
// knetns/src/lib.rs
/// 网络命名空间标识。1 = init ns；新 ns 单调分配。
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub struct NetNsId(pub core::num::NonZeroU64);

/// 当前 netns 解析接口。kprocess 提供实现（读 Process::net_ns），
/// knet 通过 kiface facade 消费——knet 无需在 Cargo 上依赖 kprocess 的新路径。
#[kiface::interface]
pub trait NetNsResolver {
    /// 返回当前任务所属进程的 netns id。任意上下文可调用、不睡眠。
    fn current() -> NetNsId;
}
```

职责划分：

| crate | 内容 |
|---|---|
| `knetns`（新） | `NetNsId`、`NetNsResolver` 接口契约（kiface）、`INIT_NET_ID` 常量 |
| `knet` | `NetNamespace` 对象（持有 Service/ListenTable/SocketSet）、id→ns 注册表、`netns::current()`（经 `NetNsResolver::current()` 解析 id 再查表）、`create_fresh()` |
| `kprocess` | `Process.net_ns: NetNsId`（默认 `INIT_NET_ID`）；`#[kiface::provide] impl NetNsResolver`；clone/unshare 时由 ksyscall 设置新 id |
| `core/ksyscall` | 编排：`clone/unshare(CLONE_NEWNET)` → `knet::netns::create_fresh()` 取新 id → 写回 `Process.net_ns` |

要点：

- kiface 的 provide 以链接符号导出，knet 消费接口时**不需要在 Cargo 上依赖
  kprocess 的新增路径**，与 `khal::ConsoleIf`（drivers/platform/console 提供）、
  khal time 同款仓库惯例。
- provider 约束：`NetNsResolver::current()` 任意上下文可调用、不睡眠
  （`Process.net_ns` 为无锁读取字段），文档与 `#[kiface::interface]` 注释写明。
- knet 侧查询顺序：id → registry 查 `Arc<NetNamespace>`；查不到（理论不可达）
  回退 INIT_NET 并 `warn!`。
- 依赖图（箭头=depends-on）：
  `posix/net → knet → knetns ← kprocess`；`ksyscall → {knet, kprocess, posix/net}`。
  无环。

### 5.3 新 ns 的形态

`knet::netns::create_fresh() -> Arc<NetNamespace>`：

- `Router` 只含一个 `LoopbackDevice`（127.0.0.1/8 已配、UP 未置位——对齐 Linux；
  libnetwork 沙箱随后会 `ip link set lo up`）。
- `Service` 新增 loopback-only 构造器（`Service::new_loopback()`）：smoltcp
  `Interface` 用回环配置创建；无物理设备、无 eth0。定时器采样对它的额外开销
  为空转（见 5.4）。
- `ListenTable`/`SocketSet` 全新空表。

### 5.4 定时器与 RX 路径

- `poller.rs` 的周期采样从“轮询单个 SERVICE”改为“遍历 NETNS_TABLE 中所有**存活**
  ns 的 service”（registry 迭代接口；锁顺序：registry 短锁收集快照 → 逐个轮询）。
- 物理 NIC RX 中断/入口路径（`device/net_rx.rs`、`link/`）**不变**：数据仍然
  只进入 init ns 的 router。`NetDevice` 记录其归属 ns（P0 可省略，物理设备仅
  init ns 持有）。

### 5.5 socket 与 ns 的绑定

- socket 创建时（`posix/net` 的 `sys_socket`）捕获 `netns::current()` 并存入
  socket（`SocketFileOps` 层或 `Socket` 各族结构的公共旁路表）。
  实现取舍：**优先在 knet::Socket 外层**（`SocketFileOps` 私有字段
  `ns: Arc<NetNamespace>`）而非各族结构内——迁移面最小。
- socket 的所有 INET 操作（bind/connect/listen/send/recv/poll/setsockopt）从
  `SERVICE` 全局改走 `socket.ns.service()`。
- 遗留语义：ns 销毁（进程全退）时 socket 可能仍持有 Arc——ns 对象以 Arc 存活，
  无 UAF；socket 后续操作作用于已孤立的 ns（与 Linux 行为一致）。

### 5.6 syscall 层

- `clone.rs`：`CLONE_NEWNET` 从 ENOSYS 改为
  `net_ns = Some(knet::netns::create_fresh())`；`NEWNET|NEWUSER` 等组合校验保持
  现有规则。
- `unshare`：`CLONE_NEWNET` → 当前进程 `net_ns` 替换为 fresh ns（线程组级别，
  与 Linux 一致：unshare 影响 calling thread 的 ns 视图；本内核 Process 即
  线程组，明示文档化该近似）。
- `setns`（P3）：仅支持 `net`；fd 必须指向 `/proc/<pid>/ns/net`（见 §6）。

### 5.7 明示降级（写入 design.md/security.md）

- **knet 已有 kprocess 依赖保持不变**（cred/fs_context 直调），netns 解析新增路径一律走 knetns/kiface，不扩大 knet→kprocess 直接面。
- **AF_UNIX 维持全局**（不随 netns 隔离）。这是对 Linux 的偏差：容器内因此可以
  连接宿主 `/var/run/docker.sock`（对 Docker 用例反而是必要行为）。实施时先按
  §3 核对 Linux 6.8 af_unix.c 真实语义并在 design.md 记录差异与理由。
- **vsock 维护全局**：新 ns 内创建 vsock socket 返回 `EOPNOTSUPP`（如实拒绝，
  不假装有设备）。
- 新 ns 内的 raw/packet socket：跟随 §5.5 绑定 fresh ns，设备表只有 lo，
  `PACKET_*` 自然无设备可用（返回 ENODEV 类错误，如实）。

## 6. procfs 与 ns 标识

`fs/filesystems/procfs/src/task_nodes/root.rs` 的 `namespace_inode()` 目前对
net 返回固定值 `0xf000_0003`（`namespace_link_target()` 输出 `net:[0xf0000003]`）。

- 改为从进程 `net_ns` 读取：link target = `net:[<ns_id>]`，`stat()` 的
  `st_ino = ns_id`、`st_dev` 用固定 magic dev。
- libnetwork/runc 通过 `stat(/proc/self/ns/net).ino` 判断 ns 唯一性——**必须**
  不同 ns 得到不同 ino。ns id 直接作 ino（>1 的 u64）。
- `/proc/<pid>/net/`（若已实现）应读 ns 的 router；未实现则维持现状并记录。

## 7. 子系统迁移清单（P0–P2 实施表）

以下“改走 current()”均指：把对原 static 的引用改为
`netns::current().<member>()`。原三个 static 删除，编译器保证无遗漏。

| # | 文件 | 迁移内容 | 阶段 |
|---|---|---|---|
| 1 | `lib.rs` | 三 static 迁入 `INIT_NET`；新增 `netns` 模块；`init_network` 收尾建 INIT_NET | P0 |
| 2 | `stack/service.rs` | `Service::new_loopback()`；`Service` 不再隐含全局语义 | P0 |
| 3 | `netns.rs`（新） | NetNamespace/registry/current()/create_fresh()/provider | P0 |
| 4 | `poller.rs` | 遍历 registry 轮询各 ns service | P0 |
| 5 | `stack/listen_table.rs`、`stack/wrapper.rs` | 成员化为 ns 字段；访问器改造 | P0 |
| 6 | `socket/general.rs`、`transport/{udp,raw,tcp}`、`stack/ingress.rs`、`control.rs` | SERVICE 引用改 current() | P1 |
| 7 | `netlink/rtnetlink.rs`、`netlink/socket.rs` | GETLINK/GETADDR/GETROUTE 等 dump 改 current() 的 router | P1 |
| 8 | `link/*`、`device/net_rx.rs` | 确认 RX 只进 init ns（物理设备归属 init ns），显式断言 | P1 |
| 9 | `core/ksyscall/src/task/clone.rs`、`posix` 的 unshare | NEWNET 语义实现 | P1 |
| 10 | `fs/filesystems/procfs` | ns/net 真实 id（§6） | P2 |
| 11 | `posix/net` | socket 创建时绑定 ns；INET 操作走绑定 ns | P2 |
| 12 | `setns`（net） | P3，独立提交 | P3 |
| 13 | 文档 | knet design.md/security.md/rustdoc | 随各阶段 |

锁与并发约束：

- registry 锁只做“收集 Arc 快照”，禁止在持 registry 锁时进入 service 轮询。
- `Service`/`Router` 现有锁粒度不变；netns 引入的仅是外层 Arc 生命周期管理。
- provider 回调路径不得睡眠、不得递归取 kprocess 大锁（从 Process 的
  SpinNoIrq 字段直接读）。

## 8. 错误语义（对齐 Linux）

| 操作 | 条件 | 结果 |
|---|---|---|
| `clone/unshare(CLONE_NEWNET)` | 无 CAP_SYS_ADMIN | `EPERM`（当前内核恒有全权 cap，暂走不到） |
| 新 ns 内 `bind()` INET | 无 IPv4 地址（lo 的 127.0.0.1 可用） | 按 Linux：addr 查表失败 `EADDRNOTAVAIL` |
| 新 ns 内 packet socket | 无设备 | `ENODEV` |
| 新 ns 内 vsock | 明示降级 | `EOPNOTSUPP` |
| `setns(net)`（P3） | fd 非 ns 文件 | `EINVAL`；权限不足 `EPERM` |

## 9. 实施分期与验收

- **P0**：#1–#5 + `create_fresh()`（骨架：ns 对象、registry、init 迁移、poller）。
  验收：`make build` + knet/kvfs 单测全绿；现有 guest 启动 + 网络行为（apt 可用）
  不回归。
- **P1**：#6–#9（syscall 语义接通）。验收：guest 内 `unshare -n true` 成功；
  `unshare -n sh -c 'ip link'`（若无 ip 命令则用 netlink dump 的 ctr/busybox 等价物）
  只见 lo；init ns 的 apt/TLS 行为不回归。
- **P2**：#10–#11 + procfs 真实 id。验收：`unshare -n sh -c 'cat /proc/self/ns/net'`
  两次得到不同 id；init ns id 恒为 1。
- **P3**：setns + AF_UNIX 语义核对结论落文档。
- **最终验收（docker）**：guest 内
  `docker run --network=host --rm hello-world` 输出 hello 横幅；
  libnetwork 默认沙箱创建成功（不再报 `namespace creation reexec failed`）。

## 10. 风险与开放问题

1. **Service::new_loopback 的 smoltcp 适配**：`Interface::new` 依赖设备/时钟配置，
   loopback-only 形态需确认 smoltcp 不要求物理设备存在（LoopbackDevice 即
   smoltcp 的 Device）。
2. **poller 遍历开销**：ns 数量在 Docker 场景 = 1（init）+ 沙箱数（个位数），
   空转 service 的采样成本可忽略；但 registry 迭代须避免长锁。
3. **AF_UNIX 语义**：若核对后 Linux 确为 per-netns，维持全局属显式偏差，
   须在 security.md 论证（fs 路径查找 + docker.sock 用例依赖）。
4. **unshare 的线程组粒度**：Linux 的 netns 是 per-process（线程共享），
   本内核 Process 即线程组，行为等价；写入 design.md 即可。
5. **与 cgroup/mount ns 的组合**：clone 多标志组合已有一致性校验代码，
   NEWNET 加入后沿用同一校验路径。

## 11. 防伪自检（提交前逐条回答）

- 新 ns 是否真的看不到 init ns 的设备/路由/socket？（负向测试：新 ns 内
  netlink dump 不含 eth0；connect init ns 的监听端口失败）
- init ns 是否真的零行为变化？（回归：apt/TLS/docker pull 全链路）
- 所有“未支持”路径是否返回真实错误码而非静默成功？
