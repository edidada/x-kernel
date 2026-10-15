# driver-host — 设计文档

## 定位

`driver-core` 服务契约的 X-Kernel 实现，也是驱动框架里**唯一**允许直接触碰
`memspace` / `kirq` / `kdma` / `khal` / `kclk` 的地方（能力 → 框架方向）。

它承担四件事：

1. **资源 provider**（`provider/`）：把 `driver_core::provider` 的 `MmioOp` /
   `IrqOp` / `DmaOp` 绑到主机子系统；
2. **每设备服务面**（`XKernelRuntime`）：实现 `DeviceServices`，把 provider 的
   裸操作包装成驱动实际拿到的 RAII guard（`IoMap` / `DevIrq` / `CoherentMem` /
   `ClkHandle`）；
3. **时钟注册表**（`clk.rs`）：`kclk` 的 boot-static provider registry 在设备
   模型上的投影，负责 `NoProvider` → `ServiceError::Deferred` 的翻译与唤醒；
4. **家族扩展服务**（`virtio_hal.rs` / `virtio_transport.rs`）：vendored
   `virtio-drivers` 需要的 `VirtIoHal` 实现，以及 `PciTransportService`
   （`driver_core::ExtensionService`）。

依赖它的：`core/driver-compose`（安装）、`drv/virtio`、`drv/clk`（例外 E-02）。
它依赖 `driver-core`，反向不成立。

## 背景

旧框架把这一层拆成两个 crate：契约在 `contracts/device-res`（含自己的一套
`IrqHandler`/`IrqEvent`/`IrqTrigger` 词汇，490 行），实现在
`adapters/xkernel/device-res`（466 行纯翻译）。`refactor(device-res)` 六步系列
把两者折叠进一个 crate，并让 IRQ 词汇只在 `kirq` 里定义一次
（`driver-core/src/resource.rs` re-export）。

折叠的理由：只有两个消费者（`XKernelRuntime` + vendored HAL 表面），多一个
crate 只多一条依赖边，不多任何隔离。

## 范围

| 文件 | 职责 |
|---|---|
| `src/lib.rs` | `XKernelRuntime`（`DeviceResourceServices` / `ClockConsumerServices` / `DeviceServices` 实现）、`GLOBAL_RUNTIME`、`RESOURCE_PROVIDER` 静态、`AdaptedIrqHandler`、`probe_pci_transport_with` |
| `src/provider/mod.rs` | `XKernelResourceProvider`（无状态单元结构体） |
| `src/provider/mmio.rs` | `MmioOp` → `memspace::iomap_device` / `iounmap` |
| `src/provider/irq.rs` | `IrqOp` → `kirq::{try_register_shared, try_free_irq, try_free_irq_action, enable, try_map, alloc_msix, free_msix}`；词汇翻译 `to_runtime_controller` / `to_runtime_route` / `resolve_irq_spec` |
| `src/provider/dma.rs` | `DmaOp` → `kdma::{allocate_dma_memory, deallocate_dma_memory, map_dma_buffer, unmap_dma_buffer}` |
| `src/clk.rs` | `ClkRegistry` + `KclkHandle`（orphan-rule 桥接 `kclk::ClkHandle` → `driver_core::ClkOps`） |
| `src/virtio_hal.rs` | `unsafe impl VirtIoHal for Hal`：DMA alloc/dealloc、`mmio_phys_to_virt` + probe-scope lease 收集器、streaming share/unshare |
| `src/virtio_transport.rs` | `PciTransportService`：`ExtensionService` 实现，从 services 实例 downcast 回 `XKernelRuntime` |

## 架构

