# uevent-netlink — 设计文档

## 定位

kobject uevent 的 **netlink 传输层**：订阅 `devnode` 的生命周期事件，把
`devnode::uevent` 序列化好的 payload 广播到 `KOBJECT_UEVENT` netlink 组。

它是 Linux `lib/kobject_uevent.c::kobject_uevent()` 的 netlink 那一半，也是
**drivers 树里唯一消费 `knet` 的 crate**。

它与 `devnode` 平级而不是住在它里面，理由是依赖姿态不同：`devnode` 是有状态的
纯数据模型（不做 I/O，7 个 crate 靠它而不想要 `knet`），本 crate 是无状态的
输出适配器。合并会把 `knet` 拖给全部 `devnode` 消费者，除非引入 cargo feature
扇出。

**方向是规则，不是门牌。** "框架 → 内核设施"这个约束由
`docs/ai/driver-subsystem-boundaries.md` 的方向规则和 review checklist 执行
（本 crate 不依赖 `knet` 以外的任何内核设施，命名空间是常量 1、发送是单向的）；
它不靠某个叫 `integration/` 的目录表达。将来出现第二个框架 → 内核适配器时，
它同样按自己的内容命名建 crate，不预先占一个空门牌。

## 范围

单文件 `src/lib.rs`（71 行）：

| 项 | 内容 |
|---|---|
| `init()` | 安装生命周期发布者（`klazy::Once` 保护，恰好一次） |
| `publish(action, node)` | 序列化 + 广播一个事件；sysfs `uevent` 文件写入路径也走这里 |
| `handle_lifecycle_event(event)` | `DevNodeEvent` → `UeventAction` 的映射 |
| `UEVENT_GROUP: u32 = 1` | Linux `NETLINK_KOBJECT_UEVENT` 的组号 |
| `tests` | 1 个测试：action 白名单拒绝注入 |

**格式不在这里。** payload 的属性顺序、NUL 分隔、`DEVPATH` 推导、bitmap 字序
全在 `devnode::uevent`——那是 udev 可见的兼容面，与 sysfs 属性共享同一份代码。

## 架构

```text
devnode 注册表
   │ DevNodeEvent::{Added, Removing, Removed}
   ▼
handle_lifecycle_event            sysfs `uevent` 文件写入
   │  Added   → UeventAction::Add        │  (用户态指定 action 字符串
   │  Removed → UeventAction::Remove     │   → UeventAction::try_from 白名单)
   │  Removing→ （不发）                  │
   └──────────────┬──────────────────────┘
                  ▼
          publish(action, node)
                  │  devnode::uevent::uevent_payload(action, node) → Vec<u8>
                  ▼
   knet::netlink::publish_kobject_uevent(UEVENT_GROUP, payload)
                  │
                  ▼
            用户态 udev / systemd-udevd
```

两条入口汇到同一个 `publish`，因此**文件触发的事件与自动事件永远不会格式不一致**。

## 调用约束 / 执行上下文

- `init()` 必须在**设备发现开始之前**调用。`devnode::install_lifecycle_publisher`
  对迟到的安装返回 `false`，本 crate 因此 `assert!` panic——启动不会静默漏掉
  早期的 add 事件。
- `handle_lifecycle_event` 作为 `devnode` 的订阅者回调运行，因此：
  - 在 `devnode` 的 `DISPATCH_ORDER` 锁**之外**、注册表锁**之外**被调用；
  - **任务上下文，可睡眠**（`knet` 的 netlink 发送会分配）；
  - **不得重入 `devnode` 的 register/withdraw**（会在 `DISPATCH_ORDER` 上自死锁）。
    本 crate 不重入——它只读 `DevNode` 快照并发送。
- `publish` 是 `pub`，sysfs 的 `uevent` 属性写入路径直接调它；调用者同样是
  任务上下文。
- `klazy::Once` 保证 `init()` 幂等，重复调用不会二次安装（二次安装会被
  `install_lifecycle_publisher` 拒绝并触发 assert）。

## 设计决策

### `Removing` 不发 uevent

Linux 的 kobject uevent 只有 `add`/`remove`/`change`/`move`/`online`/`offline`/
`bind`/`unbind`，没有"正在移除"。`devnode` 的三事件里 `Removing` 是给**内核内部
投影层**用的（devfs 在此 quiesce、停止接受新打开），对用户态没有对应语义，
所以这里显式丢弃。

### 传输与格式分离

把格式留在 `devnode::uevent` 有两个后果，都是刻意的：

1. 格式可以被独立测试（不需要 `knet`）；
2. sysfs 的 `uevent` 属性文件（`uevent_file_contents`，换行分隔）与 netlink
   payload（`uevent_payload`，NUL 分隔 + `action@devpath` 头）共享同一份
   `uevent_env`，因此两者的键值集合永远不会分叉。

代价是本 crate 只剩 71 行——但它换来了"`knet` 依赖被限制在一个 71 行的 crate 里"，
这也是它必须与 `devnode` 分成两个 crate 的全部理由。

### 组号是常量 1

`NETLINK_KOBJECT_UEVENT` 的 multicast group 在 Linux 里就是 1。这不是可配置项。

## Drop / 资源释放

本 crate 不持有资源。

- `INITIALIZATION: Once<()>` 与安装的 `Arc<dyn Fn>` 回调与内核同生命周期，
  没有注销路径（`devnode` 的 `lifecycle_publisher` 是单槽、不可卸载）。
- `uevent_payload` 每次调用分配一个 `Vec<u8>`，交给 `knet` 后由 netlink 层释放。
  发送失败时 `publish_kobject_uevent` 返回 `()`，**本 crate 不感知失败**。
