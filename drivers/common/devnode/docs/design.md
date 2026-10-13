# devnode — 设计文档

## 定位

用户态设备节点投影的**单一注册表**：devtmpfs（`/dev`）、sysfs（`/sys`）、
uevent（netlink）三个投影层都从这里取事实。

它是 Linux `drivers/base/devtmpfs.c` + `kobject_uevent` 的身份层合并：
一个节点一次注册，三个投影各自订阅。

依赖它的：`common/device-class`（class 发布时投影节点）、`subsystems/block`
（Gendisk 的 devname/设备号）、`fs/filesystems/devfs`、`fs/filesystems/sysfs`、
`integration/uevent-netlink`。

它依赖 `driver-core`，但只用一个类型：`DeviceLocation`（见"设计决策"）。

## 背景

旧模型把这一层放在 `kdevice/src/device_node.rs`（808 行），因此**模型 crate
认识了具体子系统**：`enum DeviceNodeKind { Input, Drm }` 硬编码两个类，
新增 tty/block/netdev 投影必须改模型。

本 crate 把它换成**数据驱动**：`DevNodeType` 是一个值（subsystem 名、major、
minor base、index 数、devname 格式函数、sysfs 路径格式函数），由 class crate
注册。新增子系统不触碰 devnode，也不触碰 `driver-core`。

同时它把 uevent 的**格式**与**传输**分开：格式（udev 可见的兼容面）在这里的
`uevent.rs`，传输（唯一消费 `knet` 的地方）在 `integration/uevent-netlink`。

## 范围

| 文件 | 内容 |
|---|---|
| `src/lib.rs` | `DeviceNumber`（打包 `dev_t`）、`DevNodeType`、`DevNode`、`DevNodeEvent`、注册表（`REGISTRY` / `INDEX_POOLS` / `DISPATCH_ORDER`）、`publish` / `publish_auto_indexed` / `withdraw` / `nodes` / `subscribe` / `install_lifecycle_publisher`、`physical_path` |
| `src/uevent.rs` | `UeventAction`、`uevent_devpath`、`uevent_env`、`uevent_payload`、`uevent_file_contents`、`bitmap_value` |

## 架构

```text
   class crate 注册 DevNodeType（数据）
   ┌────────────────────────────────────────────┐
   │ DevNodeType { subsystem, major,            │
   │               minor_base, index_count,     │
   │               devname_for(index),          │
   │               sysfs_path_for(phys, index) }│
   └───────────────────┬────────────────────────┘
                       │ publish_auto_indexed / publish
                       ▼
   ┌──────────────────────────────────────────────────────┐
   │  REGISTRY: SpinNoPreempt<Registry>                   │
   │    nodes: Vec<RegistryEntry{ node, added_dispatched }>│
   │    subscribers: Vec<DevNodeCallback>                  │
   │    lifecycle_publisher: Option<DevNodeCallback>       │
   │  INDEX_POOLS: SpinNoPreempt<IndexPools>  (min-free)  │
   │  DISPATCH_ORDER: ksync::Mutex<()>        (事件定序)   │
   └──────┬───────────────────┬───────────────────┬───────┘
          │ Added/Removing/Removed                 │ nodes()
          ▼                   ▼                    ▼
       devfs               sysfs           uevent-netlink
    (/dev 投影)         (/sys 树)         (KOBJECT_UEVENT 广播)
```

三个投影都是**订阅者**，devnode 不知道它们存在。`lifecycle_publisher` 是唯一的
单槽回调（由 `uevent-netlink::init()` 安装），用于让"自动生命周期事件"与
"sysfs `uevent` 文件写入触发的事件"走同一条路径，两者永远不会不一致。

## 调用约束 / 执行上下文

- `publish` / `publish_auto_indexed` / `withdraw` 在**任务上下文**调用（probe /
  remove 路径）。它们本身只取 `SpinNoPreempt`，但**事件派发在锁外**，而订阅者
  会睡眠（devfs 取任务上下文 `Mutex`、sysfs 重建树、uevent 分配并发送）。