```text
        drv/**  ── DeviceCtx ──┐
                               ▼
   driver-core:  DeviceResourceServices + ClockConsumerServices
                        ┌──────┴───────┐
                        ▼              ▼
              XKernelRuntime      ClkRegistry ──► kclk (boot-static providers)
                   │  │  │
        ┌──────────┘  │  └──────────┐
        ▼             ▼             ▼
   provider/mmio  provider/irq   provider/dma
        │             │             │
        ▼             ▼             ▼
    memspace        kirq          kdma

   virtio_hal::Hal ── resource_provider() ──► 同一个 XKernelResourceProvider
        ▲
        └── vendored virtio-drivers 通过类型参数 `H: VirtIoHal` 调用
```

### 为什么只有一个 provider 实例

`XKernelRuntime.provider` 是 `&'static XKernelResourceProvider`，指向
`lib.rs` 的 `RESOURCE_PROVIDER` 静态——**和 HAL 通过 `resource_provider()`
拿到的是同一个**。

原因：vendored `VirtIoHal` trait 的方法是**关联函数**（没有 `&self` 接收者），
所以 HAL 无法携带一个 per-runtime 的 provider 实例。如果 runtime 另建一个
`Arc::new(provider)`，就会静默把 provider 世界分叉成两份——per-device 记账、
未来的 IOMMU domain 都会分裂。因此两边共享这一个静态。

`PciTransportService` 则相反：它从**自己所属的 services 实例** downcast 出
`XKernelRuntime`，再取 `static_provider()`，不用全局回退。这样带自己 runtime
的域会绑到自己的 provider（这是 review II-6 的修复）。

## 调用约束 / 执行上下文

- `XKernelRuntime::install_global()` 由 `driver-compose::init()` 调用**恰好一次**，
  在任何驱动注册之前。`register_clk_provider()`（自由函数版本）从
  `GLOBAL_RUNTIME` 取 runtime，若未安装则 **panic**——这是刻意的启动顺序断言，
  带 `CG-021` 例外说明。
- **`provider/*` 的 Op 方法运行在任务上下文**（probe / remove 路径），不是中断
  上下文。`MmioOp::map_mmio` 会分配地址空间，`IrqOp::request_irq` 会取 kirq 的
  描述符锁。
- **`virtio_hal::Hal` 的方法可以在中断上下文被调用**（vendored transport 在
  notify / ack 路径上调 `mmio_phys_to_virt` 的情况不存在，但 `share`/`unshare`
  可能在 DMA 提交路径上）。`FALLBACK_MMIO_LEASES` 与 `PROBE_MMIO_LEASES` 都是
  `SpinNoPreempt`，不关中断，因此**不得在硬中断里首次映射**。
- `begin_mmio_probe()` 是**单槽**的：第二个 scope 在第一个未关闭时打开会
  `assert!` panic。这不是防御性代码，是把"PCI transport probe 串行"这个契约
  变成可执行的断言——`driver-core` 的 probe 泵串行化（`probing` 标志）是它
  成立的前提。scope guard 的 `Drop` 关闭槽位，所以 open 与 take 之间 panic
  不会把槽位卡住（旧的 begin/take 对没有错误路径恢复）。
- `resolve_irq_spec` / `IrqOp::map_irq` 可在 probe 期间调用；它只查/建 kirq 的
  domain 映射，不注册 handler。

## 算法流程

### `DeviceResourceServices::request_irq`

```text
IrqSpec ─┬─ ResolvedVirq{number, trigger} ─► IrqResource::plain(number, trigger)
         └─ FirmwareRoute{controller, domain, hwirq, trigger}
                └► to_runtime_route(...) ─► IrqOp::map_irq ─► virq
                     （映射失败 → RuntimeError，不是 0 号占位）
                └► IrqResource::mapped(virq, trigger, controller, domain, hwirq)

handler: IrqHandlerFn (Fn() -> IrqAction)
   └► AdaptedIrqHandler 包成 kirq::IrqHandler
        （IrqAction::Handled → kirq::IrqEvent::HANDLED）
   └► kirq::try_register_shared(spec, adapted)
        ├─ Ok(Some(token)) → IrqHandlerToken::SharedAction(id)
        ├─ Ok(None)        → IrqHandlerToken::RegularAction
        └─ Err             → ResError（Busy / NoMemory / InvalidResource）

返回 DevIrq guard，其 ReleaseHook 按 token 形状释放：
   SharedAction(id) → kirq::try_free_irq_action(spec, token)
   RegularAction    → kirq::try_free_irq(spec)
```

