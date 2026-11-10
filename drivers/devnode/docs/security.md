# devnode — 安全与可靠性分析

## Scope

`drivers/devnode/src/lib.rs` 与 `src/uevent.rs`。

本 crate 的输出**直接构成用户态可见的兼容面**：`/dev` 下的节点名与 `st_rdev`、
`/sys` 下的路径、以及 udev 消费的 netlink uevent payload。因此它的正确性问题
不只是内核内部状态错误，而是用户态工具行为错误。

## 信任模型

| 边界 | 谁在外面 | 本 crate 假设什么 |
|---|---|---|
| class crate → devnode | `drivers/base` 的 class 层 / `subsystems/block` 调 `publish*` | **可信内核代码**。`DevNodeType` 是编译期常量，`physical_path` 由发布者填；注册表 key 是类型化的 `PublicationId`（发布者传入自己拿到的 class 句柄），裸整数进不了发布路径 |
| 驱动 → class crate → devnode | 驱动提供 devname 的来源字符串、modalias、properties | **半可信**：modalias 来自 const id 表（可信），devname 今天来自 index 派生（`vd{a,b,…}`、`event{N}`），但**类型上没有约束**（见 T-02） |
| devnode → 订阅者 | devfs / sysfs / uevent-netlink | 订阅者**不得重入** register/withdraw |
| devnode → 用户态 | netlink payload、`/dev` 节点、sysfs 属性 | 用户态是 payload 的**消费者**，不是输入源。唯一例外是 sysfs `uevent` 文件的写入路径，它经 `lifecycle_publisher` 复用同一条格式化代码 |

## 外部边界 / 攻击面

| 输入源 | 入口 | 说明 |
|---|---|---|
| **sysfs `uevent` 文件写入**（用户态可写） | `lifecycle_publisher` → `uevent-netlink::publish(action, node)` | `UeventAction::try_from(&str)` 是**封闭集合白名单**，`"change\0remove"` 这类注入被拒（有回归测试）。node 内容来自注册表，不是用户输入 |
| **uevent netlink payload**（用户态可读） | `uevent_payload` | 输出面。NUL 分隔的 `KEY=VALUE` 序列，格式是 udev 兼容面 |
| 固件 / 设备 | 间接：设备名可能最终成为 devname | 今天不可达（见 T-02），但没有类型 barrier |
| 用户内存 / 用户指针 | 无 | |
| MMIO / DMA / 中断 | 无 | 本 crate 不接触硬件 |

## unsafe 代码清单

无。本 crate 没有 `unsafe`，全部是 `String` / `Vec` / 锁。

## 内存安全不变量

内存安全上没有非平凡不变量（无 unsafe、无裸指针）。**正确性**上的不变量才是
本 crate 的承重结构：

1. **一个 index 同时只属于一个节点**。`IndexPools::alloc` 是 min-free，
   `release` 只在 `withdraw` 的第 5 步（`Removed` 派发之后）发生。因此不存在
   "两个设备同一个 devname / 同一个 `dev_t`"的窗口。耗尽返回
   `PublishError::ResourceBusy`，**绝不静默别名**。
2. **`Added` 恰好派发一次，且总在 `Removing` 之前**。由 `DISPATCH_ORDER` +
   锁下重查 `RegistryEntry::added_dispatched` 保证；`withdraw` 发现 `Added`
   未派发时自己接管。
3. **派发永不在注册表锁内进行**。注册表锁是 `SpinNoPreempt`（禁抢占），
   订阅者会睡眠；禁抢占下睡眠可能把核卡死。
4. **`added_dispatched` 不出现在公开的 `DevNode` 上**，因此不会泄漏进订阅者
   的节点比较。
5. **`DEVPATH` 与 sysfs 目录树同源**。规范路径由发布者填在 `DevNode.sysfs_path`
   上，`uevent_devpath` 只做 `format!("/devices/{}", ...)`。历史上这里有两套
   推导（节点一套、uevent 一套），对 `input/inputN/eventN` 这类嵌套布局会分叉。