- **订阅者回调不得重入 devnode 的 register/withdraw**——会在 `DISPATCH_ORDER`
  这把 `ksync::Mutex` 上自死锁。当前三个订阅者（devfs/sysfs/uevent）都不重入。
- `install_lifecycle_publisher` 必须在**设备发现开始之前**调用；它对迟到的安装
  返回 `false`（`uevent-netlink::init()` 因此 `assert!`），这样启动不会静默漏掉
  早期的 add 事件。
- `nodes()` / `subscribe()` 是只读快照，可在任何上下文调用（`subscribe` 会取
  注册表锁，仍是 `SpinNoPreempt`，不可在硬中断里调用）。

## 状态机

节点没有显式状态字段，状态由"在不在注册表里"+ `added_dispatched` 表达。
**事件顺序是状态机**：

```text
publish ──► [在注册表, added_dispatched=false]
                 │  取 DISPATCH_ORDER，锁外派发
                 ▼
            Added(node) ──► added_dispatched = true
                 │
   withdraw ─────┤
                 ▼
        Removing(node)   （投影层 quiesce：停止接受新打开）
                 │
                 ▼
        Removed(node)    （投影层 delete）
                 │
                 ▼
        [从注册表移除 + index 归还池]
```

两个不变量：

1. **`Added` 恰好派发一次，且总在该节点的 `Removing` 之前。** 每条派发路径都
   先取 `DISPATCH_ORDER` 并在其下重查 `added_dispatched`；发现 `Added` 还没派发
   的 `withdraw` 会**自己接管派发**，按顺序补上。
2. **`added_dispatched` 只存在于 `RegistryEntry`，绝不出现在公开的 `DevNode` 上。**
   否则它会泄漏进订阅者的节点比较里。

### 为什么需要 `DISPATCH_ORDER` 这把额外的锁

注册表锁是 `SpinNoPreempt`（禁抢占），而订阅者是**会阻塞**的真实投影层。
在禁抢占下睡眠可能把核卡死，所以派发必须在注册表锁外。

但"插入"与"派发 Added"分成两步就重新打开了历史上的乱序：一个并发的 `withdraw`
可能在 `Added` 派发之前跑完整个 Removing → Removed → delete 序列，让订阅者看到
一个注册表里已经不存在的设备。`DISPATCH_ORDER` 只序列化**事件派发顺序**（不序列化
注册表修改），配合锁下重查 `added_dispatched`，同时拿到"锁外派发"和"顺序正确"。

## 算法流程

### `publish_auto_indexed`

```text
1. 先在 REGISTRY 里查重（必须在注册之前：注册后查总会查到自己，
   并把刚分配的 index 原样还回去）
2. INDEX_POOLS.alloc(subsystem, index_count)
     min-free：在有序 used 集合里找第一个空洞
     耗尽 → PublishError::ResourceBusy（绝不静默别名）
3. node_type.devname(index) / sysfs_path_for(physical_path, index)
   DeviceNumber::new(major, minor_base + index)
4. 插入 REGISTRY（added_dispatched = false）
5. 取 DISPATCH_ORDER，锁外派发 Added
6. 若有 lifecycle_publisher，同时给它 Added（sysfs uevent 文件写入
   与自动事件走同一条路径）
```

### `withdraw`

```text
1. 取 DISPATCH_ORDER
2. 锁下找到 entry；若 added_dispatched == false，先接管派发 Added
3. 锁外派发 Removing → 投影层 quiesce
4. 锁外派发 Removed  → 投影层 delete
5. 锁下从 REGISTRY 移除 + INDEX_POOLS.release(subsystem, index)
```

index 在第 5 步才归还，因此 Removing/Removed 期间不会有新节点拿到同一个
index——否则订阅者会看到"同一个 devname 先 remove 后 add"颠倒。

### `physical_path`

`DeviceLocation` → 规范 sysfs 物理路径（`/devices/platform/soc/9000000.pl011`、
`/devices/pci0000:00/0000:00:01.0`）。这是 `DEVPATH` 与目录树的**唯一出处**：
旧模型有两套推导（节点上一套、uevent 一套），会分叉。

