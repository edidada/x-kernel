# `bus/usb` 设计

本文件描述 USB 总线核心（host-free 的设备模型层）的职责、契约与不变量。
**第二部分**是当前工作的设计：设备生命周期与类驱动绑定层（代号 **G**），
它同时是 xHCI/USB3 现场问题的收尾设计。

---

## 第一部分：这一层是什么

`bus/usb` 是 USB 的**设备模型与协议核心**，不接触任何控制器寄存器：

- 传输抽象：`Hcd`（控制器契约）—— `port_count`/`port_status`/`port_reset`/
  `control_transfer`/`bulk_transfer`/`clear_endpoint_halt`/`set_ep0_max_packet`。
- 设备模型：`UsbDevice`（描述符解析结果）、`Speed`、`SetupPacket`、各描述符类型。
- 协议实现：`enumerate`（8 字节首读 → 地址 → 描述符 → 配置）、`hub`（hub 类请求：
  descriptor、port status/reset/disable、HUB_SET_DEPTH）、`bot`（Bulk-Only Transport）、
  `bot_fs`（根目录列举，仅诊断）。

**不在这里的东西（刻意）**：任何控制器寄存器或 DMA（在 `drv/usb`）、任何类/子系统类型
（`subsys_block` 等，在各自的 `subsystems/*`）。这一层只提供"设备存在且可寻址"的事实，
由 `drv/usb` 的控制器驱动与 `driver-init` 的组合根去绑定具体类驱动。

### 不变量

1. 本 crate **不依赖** `subsys_block`/`subsys_input`/… ，也不依赖 `drivers/init`。
2. 所有控制器差异通过 `Hcd` 表达；本层不做"如果是 xHCI 就…"的分支。
3. 一个 `UsbDevice` 的 `address` 在该设备被重新枚举前不变。

---

## 第二部分：设备生命周期与类驱动绑定层（**G**）设计

### 1. 问题

当前实现里，"设备"没有所有者：

- 枚举一次发生在**控制器的 probe 内部**（`drv/usb/src/xhci/mod.rs` 的 `enumerate()`、
  `dwc3/glue.rs` 的 probe；EHCI 同理），结果 `UsbDevice` 只作为返回值存在；
- 类驱动的绑定与发布（`publish_mass_storage()`）也在 probe 里做，返回的
  `Publication<BlockDeviceImpl>` 被塞进驱动数据（`Dwc3Data._disks` / `EhciData._disks`）；
- 于是**运行期**没有任何东西能：撤销一个已发布设备、或为一个重新插入的设备再发布。

后果已经出现在板上：设备停止应答后，挂载点仍指向一个永不返回的块设备；
设备被拔出/换口后，驱动再也不会发现它（xHCI 只在 probe 里枚举一次）。

### 2. 现状取证

| 事实 | 位置 |
| --- | --- |
| `Device::publish()` 只克隆 `Arc<DeviceCore>`，**不借用** probe 的 `Device` | `drivers/base/src/device.rs:215` |
| `Publication<T>`：`Clone` 是借用视图；**拥有者 drop 时执行 withdraw** | `drivers/base/src/class/mod.rs:104-125` |
| `Class::publish` 需要 `&'static Class<T,H>`；`BLOCK` 是 `static` | `drivers/base/src/class/class_api.rs:101`；`drivers/subsystems/block/src/class.rs:47` |
| 发布物当前挂在 probe 返回的驱动数据里 | `drivers/drv/usb/src/{xhci,dwc3/glue,ehci}.rs` |
| 没有设备表、没有 bind/unbind、没有 `PORT_STATUS_CHANGE` 消费者 | `grep` 全仓：0 命中 |

**结论**：`Publication` 本身已经具备"可撤销"的语义，缺的是
(a) 一个 `'static` 的发布入口（运行时用），(b) 一个**在锁外**持有并释放发布物的所有者。

### 3. 需求