6. **`DeviceNumber` 的位布局是兼容面**，与 Linux `new_encode_dev` 一致，
   与 block core / devfs 一致。改它同时破坏 `st_rdev` 与 `MAJOR`/`MINOR`。
7. **`bitmap_value` 的字序是兼容面**：小端字节按 native 64-bit word 分组，
   最高非零 word 在前，空格分隔，全零渲染成 `0`。input 的 capability bitmap
   依赖这个顺序被 libevdev 正确解析。
8. **注册表 key 是类型化的 `PublicationId`**：`publish` / `publish_auto_indexed`
   / `withdraw` / `DevNode::id()` 只接受发布者持有的 class 句柄，发布路径没有
   裸 `u64` 入口，任意整数无法伪装成某个节点的 key。`PublicationId::from_raw`
   只服务于回放已存在的句柄（测试 fixture），不是发布者 API；驱动的句柄来自
   class hook 收到的 `PublicationNode.id`。

## 线程安全

- `REGISTRY` / `INDEX_POOLS`：`SpinNoPreempt`。**不可在硬中断上下文调用**
  `publish*` / `withdraw` / `subscribe`。
- `DISPATCH_ORDER`：`ksync::Mutex<()>`，可睡眠。
- 锁序固定为 `DISPATCH_ORDER` → `REGISTRY`/`INDEX_POOLS`，反向不发生。
- `DevNode` 是 `Clone` 的纯数据；订阅者拿到的是快照，不与注册表共享可变状态。
- `DevNodeCallback = Arc<dyn Fn(DevNodeEvent) + Send + Sync>`：回调可跨 CPU 执行。
- **重入即自死锁**：订阅者回调里调 `publish*`/`withdraw` 会在
  `DISPATCH_ORDER` 上死锁（非重入 Mutex）。这条约束靠文档 + review，类型上
  没有 barrier。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|---|---|---|---|---|