## 并发模型

| 状态 | 保护 | 说明 |
|---|---|---|
| `REGISTRY` | `SpinNoPreempt<Registry>` | 只做表操作；**锁内绝不派发事件** |
| `INDEX_POOLS` | `SpinNoPreempt<IndexPools>` | `Vec<(&'static str, Vec<u32>)>`，subsystem 数量是个位数，线性查找可接受 |
| `DISPATCH_ORDER` | `ksync::Mutex<()>`（可睡眠） | 只序列化派发顺序 |

锁序：`DISPATCH_ORDER` → （短暂）`REGISTRY` / `INDEX_POOLS`。反向从不发生
（注册表锁内不取派发锁）。

`nodes: Vec<RegistryEntry>` 与 `subscribers: Vec<DevNodeCallback>` 都是线性扫描。
节点数与订阅者数都是个位数到几十，`nodes()` 会 clone 整个 `DevNode` 列表——
这是 sysfs/devfs 重建路径，不是热路径。

## 设计决策

### `DevNodeType` 是数据，不是枚举

```rust
pub struct DevNodeType {
    pub subsystem: &'static str,
    pub major: u32,
    pub minor_base: u32,
    pub index_count: u32,
    pub devname_for: fn(index: u32) -> String,
    pub sysfs_path_for: fn(physical_path: &str, index: u32) -> String,
}
```

两个 `fn` 指针是 Linux `struct class` 的 `devnode` 回调的等价物。加一个子系统
= 加一个 `static DevNodeType`，不改 devnode，不改 `driver-core`。

### `DeviceNumber` 自己打包 `dev_t`

```rust
(major & 0xffff_f000) << 32 | (major & 0x0000_0fff) << 8
    | (minor & 0xffff_ff00) << 12 | (minor & 0x0000_00ff)
```

与 Linux 的 `new_encode_dev` 位布局一致，也与 block core / devfs 使用的布局一致
（两边都把这个类型 re-export 成自己的 `DeviceId`）。位运算在这里是**兼容面**，
不是实现细节——改它会同时破坏 `/dev` 的 `st_rdev` 与 uevent 的 `MAJOR`/`MINOR`。

### uevent 格式与传输分离

`uevent.rs` 只产生 `Vec<u8>` payload；`uevent-netlink` 只负责
`knet::netlink::publish_kobject_uevent`。这样格式（udev 可见、属性顺序与
bitmap 字序是 load-bearing 的）可以独立测试，而 `knet` 依赖被限制在一个 71 行
的 crate 里。

### 依赖 `driver-core` 只为 `DeviceLocation`

```rust
pub use driver_core::DeviceLocation;
```

本 crate 曾经真的是"双模型无关"的（旧框架时期需要同时服务新旧两个设备模型，
因此自己复制了一份 `DeviceLocation`）。旧模型删除后这份复制也随之删除，
`DeviceLocation` 现在只有 `driver-core` 一个定义点，本 crate re-export 它，
让消费者保持单一 import 路径。

**因此"model-agnostic"这个说法现在只对了一半**：投影机制（`DevNodeType` /
`DevNode` / 注册表 / index 池 / uevent 格式）确实与模型无关，但**位置身份的
类型**来自 `driver-core`。这是刻意的取舍——重复一份 `DeviceLocation` 会让
sysfs 路径推导有两个出处，那正是本 crate 要消灭的问题。

## Drop / 资源释放

- `DevNode` 是纯数据（`String` / `Vec<(String,String)>` / `DeviceNumber`），
  clone 廉价，没有 Drop 语义。
- **index 只在 `withdraw` 里归还**（`IndexPools::release`）。一个从未 withdraw
  的发布会永久占用它的 index；耗尽表现为 `PublishError::ResourceBusy`，
  不是静默别名。
- `subscribers` 与 `lifecycle_publisher` 是 `Arc<dyn Fn + Send + Sync>`，
  没有注销 API——它们与内核同生命周期。
- `RegistryEntry.added_dispatched` 随 entry 一起从 `Vec` 移除，无独立释放。
