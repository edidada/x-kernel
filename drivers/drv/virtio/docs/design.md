# drv-virtio — 设计文档

## 定位

driver-core 模型下的 virtio **驱动模块** crate：一个模块一个设备家族，加一个
virtio-mmio transport 驱动。

| 模块 | feature | 总线 | 说明 |
|---|---|---|---|
| `blk` | `blk` | PCI + MMIO | **golden 模板**：PCI 消费者 + 类发布 + devnode + guards-in-Data |
| `net` | `net` | PCI + MMIO | 每设备中断所有权（旧模型这里是三个全局 static 做 IRQ→设备反查） |
| `rng` | `rng` | PCI + MMIO | 最小消费者 + 主机完成等待 |
| `gpu` | `gpu` | PCI + MMIO | display 类发布 |
| `input` | `input` | PCI + MMIO | input 类发布 + devnode 投影 |
| `vsock` | `vsock` | PCI + MMIO | vsock transport |
| `virtio_9p` | `fs9p` | PCI + MMIO | 9P transport |
| `mmio` | `mmio` | platform | virtio-mmio transport 驱动：认领 `virtio,mmio` 节点、读 transport header 定类型、按家族分派 |
| `common` | 无门控 | — | `map_driver_error` / `physical_path` / `pci_transport`（扩展服务入口） |

设备协议实现（`VirtIoBlkDev` 等）**不在这里**，在 `drv/virtio/devices`
（包 `virtio-devices`），它有自己的 `docs/`。本 crate 只做 driver-core 侧的
probe/remove/发布/资源所有权。

## 范围

`src/{lib,common,blk,net,rng,gpu,input,vsock,virtio_9p,mmio}.rs`，1389 行。

## 架构

```text
        driver-compose::pci_discover()          DT "virtio,mmio" 节点
                │                                       │
                ▼                                       ▼
        DeviceDescriptor{Pci}                   DeviceDescriptor{Platform}
                │                                       │
     ┌──────────┴───────────┐                           ▼
     ▼                      ▼                    MmioDriver::probe
 PciBus 驱动            (每个家族一个                │ dev.mmio(0) / dev.iomap(0)
 VirtioBlkDriver         DriverDesc +               │ probe_mmio_device(regs) → kind
 VirtioNetDriver …       const IDS 表)              │ ActivationBuilder::new(regs)
     │                      │                       └─ dispatch(kind) ──┐
     ▼                      ▼                                           │
  probe(dev):                                                           │
   1. common::pci_transport(dev)                                        │
        └─ dev.extension::<driver_host::virtio_transport               │
                             ::PciTransportService>()                   │
             └─ service.probe(descriptor)                               │
                  → (VirtioDeviceKind, virtio_devices::PciTransport, Option<virq>)
   2. 校验 kind 与本驱动一致（否则 Fatal(Unsupported)）                   │
   3. IRQ spec：transport 绑定的向量优先，否则 dev.irq(0) 的固件路由       │
   4. activate(dev, transport, spec, mmio_regs)  ◄───────────────────────┘
        ├─ VirtIoXxxDev::<driver_host::virtio_hal::Hal, T>::try_new(...)
        ├─ dev.request_irq(spec, name, closure)      // 闭包捕获 Weak（dev_id 模式）
        ├─ device_class::publish_xxx(ClassParent::detached(dev.id().raw(), name, loc), runtime)
        └─ Ok(Box<dyn Activation>)   // publication + irq + transport + 家族状态
   5. Ok(XxxData { activation: Some(..) })
```

## `Activation` 模式

每个家族的 probe 产出一个**类型擦除的 activation 对象**，它是"旧框架放在全局
side table 里的全部东西"：

```rust
pub trait Activation: Send + Sync {
    fn close(self: Box<Self>);
}

pub struct BlkData { activation: Option<Box<dyn Activation>> }
```

`close()` 的实现就是 `drop(self)`，而 `Drop for BlkActivation` 按 Linux 的
remove 顺序显式编排：