| T-01 | 用户态通过 sysfs `uevent` 文件写入注入伪造 action（例如 `"add\0SUBSYSTEM=block"`），让 udev 执行错误的规则 | 高 | 有 sysfs 写权限的进程 | **已实现**：`UeventAction::try_from` 是**精确字符串白名单**（8 个 Linux kobject action），任何多余字节即 `Err`。回归测试 `actions_accept_only_the_linux_kobject_set` 钉住 `"change"` 通过、`"change\0remove"` 被拒。payload 的其余字段全部来自注册表里的 `DevNode`，不受该写入影响 |
| T-02 | `DevNode.devname` / `properties` 里含 NUL 或 `=`，`uevent_payload` 拼出的 `KEY=VALUE\0` 序列被撕裂，udev 解析出错误的键值对（例如伪造 `DEVNAME`） | 高 | 某个发布者把**设备可控字符串**（virtio 设备名、input 的 `id->name`、DT `clock-output-names`）直接当 devname 或 property | **未实现**。本 crate 对嵌入 payload 的字符串**不做任何 NUL / `=` / 控制字符过滤**（`uevent.rs` 只 `push('\0')` 作分隔符）。今天不可达：block 的 devname 是 `vd{index}`（index 派生）、input 的是 `event{index}`、`subsystem` 是 `&'static str`、modalias 来自 const id 表。**但 `Gendisk::new` 只拒绝空名字，不拒绝 NUL/`=`**，所以这条不变量是"当前所有发布者恰好可信"，不是结构保证。**加固建议**：在 `publish*` 入口校验 devname 与每个 property 的 key/value 不含 `\0`、`=`、`\n`，违规返回 `PublishError`；或至少在 `uevent_payload` 里跳过含分隔符的键值对。Linux 的对应保护是 `kobject_uevent_env` 里的长度与格式检查 |
| T-03 | index 池耗尽后发布者拿到 `ResourceBusy`，但把它当成功处理，导致 `/dev` 缺节点而 class 注册表有设备 | 中 | `index_count` 设得太小（例如 input 的 32 个 event minor 用尽） | **已实现**：`publish_auto_indexed` 返回 `Result`，`PublishError::ResourceBusy` 与 `AlreadyExists` 可区分。**残余风险**：本 crate 无法强制发布者检查返回值 |
| T-04 | 并发 `withdraw` 与 `publish` 让新节点拿到刚释放的 index，订阅者看到"同一 devname 先 add 后 remove"颠倒 | 中 | 热插拔 | **已实现**：`release` 在 `Removed` 派发**之后**（`withdraw` 第 5 步），因此 Removing/Removed 期间该 index 仍被占用 |
| T-05 | 订阅者在回调里重入 devnode → `DISPATCH_ORDER` 自死锁 | 中 | 新增订阅者实现错误 | **部分实现**：契约写在 `DISPATCH_ORDER` 的文档注释里；当前三个订阅者（devfs/sysfs/uevent）已核实不重入。**残余风险**：类型上无 barrier，新订阅者可能违反。表现为 softlockup 而非内存破坏。加固建议：加一个"正在派发"标志，重入时 panic，把死锁变成可诊断失败 |
| T-06 | `install_lifecycle_publisher` 迟到安装，启动早期的 add 事件永久丢失 | 中 | 初始化顺序变化 | **已实现**：迟到安装返回 `false`，`uevent-netlink::init()` 因此 `assert!` panic——启动不会静默漏事件 |
| T-07 | `physical_path` 与 sysfs 实际创建的目录不一致，`DEVPATH` 指向不存在的路径 | 中 | class 发布者填错 `sysfs_path`，或嵌套布局（`input/inputN/eventN`）推导分叉 | **已实现**：不变量 5——规范路径**由发布者填在节点上**，`uevent_devpath` 不再从 `physical_path + devname` 推导（那正是产生分叉的旧行为） |
| T-08 | `nodes()` clone 整个节点列表，节点数很大时分配放大 | 低 | 大量设备 | **未实现限制**。`nodes: Vec<RegistryEntry>` 线性扫描 + 全量 clone。当前调用者是 sysfs/devfs 重建路径，不是热路径 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|---|---|---|---|---|---|---|
| F-01 | `publish_auto_indexed` 返回 `AlreadyExists` | 同一 id 重复发布 | 该节点不注册 | 发布者的 class 注册通常也会失败（`device-class` 有同样的查重），设备不被认领 | 3 | 查重**在注册之前**运行——否则会查到自己并把刚分配的 index 原样还回去 |
| F-02 | index 池耗尽（`ResourceBusy`） | `index_count` 用尽 | 该设备无 `/dev` 节点 | class 发布失败 → probe 失败 → 设备 `Unclaimed`；已有设备不受影响 | 3 | min-free 复用最低空洞，所以只有真的占满才会耗尽 |
| F-03 | `IndexPools::alloc` 里 `last_mut().unwrap()` | push 之后立即取 | 不可能失败 | — | 4 | 带 `CG-021` 例外说明的 `expect`；push 保证池存在 |
| F-04 | 订阅者抛错/失败 | devfs 创建节点失败、sysfs 重建树失败、netlink 发送失败 | 该投影层缺这个节点 | **本 crate 不感知**：`DevNodeCallback` 返回 `()`，没有错误通道。三个投影可能不一致（例如 `/dev/vda` 存在但 uevent 没发出去） | **2** | **未实现**。这是真实的可观测缺口：投影失败静默。加固建议：把 `DevNodeCallback` 改成返回 `Result`，或让投影层自己记日志并在 sysfs 暴露一致性计数 |
| F-05 | `withdraw` 一个不存在的 id | 同一 handle 被撤销两次（驱动 guard + 安全网 drain） | 无操作 | 无 | 4 | 锁下查找失败即返回，不 panic |
| F-06 | `DISPATCH_ORDER` 死锁 | 订阅者重入（T-05） | 该 CPU 卡死 | softlockup | 2 | 契约 + review；建议加 panic 断言 |
| F-07 | `lifecycle_publisher` 未安装就发布节点 | `uevent-netlink::init()` 未被调用 | 节点在 devfs/sysfs 正常，但**没有 uevent** | udev 不会为该设备跑规则（不会创建符号链接、不会加载模块） | 2 | `install_lifecycle_publisher` 返回 bool 且迟到安装失败，`init()` 里 `assert!`——把"没装"变成启动期 panic 而不是静默降级 |

