# 当前驱动模型缺失的 P0 语义

> 基线：`origin/main` @ `877f59898bca9d22a2ff8d9a9bbc2ad7b44d350c`（2026-11-16）
> 对照分支：`xiaohui/pci-usb-verify` @ `ccc2afafb`（2026-11-07）
> 共同祖先：`a2e733b04`（2026-10-20）
>
> 范围：只列 P0 语义的现状与事实，不含实现方案。
> 判定标准：**不改就无法正确实现多实例、热插拔或 provider 依赖**；不是"写起来更方便"。
> 这四项是通用框架缺口，不是 USB/MMC/CLK 专属。

---

## 0. 结论

截至 `877f59898`（2026-11-16），四项 P0 中**第一项已由主线补上**，其余三项仍缺：

| P0 | 最新主线 | 结论 |
|---|---|---|
| 每设备驱动私有数据 | **!1398（`7f0ab2071`）已落地**：`DeviceDriver` 增加 `type Data`，probe 返回私有状态，remove/PM 回调拿到它；`BindingRuntime` 有数据槽 | **已补上** |
| 延迟探测 / provider 唤醒 | `ProbeOutcome` 仍无 `Deferred`；provider probe 成功不触发消费者重试 | **仍缺** |
| 类发布身份 | `ClassDevice::id()` 仍是 parent id；registry 仍按 parent id 去重，无 `PublicationId`。!1365 的两阶段退休修了 rebind 竞态 | **多发布 / 按发布撤销仍缺** |
| 子设备移除级联 | 仍只清 parent 指针（`clear_registration_topology`），子设备不移除、不 quiesce | **仍缺** |

---

## 1. P0 总表

| P0 | main 最新事实 | 现有 workaround | 后果 |
|---|---|---|---|
| per-binding 驱动数据 | **已补**（见 §2） | —（原 `ACTIVATIONS` 全局表已删除） | — |
| deferred / wake | `Unclaimed` 结束本次尝试，descriptor 回 `Pending`；provider probe 成功不唤醒等待者 | 依赖 DT 顺序；手动 `rescan`；provider 侧自建重试 | consumer 先于 provider 就失败且不重试；provider 链（usbdp→usb2phy）无法表达 |
| class 发布身份 | key = parent `DeviceId`；移除按 parent id；同 id 退休期再发布返回 `ResourceBusy` | 一个 parent 一个发布；或移除整个 controller | 同一 controller 第二个盘 `AlreadyExists`；拔一个盘只能拆整个 controller |
| child 级联 | `clear_registration_topology` 只清 parent 指针、把 child 从 parent 的 children 集里摘掉 | 驱动自己 remove 子设备；或不热拔 | 子设备仍 Active/bound/published；remove/quiesce 不运行；IRQ/DMA/clock 仍活 |

---

## 2. 一、每设备驱动私有数据：**已补上**（!1398）

### 2.1 落地内容（`7f0ab2071`，2026-11-16）

`DeviceDriver`（`drivers/base/kdevice/src/driver/mod.rs:284`）现在的形状：

```rust
pub trait DeviceDriver: Send + Sync {
    /// One binding's private state. `Send` required, `Sync` deliberately not.
    type Data: Send + 'static;

    fn probe_device(&self, ctx: &ProbeContext<'_>) -> DriverResult<Self::Data>;   // :302
    fn remove(&self, ctx: &RemoveContext<'_>, data: Option<&mut Self::Data>)
        -> DriverResult<()> { Ok(()) }                                            // :310
    fn suspend(&self, ctx: &PmContext<'_>, data: Option<&mut Self::Data>) { .. }  // :315
    fn resume(&self, ctx: &PmContext<'_>, data: Option<&mut Self::Data>) { .. }   // :320
    fn shutdown(&self, ctx: &PmContext<'_>, data: Option<&mut Self::Data>) {}     // :325
}
```

配套事实：