```text
begin_close(drain.signals())      // 拒绝新请求，登记在途 I/O
  → del_gendisk(device_number)    // 撤销块设备号
  → drain.wait_completion()       // 等在途 I/O 结束（可睡眠）
  → device.disable_interrupts()   // 设备侧停中断
  → drop(irq.take())              // 释放 IRQ guard
  → reclaimer.stop_and_wait()     // 停 softirq 回收器
  → device.finish_close()         // 销毁 transport
  → publication 撤销（字段 drop）
```

**为什么用 `Box<dyn Activation>` 而不是泛型字段**：`Data` 的类型必须与
transport 的具体类型（`PciTransport` vs `MmioTransport`）和 HAL 类型解耦，
否则 `BlkData<H, T>` 会把泛型参数传染到 `DriverOps::Data` 关联类型上，
而域持有的是 `Box<dyn Any + Send + Sync>`。类型擦除只发生在这一层，
`close()` 的顺序仍然是静态代码。

**双保险**：`BlkData::drop` 也调 `activation.take().close()`，而
`DriverOps::remove` 先调一次。`take()` 保证幂等——`remove` 跑过之后
`Drop` 看到 `None`。这让"probe 成功但从未 remove"（例如域被整体丢弃）
与"正常 remove"走同一条清理代码。

## virtio-mmio：映射跟随设备对象（Linux BAR 模型）

`ActivationBuilder` 持有寄存器映射，家族在构造设备对象前调 `take_regs()`
把映射**移进设备对象**：

```rust
let builder = ActivationBuilder::new(regs);   // mmio.rs 持有 IoMap
   ↓ dispatch
let mmio_regs = builder.take_regs();          // 家族取走
activate(dev, transport, irq, mmio_regs)?     // 映射进 VirtIoBlkDev
   ↓
builder.build(activation)                     // MmioData 只含 activation，不含映射
```

因此映射的生命周期 = 设备对象的生命周期 = **最后一个 `ClassDevice` clone 的
生命周期**，而不是驱动 `Data` 的生命周期。这正是 Linux 的 BAR 语义
（`pci_release_regions` 在最后一次 `put_device` 之后跑，不是在 driver unbind 时）。

`MmioData::drop` 的注释写明了后果：**撤销发布不可能让消费者 clone 拿着悬垂
指针**——因为映射比发布活得久。

旧框架在**枚举阶段**就 `iomap` 并读寄存器探测 transport 类型，映射泄漏成
HAL 级长期映射；现在探测发生在 `MmioDriver::probe` 里，映射由 builder 持有。

## 调用约束 / 执行上下文

- `probe` / `remove` 在域锁外的任务上下文运行，可睡眠。
  `BlkActivation::drop` 里的 `prepare_block_wait().expect(...)` 与
  `drain.wait_completion()` **依赖这个前提**（带 `CG-021` 例外说明）。
- **IRQ handler 闭包在硬中断上下文运行**，只做：`Weak::upgrade` → `ack_irq(0)`
  → 若 handled 则 `reclaimer.mark_pending()` → 返回 `IrqAction`。
  不取域锁、不分配、不睡眠。
- handler 捕获 **`Weak`**（不是 `Arc`）：activation 持有强引用并在 teardown 时
  先撤 IRQ，因此 `upgrade()` 返回 `None` 恰好意味着"设备正在消失"，
  handler 报 `NotHandled`。这是 Linux 的 `dev_id` 模式。
- `common::pci_transport` 触发 `driver-host` 的 transport probe，其中
  `begin_mmio_probe()` 是**单槽**的：它断言 PCI transport probe 串行。
  这个前提由 `driver-core` 的 probe 泵串行化提供，不是本 crate 的责任，
  但本 crate 是它的受益者。
- `mmio` 模块的 `dispatch` 在家族 feature 关闭时返回 `Fatal(Unsupported)`——
  transport 探测成功但没有对应驱动，这是"配置不完整"而不是"硬件不存在"，
  因此只 `log::debug` 不 `log::warn`。

## 设计决策

### 每个家族校验 the family kind vocabulary

```rust
let (kind, transport, irq) = crate::common::pci_transport(dev)?;
if !matches!(kind, VirtioDeviceKind::Block) {
    return Err(ProbeError::Fatal(RuntimeError::Unsupported));
}
```

