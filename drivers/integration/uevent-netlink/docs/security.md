# uevent-netlink — 安全与可靠性分析

## Scope

`drivers/integration/uevent-netlink/src/lib.rs`（71 行）。

这个 crate 小，但它坐在一条**内核 → 用户态的广播通道**上，且有一个用户态可写
的入口（sysfs `uevent` 文件）。因此它的攻击面比行数暗示的要大。

## 信任模型

| 边界 | 谁在外面 | 假设 |
|---|---|---|
| `devnode` → 本 crate | `DevNodeEvent` 回调 | 可信。`DevNode` 是注册表里的快照 |
| **sysfs `uevent` 文件 → 本 crate** | 用户态写入的 action 字符串 | **不可信**。唯一的用户输入入口 |
| 本 crate → `knet` | payload 字节 | 本 crate 负责 payload 的**结构**正确；内容来自 `devnode` |
| 本 crate → 用户态监听者 | netlink 广播 | 用户态是消费者。payload 是 udev 可见的兼容面 |

## 外部边界 / 攻击面

| 输入源 | 入口 | 说明 |
|---|---|---|
| **用户态写入**（sysfs `uevent` 属性） | `publish(action, node)` 的 `action` 参数，由 sysfs 层用 `UeventAction::try_from(&str)` 转换 | **主要攻击面**。转换在 sysfs 层完成，本 crate 收到的是已验证的枚举值 |
| **用户态监听**（netlink 组 1） | `knet::netlink::publish_kobject_uevent` | 输出面。任何加入该组的 socket 都会收到全部事件 |
| 设备 / 固件 | 间接：经 `devnode` 的 `DevNode` 字段 | 见 `devnode/docs/security.md` T-02 |
| 用户内存指针 | 无 | |
| MMIO / DMA / 中断 | 无 | |

## unsafe 代码清单

无。

## 内存安全不变量

内存安全上没有非平凡不变量（无 unsafe、无裸指针、无手工生命周期）。
**协议正确性**上的不变量：

1. **`UeventAction` 是封闭枚举**，本 crate 收到的 action 只可能是 8 个 Linux
   kobject action 之一。字符串 → 枚举的转换发生在**边界上**（sysfs 层），
   而不是在序列化时——因此 `uevent_payload` 里的 `action.as_str()` 永远不可能
   含 NUL 或 `=`。
2. **payload 的 NUL 分隔结构由 `devnode::uevent` 独家产生**。本 crate 不拼接
   任何字符串，只把 `Vec<u8>` 交给 netlink。因此本 crate 不可能引入格式错误。
3. **`init()` 恰好成功一次**（`klazy::Once`），且失败即 panic。不存在
   "两个发布者都装上了，每个事件发两遍"的状态。

## 线程安全

- 无内部可变状态（`INITIALIZATION` 是 `Once`，`UEVENT_GROUP` 是常量）。
- `handle_lifecycle_event` 是 `Arc<dyn Fn + Send + Sync>`，可跨 CPU 并发调用。
  因此**同一时刻可能有多个 uevent 在发送**；netlink 层的顺序保证由 `knet` 负责，
  本 crate 不做序列化。事件之间的相对顺序由 `devnode` 的 `DISPATCH_ORDER`
  保证（那是派发顺序，不是发送完成顺序）。