| # | 需求 | 触发场景 |
| --- | --- | --- |
| R1 | 设备移除 → 撤销其全部发布物 | 拔盘、换口、`SetPortFeature(PORT_DISABLE)` |
| R2 | 设备复位 → 保留发布物，只重建端点/上下文 | 挂载中掉线（当前 USB3 症状） |
| R3 | 设备（重新）出现 → 枚举并（重新）发布 | 热插拔、复位后身份变化 |
| R4 | 一次失败（半绑定/发布失败）不得留下半状态 | 容量读失败、Configure Endpoint 失败 |
| R5 | 撤销不得在持有控制器锁时发生 | 块后端持有 `Arc<Mutex<Xhci>>` |
| R6 | 上述语义可在 QEMU 中单测（不需要真实设备） | 回归 |

非目标：runtime PM/autosuspend、USB3 LPM、isochronous/streams、多配置/BOS。

### 4. 约束

- `driver-base` **不得**命名任何设备族（不能出现 "usb"）：新增能力必须是泛型 `bus` 的。
- `bus/usb` **不得**依赖任何 `subsystems/*` 类 crate；绑定必须类型擦除。
- `drv/**` 不得新增 host 依赖（`make check_boundaries` 会失败）。
- 单 host 框架：不追求跨主机可移植，但契约仍要 host-free。

### 5. 方案比较

| 方案 | 做法 | 问题 |
| --- | --- | --- |
| **A** 发布物留在驱动数据 | 移除时用标志位让块后端 fast-fail | `/dev` 节点残留、挂载点仍可见；不满足 R1 |
| **B** 发布物移入控制器实例 | `Xhci` 内 `Vec<Publication<_>>` | `Publication → 块后端 → Arc<Mutex<Xhci>> → Publication` 成环，移除时**自锁**（违反 R5） |
| **C** 独立设备表 + 锁外释放（**采用**） | `Arc<Mutex<UsbBus>>` 拥有绑定与发布物；`detach()` **返回**被移除的所有者，由调用方在控制器锁外 drop | 需要 (i) `driver-base` 一个泛型句柄、(ii) 明确的锁序规则 |

采用 C。核心洞见：**"谁被撤销"和"谁执行撤销"必须分离** —— 表决定移除，
调用方在安全点执行 drop。

### 6. 设计

#### 6.1 组件

```
drivers/base           DeviceHandle（新增，泛型、'static、不含设备族词汇）
                       ↑ 提供 publish()
drivers/bus/usb        UsbBus（新增）：设备表 + 绑定表 + 类型擦除的发布物所有者
                       ClassBinding（新增）：一个设备上的一个类绑定
                       ↑ 由控制器驱动持有 Arc<Mutex<UsbBus>>
drivers/drv/usb        控制器 probe：建表、把 DeviceHandle 放进表
                       块后端：持有 Arc<Mutex<Xhci>>（不变）
drivers/init           组合根：把 Bus 表挂到驱动数据（生命周期 > 控制器）
```

#### 6.2 `driver-base` 增量（唯一改动，泛型）

```rust
/// A `'static` handle to a device's identity, obtained in probe.
///
/// Exists because a class binding may be created *after* probe returns (a device
/// that is re-enumerated), while `Device<'_, B>` is probe-scoped.
pub struct DeviceHandle {
    core: alloc::sync::Arc<DeviceCore>,
    // identity strings copied at acquisition, so no borrow outlives probe
    path: alloc::string::String,
    modalias: Option<alloc::string::String>,
}