## 故障管理

- 只有两个函数返回 `Result`：`publish_auto_indexed` / `publish`
  （`PublishError::{ResourceBusy, AlreadyExists}`）与
  `install_lifecycle_publisher`（`bool`）。
- `withdraw` 返回 `()`：对一个不存在的 id 撤销是幂等的无操作。这是刻意的——
  移除路径可能被走两次（驱动的 `Publication` guard + 安全网 drain），
  第二次必须安静成功。
- **本 crate 不 panic**（除 `IndexPools::alloc` 里那个带 `CG-021` 说明的
  `expect`，它论证为不可达）。
- 订阅者失败无通道（F-04）——这是本 crate 最主要的可靠性缺口。

## 隐私分析

无用户数据。但要注意 **uevent 是广播**：`KOBJECT_UEVENT` netlink 组上的任何
监听者都会收到 `DEVPATH`/`SUBSYSTEM`/`MAJOR`/`MINOR`/`DEVNAME`/`MODALIAS` 与
class 携带的 properties。input 设备的 properties 包含 identity（物理位置、
unique id）——这些是硬件描述，与 Linux 的暴露面等价，但**新增 property 时应
确认它不含用户可识别信息**，因为一旦发出就是 udev 可见的兼容面。

## 已知限制

1. **不对嵌入 payload 的字符串做分隔符过滤**（T-02）。今天所有发布者可信，
   但这是约定不是结构。
2. **订阅者失败静默**（F-04）。三个投影层可能不一致而无从察觉。
3. **重入保护靠文档**（T-05 / F-06）。
4. **`nodes()` 全量 clone + 线性扫描**（T-08）。`subscribers` 无注销 API，
   与内核同生命周期。
5. **`INDEX_POOLS` 的 `Vec<(&'static str, Vec<u32>)>` 是线性查找**，subsystem
   数量个位数时可接受。
6. **`physical_path_pci` 是总线词汇**（PCI `segment:bus:device.function` 命名），
   住在节点模型里而不是 bus 层。保留理由是 `DEVPATH` 与 sysfs 目录必须同源，
   否则会漂移；代价是本 crate 并非完全总线无关。这是记录在案的单点例外，
   不是新增总线命名的先例（见 `design.md` 的 `physical_path` 一节）。
7. **`DevNodeType` 的两个 `fn` 指针返回 `String`**，每次投影都分配。
   devname/sysfs_path 在发布时算一次并存在 `DevNode` 上，所以不在热路径。

## 审计清单

- [ ] 新增的 property key/value 是否可能含 `\0`、`=`、`\n`？（T-02，当前无过滤）
- [ ] 新增的 devname 来源是否**只**由 index 派生？如果引入了设备/固件可控的
      字符串，必须先加 T-02 的过滤。
- [ ] 新增的投影层订阅者是否会重入 `publish*`/`withdraw`？（F-06）
- [ ] 新增的派发点是否先取 `DISPATCH_ORDER`、并在锁下重查 `added_dispatched`？
      绕过它会破坏不变量 2。
- [ ] 有没有在持有 `REGISTRY` 锁时调用订阅者？（不变量 3）
- [ ] 新增的 `DevNodeType` 是否给了正确的 `index_count`？耗尽表现为
      `ResourceBusy`（F-02），不是别名。
- [ ] 改 `DeviceNumber` 的位布局或 `bitmap_value` 的字序了吗？两者都是
      **用户态兼容面**（不变量 6、7），改动会破坏 `st_rdev` 与 libevdev。
- [ ] 改 `uevent_env` 的属性顺序了吗？`ACTION`/`DEVPATH`/`SUBSYSTEM` 在前、
      然后 `MAJOR`/`MINOR`/`DEVNAME`/`MODALIAS`、然后 class extras——顺序是
      udev 可见的。
- [ ] `withdraw` 是否仍然在 `Removed` 派发**之后**才归还 index？（不变量 1、T-04）