- **类型擦除存储在框架侧**：`DriverData { value: Box<dyn Any + Send> }`
  （`drivers/base/kdevice/src/driver/data.rs:14`）。文档明确：
  "Only the framework adapter can create it or downcast it; drivers only see
  their associated `DeviceDriver::Data`"——驱动看不到 `Any`，downcast 只发生在
  框架的 erased 适配层（`probe_erased`/`remove_erased`，`driver/mod.rs:351+`）。
- **数据槽挂在 per-binding runtime 上**：`BindingRuntime` 新增
  `source: BindingSource` 和 `data: SpinNoPreempt<DataSlot>`
  （`binding.rs:268-270`）；`install_data` / `take_data` / `restore_data`
  （`binding.rs:555/583/609`）带失败分类
  （`Occupied` / `WrongSource` / `RuntimeMismatch`）。
- **probe 失败回滚**：probe 返回 `Err` 走 `retire_failed_binding`；
  install 失败走 `recover_failed_data_install`（`lifecycle/mod.rs:500-552`）。
- **remove 全流程有 data**：`remove_bound_device_locked` 先
  `remove_ctx.take_data()`，把 `&mut Data` 传给驱动，最后 `drop(data)`、
  `run_cleanups()`（`lifecycle/mod.rs:1133+`）。
- **受控上下文**：新增 `ProbeContext` / `RemoveContext` / `PmContext`
  （`driver/context.rs`），提供 `device()` / `binding()` / `add_cleanup()`，
  驱动不再直接拿裸 `Arc<DeviceObject>` 做生命周期操作。
- **adoption 绑定合法地没有 Data**：`BindingSource::{Probe, Adoption, Test}`
  （`binding.rs:178`）；`remove` 收到 `Option<&mut Data>`，adoption 源为 `None`。
- **旧 workaround 已删**：virtio-blk 的
  `static ACTIVATIONS: Mutex<BTreeMap<DeviceId, Box<dyn FnOnce() + Send>>>`
  全树无命中；virtio 迁移为 `type Data = VirtioBindingData`（含
  `Block(BlockActivation)` 变体），ramdisk 用 `type Data = ()`。
- **PM 回调存在但未接线**：`suspend`/`resume` 在非 unittest 构建里是
  dead-code（"PM scheduling is not wired yet"）。

### 2.2 与本文原判定的关系

原判定是"框架没有 per-binding 私有数据槽位"。!1398 之后这个判定**不再成立**：
`type Data` + `BindingRuntime` 数据槽 + erased 适配层，就是"typed adapter +
类型擦除存储"的形态——核心只见 `Any`，适配器/驱动见 `D`。

仍值得注意的两个边界事实（不是缺口）：

- `Data: Send` 而刻意不要求 `Sync`：中断/工作队列共享状态要走内部可变性，
  框架不替驱动解决。
- adoption/test 源的 binding 没有 Data，`remove` 的 `data` 是 `None`：
  驱动必须处理 `None` 分支。

---

## 3. 二、延迟探测与 provider 唤醒：仍缺

### 3.1 main 最新事实

`ProbeOutcome` 仍是五个变体，没有 `Deferred`
（`drivers/base/kdevice/src/lifecycle/mod.rs:63`）：

```text
Activated / Skipped / Busy / Unmatched / Unclaimed
```

probe 失败：记录失败、`retire_failed_binding`、尝试下一个候选；一轮结束
`requeue_desc`，返回 `Unclaimed` / `Unmatched`。失败原因没有被记录成
"等待某个 provider"。

`register_driver_object` 仍会重扫 `descriptors_awaiting_binding`（后来注册的
驱动可补绑），但 **provider 设备 probe 成功不触发任何消费者重试**。

在 `drivers/base/kdevice/src` 下搜索：

```text
git grep -n "Deferred\|wake_deferred\|DepSource" origin/main -- drivers/base/kdevice/src
```

无命中（仅 `device_node.rs` 的节点退休延迟，与 probe defer 无关）。
`DriverError`（`drivers/contracts/driver_base/src/lib.rs:139`）仍只有
`WouldBlock` / `ResourceBusy` 两个"可重试"提示，域没有 park/wake。

### 3.2 为什么必须