**关键顺序**：固件路由必须在注册**之前**解析。provider 会经过 controller 的
domain 做映射（已有映射则复用），所以注册用的 resource 与返回的 `DevIrq`
携带的是**真实 virq**，永远不是占位 0。一个 `DomainMapped { number: 0 }` 会
交给驱动一个"号不是主机实际分发的那个"的 IRQ guard。

### `resolve_irq_spec`（原 `virtio_devices::resolve_legacy_irq`）

只要号、不注册 handler 的路径（`irq_create_of_mapping` 等价物）。
`ResolvedVirq` 直通；`FirmwareRoute` 经 `to_runtime_route` + `map_irq`。
放在本 crate 而不是家族库，是因为 controller 词汇翻译与 domain 回退是
provider 知识（规则 L1）。

### 时钟：deferred probe 的握手

```text
消费者 probe:  dev.clk("baudclk")
   └► ClkRegistry::get(ClkSpec{phandle, index})
        ├─ kclk 查到 → KclkHandle 包成 Arc<dyn ClkOps> → ClkHandle
        └─ NoProvider → ServiceError::Deferred(DepSource::clk(phandle))
                          └► 域 park 设备

provider 驱动 probe 末尾:  driver_host::register_clk_provider(&lookup)
   └► ClkRegistry::register_provider
        ├─ kclk::register_provider(phandle, lookup)
        └─ wake_hook(DepSource::clk(phandle))
             └► global_domain().wake(dep) → 精确重探被 park 的设备
```

没有轮询、没有重试表。`ClkRegistry::new(wake)` 的 wake 闭包由
`XKernelRuntime::install_global()` 接成 `global_domain().wake`。

### virtio HAL 的 MMIO lease 生命周期

vendored `VirtIoHal::mmio_phys_to_virt` 返回裸 `NonNull<u8>`，**trait 签名
无处安放映射令牌**。因此映射的所有权用"租约收集"补回来：

```text
Hal::mmio_phys_to_virt(paddr, size)
  ├─ provider.map_mmio(region, "virtio")            → MmioMapping
  ├─ unsafe { IoMap::new(vaddr, region, release) }  → lease（RAII，drop 即 unmap）
  ├─ mmio_probe_collector(|leases| leases.push(lease))
  │     ├─ 有开放 scope → lease 进 scope          （正常路径）
  │     └─ 无 scope     → lease 进 FALLBACK_MMIO_LEASES（永久存活，可见可计数）
  └─ 返回 NonNull（vaddr）

probe_pci_transport_with:
  let scope = virtio_hal::begin_mmio_probe();
  let probed = virtio_devices::probe_pci_transport_only::<Hal>(...);
  let leases = scope.take_leases();
  transport.retain_mappings(leases);   // 映射随 transport 存活，transport drop 才 unmap
```

这就是 Linux 的 BAR 模型：映射的生命周期跟着设备对象走，而不是在 trait 边界
上泄漏。probe 失败时 leases 就地 drop，映射随失败的 probe 一起消失。

## 并发模型

| 状态 | 保护 | 说明 |
|---|---|---|
| `GLOBAL_RUNTIME` | `SpinNoPreempt<Option<Arc<XKernelRuntime>>>` | 安装一次；`register_clk_provider` 自由函数 clone 出来后立即释放锁 |
| `RESOURCE_PROVIDER` | 无（`XKernelResourceProvider` 是无状态单元结构体） | 所有 Op 方法都是 `&self`，状态在被调用的主机子系统里 |
| `PROBE_MMIO_LEASES` | `SpinNoPreempt<Option<Vec<IoMap>>>` | 单槽，`begin_mmio_probe` 断言不嵌套 |
| `FALLBACK_MMIO_LEASES` | `SpinNoPreempt<Vec<IoMap>>` | scope 外映射的永久归宿；只增不减 |
| `ClkRegistry` 内部 | 见 `clk.rs` | provider 表 + wake 钩子 |