impl DeviceHandle {
    pub fn publish<T, H>(&self, class: &'static Class<T, H>, runtime: T, name: impl Into<String>)
        -> KResult<Publication<T>>
    where T: Send + Sync + 'static, H: ClassHooks<T>;
}

impl<'a, B: Bus> Device<'a, B> {
    pub fn handle(&self) -> DeviceHandle;
}
```

`driver-base` 里不出现 "usb"，也不新增 trait；`publish` 语义与
`Device::publish` 完全一致（同一实现，`Device::publish` 改为转发）。

#### 6.3 `bus/usb` 增量（host-free，无类依赖）

```rust
/// One class binding on one device.
pub struct ClassBinding {
    /// Which class claimed it (`"usb-storage"`, …), for logs and for `unbind`.
    class_name: &'static str,
    /// Interface number the binding covers.
    interface: u8,
    /// The withdrawal owners, type-erased: dropping these unpublishes.
    /// `Box<dyn Any + Send + Sync>` keeps `bus/usb` free of `subsys_*`.
    owners: alloc::vec::Vec<alloc::boxed::Box<dyn core::any::Any + Send + Sync>>,
}

pub struct UsbBus {
    /// Identity the class bindings publish under.
    handle: driver_base::DeviceHandle,
    devices: alloc::vec::Vec<UsbDeviceEntry>,
}

struct UsbDeviceEntry {
    device: UsbDevice,           // address, port, speed, descriptors
    bindings: alloc::vec::Vec<ClassBinding>,
}

impl UsbBus {
    pub fn new(handle: driver_base::DeviceHandle) -> Self;
    /// Records a freshly enumerated device. Fails if the address is taken.
    pub fn attach(&mut self, device: UsbDevice) -> KResult<()>;
    /// Adds one class binding. On failure the caller's half state is unwound
    /// (R4): `bind()` is all-or-nothing.
    pub fn bind(&mut self, address: u8, binding: ClassBinding) -> KResult<()>;
    /// Removes a device and **returns** its owners: the caller drops them
    /// outside the controller lock (R5). Never drops them here.
    pub fn detach(&mut self, address: u8) -> alloc::vec::Vec<ClassBinding>;
    pub fn device(&self, address: u8) -> Option<&UsbDevice>;
    pub fn devices(&self) -> impl Iterator<Item = &UsbDevice>;
}
```

**为什么 `detach()` 返回所有者**：`Publication::drop` 会同步执行 withdraw，
而 withdraw 可能等待在途消费者；消费者（块后端）需要控制器锁。若在持锁时 drop，
就是方案 B 的自锁。返回后由调用方在锁外释放，锁序就是显然的。

#### 6.4 控制器侧（`drv/usb`）

新增两个方法，都不改变传输路径：

```rust
// xhci
pub fn rescan(&mut self) -> alloc::vec::Vec<ClassBinding>;   // 锁外释放
fn unbind_and_detach(&mut self, address: u8) -> alloc::vec::Vec<ClassBinding>;
pub fn bind_and_publish(&mut self, address: u8) -> KResult<()>;  // 用 DeviceHandle
```

- `rescan()` 消费 `port_change`（已在事件分派里记录）与 PORTSC：
  - `CCS == 0` 且有 slot → `detach`：`Disable Slot`、`bulk`/`ep0` 环随之释放，
    绑定从表里摘出并**返回**给调用方；
  - `CCS == 1` 且无 slot → `enumerate_one(port)`，然后 `bind_and_publish`；
  - `hc_error` → 控制器复位 + 全表 detach（对应 `xhci_hc_died()`）。
- `reset_and_reverify()`（已实现）保持现状：**不动绑定**，只重建设备与端点上下文（R2）。

#### 6.5 设备状态机

```
Absent ──attach──▶ Addressed ──configure──▶ Configured ──bind──▶ Bound
   ▲                                                              │
   └──────────── detach（回收所有绑定所有者）◀──── Removing ◀──────┘
                                    │
                        reset_and_reverify（Bound 内自环，R2）
```

| 迁移 | 所有者 | 失败处理 |
| --- | --- | --- |
| Absent → Addressed | 控制器（`Enable Slot` + `Address Device`） | `Disable Slot`，回 Absent |
| Addressed → Configured | 控制器（`Configure Endpoint`） | 同上 |
| Configured → Bound | 类驱动（`bind()` 内一步完成） | `bind()` 全有或全无，回 Configured |
| Bound → Removing | 控制器（设备消失/控制器错误） | — |
| Removing → Absent | **调用方**在锁外 drop 所有者（R5） | — |

#### 6.6 锁序（必须写进代码注释）

```
UsbBus 锁   <  控制器锁（Xhci/Ehci）
```

- 允许：持 `UsbBus` 锁时调用控制器（表中操作都是短的）；
- **禁止**：持控制器锁时 drop 任何 `ClassBinding`；
- 因此所有"移除"路径的返回类型都是 `Vec<ClassBinding>`，而不是 `()`。

#### 6.7 失败与回收矩阵

| 场景 | 结果 |
| --- | --- |
| 枚举中途失败 | 无绑定、无发布物；端口复位留下，下一轮 `rescan` 重试 |
| `bind()` 内发布失败 | 已取得的 `Publication` 在 `bind()` 返回前 drop（局部作用域），表里不留条目 |
| 设备在 `Bound` 时消失 | `Detached` → `/dev` 节点消失，挂载点读立即失败（不再挂死） |
| 控制器 HCE | 全表 `detach` + 控制器复位；设备层在下一轮 `rescan` 重新枚举 |
| 复位期间接到传输请求 | 后端返回 `Io`（`wire_dead`/`halt`），不做隐含重试 |

### 7. 验收

**QEMU 单测**（`drivers/bus/usb/ut-tests`，全 mock `Hcd`）：

1. `attach` 同地址两次 → 第二次失败，表不变；
2. `detach` 返回的 `Vec<ClassBinding>` drop 后，`devices()` 不再包含该地址；
3. `bind()` 失败路径不留下条目（用一个在 `publish` 里失败的假 Class）；
4. 状态机：`Attach → Bind → Detach → Attach` 后地址可复用。

**板上验收**：

- A. 挂载中的盘被拔出 → `/proc/mounts` 里的挂载点读返回 EIO 而非挂死；`/dev/usbN` 消失；
- B. 重新插入 → 自动重新枚举并发布（`/dev/usbN` 回来）；
- C. 现有回归：USB2 盘 `ls`/`cat`/写入读回；USB3 盘发布块设备（当前已达成）。

### 8. 交付状态

| 步 | 内容 | 状态 |
| --- | --- | --- |
| **P1** | `driver-base::DeviceHandle`（泛型、`'static`）+ `Device::publish` 转发 | ✅ |
| **P2** | `bus/usb::UsbBus` / `ClassBinding` / `drop_bindings()` | ✅ |
| **P3** | xHCI：表挂在控制器状态上；`rescan()` 消费 `port_change`/`hc_error` 并摘除消失设备 | ✅ |
| **P4** | xHCI：`bind_and_publish()` 用 `DeviceHandle` 在 probe 之后发布；块后端入口先 `rescan()`，绑定的 dropdown 在锁外 | ✅ |
| **P5** | EHCI：同一张表；`detach_absent()` 在块后端入口摘除消失设备 | ✅ |
| **P6** | 组合根：表由 probe 创建并存入驱动数据（`_bus`），`docs/security.md` 补齐 | ✅ |

**P2 的 QEMU 单测尚未编写**（表的语义已按契约实现，但 §7 列的 4 个用例仍是待办），
这是本设计唯一未完成的验收项。

已知引用环（已记录在 `security.md` §3）：控制器 → `UsbBus` → `Publication` → 块后端 →
控制器。它不造成自锁（因为移除路径只在锁外 drop），但它让"驱动卸载"没有定义语义。

### 9. 明确不改

- 传输路径（控制/bulk 环、TRB 链、门铃、完成事件）—— 已与参考逐行对齐；
- `kirq`、`driver-base` 的类机制与 `Class` 契约；
- 中断缺失：仍是轮询；`port_change`/`hc_error` 由 `rescan()` 消费，不引入 IRQ。