- **依赖图不是一条线**：RK3588 上 `CRU(clk/reset)` → `PHY` → `PCIe/USB/MMC`；
  `usbdp` 依赖 `usb2phy` 发布的 480 MHz clock；`PCIe` 同时依赖
  clk + reset + phy + PMU。
- **枚举顺序不是依赖顺序**：consumer 在 provider 之前会失败一次，同一 pass
  不会回头重试。
- **provider 自己也可能晚到**：provider 链上任何一环晚到，下游都必须能重新
  probe。
- **运行期重新 probe**：控制器 unbind/rebind 时 provider 可能尚未就绪。
- **手动 rescan 的代价**：O(N) 重试、重复执行失败的 probe，且无法区分
  "provider 未就绪"和"没有匹配驱动"。

### 3.3 结论

**仍缺**：没有 `Deferred` 结果、没有 provider 发布后的域级唤醒、没有等待者
队列。drvdata 落地后，这一项成为"驱动带依赖 probe"的唯一阻塞语义。

---

## 4. 三、类发布身份（多发布 / 按发布撤销）：仍缺

### 4.1 main 最新事实

`ClassDevice::id()` 仍是 parent 的 `DeviceId`
（`drivers/base/kclass/src/generic.rs:167`）：

```rust
pub fn id(&self) -> DeviceId {
    self.inner.parent.id()
}
```

registry 仍按 `DeviceId` 去重（`generic.rs:382-384`，重复即
`AlreadyExists`）；移除仍按 `DeviceId`（`Removed` 事件路径）。在 `drivers/`
下搜索 `PublicationId` 无命中。

!1365 引入的 **两阶段退休**（`is_retiring`，`generic.rs:366,377-381`）解决的是
**rebind 竞态**：同一 id 的旧发布在退休完成前，新发布返回 `ResourceBusy`
（"a replacement device created by a fast rebind cannot become visible before
A's unpublish returns"）。这是同一 parent 同一 id 的**替换**语义，不是
**多个并发发布**语义。

`ClassDevice` 仍捕获 `BindingRef`（历史 driver 身份、binding 级
availability），没有 per-publication 的存活状态；`ClassDevice` 没有 `Drop`
撤销，registry 自己持有 clone。

### 4.2 为什么必须

- **USB 多盘**：一个控制器（一个 `DeviceObject`）下挂两个 U 盘，两个盘都
  发布在控制器 parent 上时，第二个 `AlreadyExists`。
- **拔一个盘**：无法只撤销该盘的发布；移除按 parent id。
- **MMC 重新插拔**：卡发布在 host parent 上时，同一张卡重新插拔落在同一个
  parent id；两阶段退休只保证"替换不竞态"，不提供"旧发布先撤、新发布后上"
  之外的并发多发布。
- **per-publication liveness**：`is_available()` 观察 binding，控制器活着时
  已拔掉的盘的发布不会自己变不可用。

### 4.3 结论

**多发布 / 按发布撤销仍缺**；rebind 替换竞态已由两阶段退休修掉。

---

## 5. 四、子设备移除级联：仍缺

### 5.1 main 最新事实

移除路径已重构（`remove_device_managed` → `remove_bound_device_locked`
→ `remove_device_from_index` → `clear_registration_topology`），但语义未变：

```text
lifecycle/mod.rs:1060  remove_bound_device_locked   驱动 remove + take_data + run_cleanups
lifecycle/mod.rs:852   remove_device_from_index      快照 parent/children
topology/relations.rs:402  clear_registration_topology
```

`clear_registration_topology`（`topology/relations.rs:445-478`）对每个 child：

```text
relations[child_index].parent = None;      // 清 child 的 parent 指针
relations[parent_index].children 移除该 id  // 从 parent 的 children 集摘掉
```

注释明确："The child's new parent edge is untouched"——**子设备不被移除、不
quiesce、发布不撤销**。`!1383`（稳定设备对象与可更换绑定）改变了 unbind 语义
（Requeue 时稳定对象存活、只退休 binding），但 unregister 路径对 children
仍是孤儿化。

### 5.2 为什么必须