没有跨模块的锁序要求：本 crate 的锁都是叶锁（锁内不调用外部代码，唯一例外
是 wake 钩子，它在 `ClkRegistry` 释放自己的锁之后调用）。

## 设计决策

### provider 无状态，状态在主机子系统里

`XKernelResourceProvider` 是单元结构体。这样 `&'static` 共享是安全的，也不需要
per-domain 的 provider 实例（per-domain 的差异体现在 `XKernelRuntime` 上，
而不是 provider 上）。

### `AdaptedIrqHandler`：把 `Fn() -> IrqAction` 适配成 `kirq::IrqHandler`

驱动侧的 handler 签名是 `Fn() -> IrqAction`（**不带 irq 号**），因为 handler
通过闭包捕获自己的 `Weak`（Linux 的 `dev_id` 模式），不需要靠 irq 号反查设备。
`kirq::IrqHandler::handle(&self, irq: usize)` 带号，适配时丢弃它。

这与旧模型相反：旧的 `IrqHandler::handle(&self, irq)` 迫使 virtio-net 在驱动层
实现"共享中断遍历"——那是总线/中断子系统的职责。

### `resolve_irq_spec` 放在本 crate

见"算法流程"。它原先在 `drv/virtio/devices/src/pci.rs`，导致家族库被迫依赖
`kirq::IrqController` 与 `driver_core::provider::IrqRouteDesc`，并在本地重复了
一份 controller 词汇映射（本 crate 已有 `to_runtime_controller`）。搬过来之后
家族库少两个主机依赖，映射也只剩一份。

### `unsafe impl VirtIoHal for Hal`

vendored trait 要求实现者保证 DMA/MMIO 的生命周期契约。SAFETY 论证：所有操作
都经由全局 provider 路由，且生命周期与 trait 文档一致（DMA 与 MMIO 存活到配对的
dealloc/unmap）。这是本 crate 最大的 unsafe 面，详见 `security.md`。

### `Hal::dma_dealloc` 从 `DmaSpec` 重建释放参数

alloc/dealloc 的参数是确定性对称的（同 pages、同 4K 对齐），所以 dealloc 不查表。
这是 review 三轮 2.2 提出、被判定为"今日正确、改动是加机器不加安全"的项
（`docs/driver-framework-review.md` §8.3 类别 B）：把 spec 存进表会增加查账机器，
收益为零；若 alloc 端的对齐策略变化，编译期/测试会暴露。

## Drop / 资源释放

- `IoMap` / `DevIrq` / `CoherentMem` 的 `ReleaseHook` 由本 crate 在构造时注入，
  闭包捕获 `&'static XKernelResourceProvider` 与释放所需的令牌
  （`MmioMapping` / `IrqResource` + `IrqHandlerToken` / `DmaAllocation` + layout）。
- `unmap_mmio` / `release_irq` **没有错误返回**：`memspace::iounmap` 的结果被
  丢弃（"已经不存在的映射不需要动作"），`kirq::try_free_irq` 的错误只 `log::warn`。
  一个例外：`IrqOp::release_irq` 在 kirq 返回 `InvalidContext` 这类错误时
  **panic**（带 `CG-021` 例外说明 + `TODO(cg)`），因为契约返回 `()`，错误无法
  传回驱动；把 `release_irq` 改成可失败并沿设备 teardown 传播是待办项。
- `FALLBACK_MMIO_LEASES` 永不释放——这是刻意的：scope 外的映射没有归属者，
  释放它会是 use-after-free。旧行为是把令牌直接 drop（静默引用泄漏），现在
  至少可见、可计数。
- `Hal::dma_dealloc` / `unshare` 由 vendored transport 在 drop 时调用；本 crate
  不主动回收。