- 在 `devnode` 的派发路径上运行时处于任务上下文，可睡眠。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|---|---|---|---|---|
| T-01 | 用户态通过 sysfs `uevent` 写入注入伪造 action 或额外环境变量（例如 `"add\0SUBSYSTEM=block"`、`"add\nDEVNAME=/dev/sda"`），让 udev 跑错误规则——可能触发模块加载或设备节点创建 | 高 | 有 sysfs 写权限的进程（通常是 root） | **已实现**：`UeventAction::try_from` 是**精确字符串白名单**，任何多余字节即 `Err`；本 crate 的 `publish` 只接受已转换的枚举值，因此不变量 1 成立。回归测试 `actions_accept_only_the_linux_kobject_set` 钉住 `"change"` 通过、`"change\0remove"` 被拒。**残余风险**：payload 的**其余字段**（DEVNAME/properties）不经这个白名单——它们的过滤缺口在 `devnode`（见该 crate 的 T-02） |
| T-02 | 用户态伪造 `remove` 事件，让 udev 删除一个仍在使用的 `/dev` 节点 | 中 | sysfs 写权限 | **部分实现**：sysfs 触发的 `publish` 用的是**注册表里真实的 `DevNode`**，因此 DEVPATH/DEVNAME 与真实设备一致——伪造的只有 action。**残余风险**：一个假的 `remove` 确实会让 udev 删掉节点。这是 Linux 的同样行为（root 写 sysfs `uevent` 就能做到），缓解在于 sysfs 写权限本身就是 root 特权。加固建议（若需要）：限制 sysfs `uevent` 只接受 `add`/`change` |
| T-03 | uevent 洪泛：大量设备事件让 netlink 缓冲区溢出，用户态丢事件 | 中 | 启动期批量枚举（实测 56 个 DT 描述符）或热插拔风暴 | **未实现**。本 crate 不做速率限制，也不感知发送失败（`publish_kobject_uevent` 返回 `()`）。**残余风险**：udev 错过事件会导致 `/dev` 与内核状态不一致，直到下次 `udevadm trigger`。加固建议：让 netlink 层返回发送结果，本 crate 在失败时计数并在 sysfs 暴露 |
| T-04 | 事件顺序颠倒，udev 先看到 `remove` 后看到 `add` | 中 | 并发派发 | **部分实现**：`devnode` 的 `DISPATCH_ORDER` 保证单个节点的 `Added → Removing → Removed` 顺序被**观察到**；但本 crate 的发送是并发的，**发送完成顺序**不保证。Linux 有同样的性质（netlink 是 datagram，单 socket 有序但多 CPU 派发本身可交错） |
| T-05 | `init()` 未被调用，所有 uevent 静默丢失 | 中 | 初始化顺序变化 | **已实现**：`install_lifecycle_publisher` 对迟到安装返回 `false`，`init()` 里 `assert!` → 启动期 panic。选择 panic 而不是告警，因为"没有 uevent"会让 udev 不创建符号链接、不加载模块，故障表现远离根因 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|---|---|---|---|---|---|---|
| F-01 | `init()` 在设备发现之后调用 | 初始化顺序被改 | `install_lifecycle_publisher` 返回 `false` | **panic**，停机 | 2 | 刻意的响亮失败（T-05）。早期的 add 事件如果漏掉，udev 永远不会为那些设备建节点，而故障现场在用户态 |
| F-02 | netlink 发送失败 | 无监听者、缓冲区满、`knet` 内部错误 | 该事件丢失 | 用户态看不到这个设备变化 | **2** | **未实现**：`publish_kobject_uevent` 返回 `()`，本 crate 无错误通道，不记日志、不计数。这是本 crate 最主要的可靠性缺口 |
| F-03 | payload 分配失败 | 内存紧张 | 该事件丢失或 panic（取决于 `Vec` 分配失败的策略） | 同 F-02 | 3 | `uevent_payload` 返回 `Vec<u8>`，分配失败在 `alloc` 层处理 |
| F-04 | `DevNodeEvent::Removing` 被丢弃 | 设计（Linux 无对应 action） | 无 uevent | 无。内核内部投影层（devfs quiesce）仍收到该事件 | 4 | `handle_lifecycle_event` 显式 `Removing(_) => {}`，不是遗漏 |
| F-05 | 重复 `init()` | 调用点增加 | `Once` 短路，无操作 | 无 | 4 | `klazy::Once` |

## 故障管理

- 本 crate **不返回任何 `Result`**：`init()` 是 `()`（失败即 assert panic），
  `publish()` 是 `()`（失败静默）。
- 唯一的 panic 是 `init()` 里的 `assert!`，它断言的是启动顺序契约。
- **没有失败可见性**（F-02）：既没有日志，也没有计数器。相比之下
  `devnode`/`driver-base` 至少在关键路径上 `log::warn`。这是本 crate 应该改进
  的第一项。

## 隐私分析

**需要注意**：uevent 是**广播**，任何加入 netlink 组 1 的 socket 都会收到全部
事件。payload 包含 `DEVPATH`、`SUBSYSTEM`、`MAJOR`/`MINOR`、`DEVNAME`、
`MODALIAS` 以及 class 携带的 properties。

- 这些都是硬件描述，与 Linux 的暴露面等价，不含用户数据。
- 但 input 设备的 properties 含 identity（物理位置、unique id）。新增 property
  时应确认它不含用户可识别信息——**一旦发出就是 udev 可见的兼容面**，
  事后收回会破坏用户态。

## 已知限制

1. **发送失败完全静默**（F-02）。无日志、无计数、无 sysfs 暴露。
2. **无速率限制**（T-03）。
3. **发送完成顺序不保证**（T-04）。
4. **`lifecycle_publisher` 不可卸载**，本 crate 的回调与内核同生命周期。
5. **sysfs 触发的 `publish` 可以伪造 action**（T-02），与 Linux 行为一致。
6. **只支持 netlink 传输**。Linux 还有 `CONFIG_UEVENT_HELPER` 的
   fork-and-exec 路径（已被上游弃用），本 crate 不实现，这是正确的。

## 审计清单

- [ ] 新增的 action 是否来自 `UeventAction` 的封闭集合？（不变量 1）如果引入了
      从字符串直接拼 payload 的路径，T-01 的防御就失效了。
- [ ] 有没有在本 crate 里拼接 payload 字符串？格式必须只在 `devnode::uevent`
      里产生（不变量 2）。
- [ ] `init()` 的调用点是否仍在设备发现之前？（F-01）
- [ ] 回调里有没有重入 `devnode` 的 register/withdraw？（会在 `DISPATCH_ORDER`
      上自死锁）
- [ ] 新增的 property 是否含用户可识别信息？（隐私分析）
- [ ] 如果给 `publish` 加了错误处理，是否同时给 sysfs 写入路径与自动路径都加了？
      两条入口必须保持行为一致，否则文件触发与自动事件会不一致。