transport probe 返回的 kind 来自 virtio device type，而 `IDS` 表匹配的是
PCI vendor/device id。**transitional 系列按类型索引**（`0x1000` = net、
`0x1001` = blk、`0x1005` = rng），一行写错就会静默 steal 另一个驱动的设备。
这个校验把"ID 表写错"从静默错误变成显式 `Fatal`。

### `IDS` 表同时含 modern 与 transitional

```rust
static IDS: &[PciMatch] = &[
    PciMatch::device(0x1af4, 0x1001),   // transitional
    PciMatch::device(0x1af4, 0x1042),   // modern (0x1040 + type)
];
```

`PciMatch::matches` 对精确 device id 给 `MatchScore::Exact`，对
`device_id == 0xFFFF` 的通配给 `Generic`。这里两行都是精确匹配。
`modalias()` 生成 `pci:v00001AF4d00001042`，经 devnode 进 uevent。

### 依赖 `driver_host` / `ktask` / `kpoll`（登记的例外 E-02、E-04）

- `driver_host::virtio_hal::Hal`：vendored `virtio-drivers` 要求
  `VirtIoHal: 'static` 作为**类型参数**，所以构造 `VirtIoBlkDev<H, T>` 必须
  命名一个具体 `H`。
- `driver_host::virtio_transport::PciTransportService`：`ExtensionService`
  用 `as_any` downcast，调用方必须命名具体类型——这正是 `driver-core` 不认识
  家族的代价。
- `ktask` / `kpoll`：`rng` 的主机完成等待（`HostRngCompletion`）。
  `subsystems/block::dispatch::wait` 已经用同一原语实现了服务化版本；
  hwrng 只有一个消费者，等第二个出现再抽服务。

三条都登记在 `docs/ai/driver-subsystem-boundaries.md` 的例外表里，带退出条件。

**`kirq` 不在列表里，也不在 `Cargo.toml` 里**：本 crate 与 `virtio-devices`
都不依赖它。中断注册全部走 `dev.request_irq(spec, name, closure)` +
`driver_core::IrqAction`，家族库只提供一个固有方法 `ack_irq(irq) -> bool`
（net 是 `VirtIoNetIrqWorker::service() -> bool`）。旧模型里设备对象直接
`impl device_res::IrqHandler` 并被注册进 provider，那个形状遗留下了三个
**从未被注册**的 `impl kirq::IrqHandler`，已删除。

### `remove()` 大多是空的

`blk` 的 `remove` 显式 `activation.close()`；`rng` 的 `remove` 是空的
（guard 在 `Data` drop 时释放）；`mmio` 的 `remove` 也是空的
（`MmioData::drop` 调 close）。

区别在于**是否需要一个与 drop 顺序不同的 quiesce 时机**。blk 需要：
`del_gendisk` 必须在 drain 之前、`disable_interrupts` 必须在 drain 之后，
这个交错无法只靠字段顺序表达（`BlkActivation` 的 `publication` 字段注释
写明了这一点）。

## Drop / 资源释放

三层，每层的所有权明确：

```text
XxxData { activation: Option<Box<dyn Activation>> }
   │  remove() 先 take().close()；Drop 再 take()（幂等）
   ▼
Box<dyn Activation> → XxxActivation { device, irq, reclaimer, disk, publication }
   │  Drop 按上面"Activation 模式"里的显式顺序 close
   ▼
guards: DevIrq（→ driver-host release_irq）、ClassWithdraw（→ class unpublish
        + devnode withdraw）、IoMap（mmio 变体：不在这里，在设备对象里）
```

**MMIO 变体的映射不在这条链上**——它跟着设备对象（`ActivationBuilder::take_regs`），
比发布活得久。这是刻意的，见"virtio-mmio"一节。

probe 失败时：`activate()` 里的 `activation` 是局部值，`?` 提前返回即 drop，
已经拿到的 IRQ guard / 发布会按字段逆序释放；`XxxData` 从未构造，所以没有
第二次释放。