- **子设备的 remove/quiesce 不会运行**：IRQ、DMA、clock 仍可能访问已掉线的
  硬件——比 `/dev` 悬挂更严重。
- **子设备仍 Active/bound/published**：class registry 的发布不撤销，用户态
  视角指向失效硬件。
- **适用对象**：以子 `DeviceObject` 建模的设备（PCIe 下游、MMC 卡、子设备形式
  的 hub child）。USB 走私有表时级联由表自己负责，但 kdevice 层的级联仍缺。

### 5.3 结论

**仍缺**：实现从 `set_parent(None)` 重构为 `clear_registration_topology`，
孤儿化语义不变。

---

## 6. 最新主线相关变化（`84208ed39..877f59898`，41 个提交）

| 提交 | 做了什么 | 没有做什么 |
|---|---|---|
| `7f0ab2071` !1398 | **typed driver data**：`type Data`、`Probe/Remove/PmContext`、`DriverData` 擦除存储、`BindingRuntime` 数据槽、probe 失败退休；删除 `ACTIVATIONS` | 不涉及 defer / class 多发布 / child 级联 |
| `efbc7698a` !1383 | 稳定设备对象与可更换绑定：unbind 只退休 binding、对象与拓扑存活；stale remove 用 `BindingRef::same_binding` 拒绝 | unregister 对 children 仍孤儿化 |
| （更早）`8f866f915` !1359 | per-binding `BindingRuntime` 容器 | 当时无数据槽（本周期由 !1398 补上） |
| （更早）`c33f593d9` !1365 | class handle 捕获 `BindingRef` 历史身份 | 无 `PublicationId`；本周期补了同 id 两阶段退休 |

其余为 memcg、DRM、knet、build 警告清理等，不影响本文结论。

---

## 7. 不在本文 P0 范围

- CLK / RESET / PHY 框架（新增子系统）；
- PCIe DW RC / 非 ECAM；
- USB host stack；
- MMC core / DW MMC；
- 开放 bus identity / location（一等 MMC/USB 总线时才需要）；
- class 机械可扩展（`DeviceKind`/`kclass` 宏）；
- RTC class 迁移。

---

## 8. 验证方法

```bash
# 最新 main
git log -1 --format='%H %ci %s' origin/main

# 一、drvdata（已补）
git show origin/main:drivers/base/kdevice/src/driver/mod.rs | grep -n \
  'pub trait DeviceDriver\|type Data\|fn probe_device\|fn remove\|fn suspend\|fn resume\|fn shutdown'
git show origin/main:drivers/base/kdevice/src/driver/data.rs | grep -n 'struct DriverData'
git show origin/main:drivers/base/kdevice/src/device/binding.rs | grep -n \
  'struct BindingRuntime\|source:\|data:\|fn install_data\|fn take_data\|fn restore_data'
git grep -n 'ACTIVATIONS' origin/main -- drivers   # 无命中 = 全局表已删

# 二、deferred / provider wake（仍缺）
git show origin/main:drivers/base/kdevice/src/lifecycle/mod.rs | grep -n 'pub enum ProbeOutcome'
git grep -n 'Deferred\|wake_deferred\|DepSource' origin/main -- drivers/base/kdevice/src

# 三、class 发布身份（仍缺）
git show origin/main:drivers/base/kclass/src/generic.rs | grep -n \
  'pub fn id(&self)\|fn publish(&mut self\|is_retiring'
git grep -n 'PublicationId' origin/main -- drivers

# 四、child 级联（仍缺）
git show origin/main:drivers/base/kdevice/src/topology/relations.rs | sed -n '440,478p'
```

---

## 9. 一句话总结

**四项 P0 中，drvdata 已在 !1398（2026-11-16）补上**——`type Data` +
`Probe/Remove/PmContext` + 框架侧类型擦除存储，`ACTIVATIONS` 全局表已删；
**deferred/provider 唤醒、class 多发布与按发布撤销、child 级联三项仍缺**。
原 P0 清单的优先级相应更新：deferred wake 成为下一个最阻塞项。