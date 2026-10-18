# driver-host — 安全与可靠性分析

## Scope

`drivers/core/driver-host/src/`：`lib.rs`、`provider/{mod,mmio,irq,dma}.rs`、
`clk.rs`、`virtio_hal.rs`、`virtio_transport.rs`。

这是驱动框架里**唯一**直接调用 `memspace` / `kirq` / `kdma` / `khal` / `kclk`
的 crate，因此它承担了框架侧几乎全部的硬件与内存安全责任。

## 信任模型

| 边界 | 谁在外面 | 本 crate 假设什么 |
|---|---|---|
| `driver-base` → 本 crate | `Host` 被当作 `dyn DeviceServices` 调用 | 契约可信：传入的 `MmioRegion` / `IrqSpec` / `DmaSpec` 已经是模型校验过的形状，但**物理地址本身的合法性由 `memspace` 判断** |
| 固件 → 物理地址 | DT/ACPI/PCI 描述符里的 `MmioRegion.base` | **半可信**。本 crate 不做二次判断，直接交给 `memspace::iomap_device` |
| vendored `virtio-drivers` → `Hal` | 传输层通过 `unsafe trait VirtIoHal` 的关联函数调用 | **契约式可信**：本 crate 必须满足 trait 文档的生命周期约定；trait 的签名是**不可失败**的，所以主机侧的错误只能变成哨兵值（见 F-04/F-05） |
| 设备 → DMA 缓冲 | 设备可以对已 `share` 的缓冲做任何事 | **不可信**。这是 DMA 的固有性质，见 T-06 |
| provider 发布 → 本 crate | `ProviderRegistration`（`dev.provide` 经 `ProviderServices`） | provider 必须活到内核生命周期结束（boot-static 模型）；`key` 与 lookup 自称 id 不一致、或重复 key，一律拒绝 |

## 外部边界 / 攻击面

| 输入源 | 入口 | 说明 |
|---|---|---|
| 设备树 / ACPI / PCI 配置空间 | `MmioOp::map_mmio(region, name)` | `region.base`/`region.size` 直接来自固件；`memspace` 是唯一校验点 |
| 同上 | `IrqOp::map_irq(IrqRouteDesc)` | `hwirq`、`controller`、`domain` 来自固件；`kirq` 的 domain 映射是唯一校验点 |
| PCI MSI-X 能力结构 | `drv/virtio/devices/src/pci/x86_64.rs::setup_msix` → `IrqOp::alloc_msix` | MSI-X table BAR/offset/size 来自设备配置空间——**设备可控**。校验在 `bus/pci::msix`（`validate_msix_layout`），本 crate 只提供 alloc/free |
| 设备 DMA | `DmaOp::{alloc_coherent, map_streaming}` | 分配与映射本身由驱动发起；映射之后缓冲内容设备可写 |
| virtio 传输层 | `Hal::{dma_alloc, dma_dealloc, mmio_phys_to_virt, share, unshare}` | `paddr`/`size`/`buffer` 由 vendored 代码给出，最终源头是设备配置空间里的 BAR 与队列描述符 |
| 用户内存 / 用户指针 | 无 | 本 crate 不接受用户态指针 |

## unsafe 代码清单

生产代码 **14 处** `unsafe`，分三组。

### A. `provider/dma.rs` —— kdma 边界（5 处）

| 位置 | 不变量 | 由谁保证 |
|---|---|---|
| `dma.rs:22` `kdma::allocate_dma_memory(layout)` | `layout` 是合法非零 layout | 上一行 `Layout::from_size_align(...).map_err(...)` 已把非法 layout 变成 `ResError::InvalidResource` |
| `dma.rs:49` `kdma::deallocate_dma_memory(info, layout)` | `info`/`layout` 描述的是 `alloc_coherent` 为同一 spec 返回的缓冲，且**恰好释放一次** | `DmaAllocation.spec` 原样携带 alloc 时的 `len`/`align`；释放由 `driver_base::CoherentMem` 的 `Drop` 触发一次。这是 §8.3 类别 B 记录的"对称性靠约定"项 |
| `dma.rs:60` `buffer.as_ref()` | `buffer` 在映射期间是有效活切片 | 调用者（`Hal::share` ← vendored transport）契约 |
| `dma.rs:64` `kdma::map_dma_buffer(buffer, dir)` | 同上 | 同上 |
| `dma.rs:88` `kdma::unmap_dma_buffer(...)` | `mapping` 描述的是 `map_streaming` 建立的映射；`cpu_addr + len` 重建原缓冲 | `DmaMapping` 的四个字段由 `map_streaming` 一次性填齐，之后不可变 |

### B. `virtio_hal.rs` —— vendored HAL 实现（6 处）

| 位置 | 不变量 | 由谁保证 |
|---|---|---|
| `virtio_hal.rs:28` `unsafe impl VirtIoHal for Hal`（**整个 impl**） | 每个方法都满足 trait 文档的生命周期约定：DMA 与 MMIO 存活到配对的 dealloc/unmap | 见下面的"HAL 生命周期论证" |
| `virtio_hal.rs:41` `write_bytes(cpu_ptr, 0, size)` | provider 返回了 `size` 字节的独占相干内存 | `alloc_coherent` 成功即独占；`cpu_addr` 非空由紧随的 `expect` 断言（带 `CG-021` 说明） |
| `virtio_hal.rs:61` `unsafe fn dma_dealloc` | trait 要求的 `unsafe fn`；参数与 alloc 确定性对称 | 同 `dma.rs:49` |
| `virtio_hal.rs:77` `unsafe fn mmio_phys_to_virt` | 返回的指针在调用者使用期间保持有效 | **lease 收集机制**：映射令牌被包成 `IoMap`，进 probe scope（随 transport 存活）或 `FALLBACK_MMIO_LEASES`（永久存活）。绝不 drop 令牌后返回指针 |
| `virtio_hal.rs:94` `IoMap::new(...)` | provider 刚映射成功；lease 的 release hook 配对 provider 的 unmap | 紧邻的 `map_mmio` 成功分支 |
| `virtio_hal.rs:182` `buffer.as_ref()`（在 `unshare` 里） | vendored 契约交出的 buffer 在本次调用期间有效；此处只读 | trait 文档 |

**HAL 生命周期论证**（`unsafe impl` 的整体依据）：

- `dma_alloc` 返回 `(bus_addr, cpu_ptr)`，释放只在 `dma_dealloc` 发生，且由
  vendored transport 在 drop 时调用 → 配对成立。
- `mmio_phys_to_virt` 的映射令牌被 lease 持有；lease 要么进 transport
  （`PciTransport::retain_mappings`，`PciTransport` 的字段声明顺序保证 vendored
  transport 先 drop、映射后释放），要么进永久 fallback 表 → 指针不会悬垂。
- `share`/`unshare` 是无状态的一对：`unshare` 用 `paddr == 0` 判定"从未共享
  成功"，直接返回，不会对未映射的缓冲做 unmap。

### C. `lib.rs` —— guard 构造（1 处）

| 位置 | 不变量 | 由谁保证 |
|---|---|---|
| `lib.rs:182` `IoMap::new(mapping.vaddr, region, release)` | `vaddr` 是 `region` 的有效映射；release hook 配对 unmap | 紧邻的 `self.iomap_resource(region)?` 成功；hook 捕获 `&'static provider` 与 `MmioMapping` |

`provider/irq.rs` **没有 unsafe**：所有 kirq 调用都是 safe API。

## 内存安全不变量

1. **`XKernelResourceProvider` 无状态**（单元结构体），所以 `&'static` 共享不
   引入数据竞争，也不需要 per-domain 实例。
2. **`DmaAllocation.spec` 与 `DmaMapping` 的字段一旦构造就不可变**，释放时按原
   样重建参数。任何"释放时重新计算 layout"的改动都会破坏 `dma.rs:49` 的不变量。
3. **MMIO lease 绝不先于其指针释放**。这是 `mmio_phys_to_virt` 的核心不变量；
   `FALLBACK_MMIO_LEASES` 的存在就是为了在没有 scope 时也能守住它（宁可泄漏
   也不悬垂）。
4. **probe scope 单槽**。`begin_mmio_probe` 的 `assert!` 保证不会有两个 probe
   的 lease 混在一个 scope 里。前提由 `driver-base` 的 probe 泵串行化提供。
5. **固件路由在注册前解析**。`request_irq` 先把 `FirmwareRoute` 映射成 virq，
   再注册；因此 `DevIrq::number()` 与主机实际分发的号一致。破坏这条会让驱动
   持有一个"号是错的"的 guard，共享线上会误判归属。
6. **`Host` 安装先于任何驱动**。`Host::global()` 在
   未安装时 panic（启动顺序断言），而不是静默丢弃 provider 注册。

## 线程安全

- `Host: Send + Sync`（字段是 `&'static` provider + `ClkRegistry`），
  因此可以作为 `Arc<dyn DeviceServices>` 装进域。
- `Hal` 是零大小类型，`VirtIoHal` 的方法全是关联函数 → 天然 `Send + Sync`，
  但也因此**无法携带 per-runtime 状态**，这是"只有一个 provider 实例"的根因。
- `PROBE_MMIO_LEASES` / `FALLBACK_MMIO_LEASES` / `Host` 都是
  `SpinNoPreempt`：**禁抢占、不关中断**。`Hal::mmio_phys_to_virt` 会取这两把锁
  并可能分配地址空间，因此**不得在硬中断上下文首次映射**。
- `IrqOp::request_irq` 里构造的 `AdaptedIrqHandler` 闭包会在**硬中断上下文**
  执行；它只调用驱动给的 `IrqHandlerFn`（`Fn() -> IrqAction`），不取本 crate
  的任何锁。
- `ClkRegistry` 的 wake 钩子在自己的锁释放之后调用，钩子里会取
  `driver-base` 的域锁 → 锁序是 `ClkRegistry → 域锁`，单向，不成环。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|---|---|---|---|---|
| T-01 | 固件给出与内核镜像或其他设备重叠的 `MmioRegion`，`map_mmio` 成功后驱动写寄存器 → 破坏内核内存 | 高 | 被篡改的 DT/ACPI，或固件描述与真实硬件不符 | **已实现**：本 crate 不做物理地址合法性判断，全部委托 `memspace::iomap_device`（`IoMapError::InvalidRange` → `ResError::InvalidResource`）。**残余风险**：若 `memspace` 接受一段与内核保留区重叠的物理地址，本层无法发现。加固建议在 `memspace` 侧维护保留区拒绝表 |
| T-02 | 设备通过 MSI-X 能力结构给出越界的 table BAR/offset/size，`alloc_msix` + 表写入越界 | 高 | 恶意/故障 PCI 设备 | **已实现**：校验**不在本 crate**，在 `bus/pci::msix`（`table_bar >= PCI_BAR_COUNT` 拒绝、`table_size < 2` 拒绝、`validate_msix_layout` 对着 BAR size 校验 offset+size）。本 crate 只提供 `alloc_msix`/`free_msix`，不接受设备给的地址。**残余风险**：`setup_msix` 里 `H::mmio_phys_to_virt(table_paddr, table_bytes)` 依赖上述校验的正确性 |
| T-03 | `mmio_phys_to_virt` 返回的指针在映射释放后被 vendored transport 使用 | 高 | lease 归属错误 | **已实现**：不变量 3。lease 进 scope 后由 `PciTransport.mmio_leases` 持有，字段声明在 `inner` **之后**，因此 vendored transport 先 drop；scope 外的映射进永久 fallback 表 |
| T-04 | 两个 PCI transport probe 并发，lease 混进同一个 scope → 一个 transport 释放了另一个的映射 | 高 | 并发 probe（当前架构不存在） | **已实现**：`begin_mmio_probe` 的单槽 `assert!`，把静默混淆变成响亮 panic。`MmioProbeScope` 的 `Drop` 保证 panic 路径也关闭槽位 |
| T-05 | `DevIrq` 的号与主机实际分发的 virq 不一致，共享中断线上驱动误判归属（吞掉别人的中断，或反复报 `NotHandled` 导致 irq storm 禁用） | 中 | 固件路由未解析就注册 | **已实现**：不变量 5，`request_irq` 先 `map_irq` 再注册，`DomainMapped{number:0}` 这类占位不可能出现 |
| T-06 | 设备对已 `share` 的 DMA 缓冲写入非预期内容 | 高 | DMA 的固有性质 | **部分实现**：本 crate 只负责映射的正确性（方向、长度、cache 维护由 `kdma` 负责）。**内容校验是消费者责任**：virtio-blk 的请求完成校验在 `drv/virtio/devices/src/blk/requests.rs`。**残余风险**：任何新增的 DMA 消费者必须自己校验设备写回的数据，本层不会提醒 |
| T-07 | `dma_dealloc` 用错误的 layout 释放（alloc/dealloc 不对称） | 高 | 修改 `alloc_coherent` 的对齐策略而未同步 dealloc | **部分实现**：`DmaAllocation.spec` 原样携带 alloc 参数，dealloc 从 spec 重建 layout，正常路径下对称是结构保证的。**残余风险**：`Hal::dma_alloc` 硬编码 `PAGE_SIZE_4K` 对齐而 `DmaSpec::new(size, PAGE_SIZE_4K)` 也硬编码同一值——两处常量必须一起改。加固建议：把对齐提取成一个共享常量 |
| T-08 | 未安装 runtime 就发布 provider，注册被静默丢弃 → 消费者永久 deferred | 中 | 启动顺序错误 | **已实现**：驱动侧 `dev.provide` 经设备自己的 `DeviceServices`（`NoServices` 返回 `Unsupported`），发布失败即 probe 失败，不会静默丢弃；HAL 侧 `Host::global()` 带 `CG-021` 启动顺序断言 |
| T-09 | `unmap_mmio` / `release_irq` 失败但无法上报（契约返回 `()`），资源实际未释放 | 中 | kirq 返回 `InvalidContext`、memspace 映射已不存在 | **部分实现**：`unmap_mmio` 丢弃错误（"已不存在的映射不需要动作"，语义正确）；`release_irq` 对 `try_free_irq` 的"未注册"只 `log::warn`，但对 `InvalidContext` 这类错误 **panic**。**残余风险**：panic 发生在 teardown 路径，会把"一个设备移除失败"升级成停机。已记录 `TODO(cg)`：把 `release_irq` 改成可失败并沿 teardown 传播 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|---|---|---|---|---|---|---|
| F-01 | `map_mmio` 失败 | 地址范围非法 / 无内存 / 映射失败 | `ResError` → `RuntimeError` → `ProbeError::Fatal` | 该设备 `Unclaimed`，其余设备正常 | 3 | 三种 `memspace::IoMapError` 映射成三种 `ResError`，不合并 |
| F-02 | `request_irq` 失败 | 线已被独占（`Busy`）/ virq 耗尽（`NoMemory`）/ 描述符非法 | 同上 | 同上 | 3 | `map_irq_desc_error` 把 12 种 `kirq::IrqDescError` 分类映射；`try_register_shared` 返回 `Ok(None)` 表示非共享注册，是正常路径不是错误 |
| F-03 | `map_irq` 失败（只要号不注册） | controller 没有注册 domain / hwirq 冲突 | `resolve_irq_spec` 返回 `None` | 调用方（virtio transport probe）回退到描述符的 legacy 路由；都没有则 probe `Fatal` | 3 | `log::warn` 带完整 route 描述 |
| F-04 | **`Hal::dma_alloc` 失败但 trait 签名不可失败** | provider 返回 `NoMemory` 等 | 返回 `(0, NonNull::dangling())` | vendored transport 会把 bus_addr 0 写进描述符 → 设备 DMA 到物理地址 0 | **2** | **已实现**：`log::error` 带 pages 与错误码。**残余风险**：这是 vendored trait 的不可失败签名强加的哨兵值，本 crate 无法拒绝。缓解依赖 vendored transport 对 0 地址的检查——**不保证存在**。加固建议：向 vendored crate 提交可失败的 `dma_alloc`，或在此处 panic（当前选择"记录 + 返回哨兵"是为了不在启动期把一个设备的内存不足升级成停机） |
| F-05 | **`Hal::share` 失败但 trait 签名不可失败** | `map_streaming` 失败 | 返回 bus_addr `0` | 同 F-04 | **2** | 已实现：`log::error`；`unshare` 用 `paddr == 0` 短路，因此不会对未映射缓冲做 unmap（这一点是安全的）。残余风险同 F-04 |
| F-06 | `Hal::mmio_phys_to_virt` 映射失败 | provider 返回错误 | **panic** | 停机 | 1 | 刻意选择：vendored trait 的关联函数无法返回错误，映射失败在这个边界不可恢复。**对比**：驱动路径经 `Device::iomap` 会正确报 `ProbeError`，所以这个 panic 只在 vendored 构造器内部触发 |
| F-07 | scope 外调用 `mmio_phys_to_virt` | 非 probe 路径的映射（例如运行期 transport 重新映射） | lease 进 `FALLBACK_MMIO_LEASES`，永不释放 | 内核地址空间引用泄漏（可见、可计数） | 4 | 刻意选择：宁可泄漏也不悬垂。旧行为是直接 drop 令牌（静默泄漏且不可见） |
| F-08 | `free_coherent` 收到 null cpu_addr | 上游构造了非法 `DmaAllocation` | **panic**（`expect`，带 `CG-021`） | 停机 | 2 | 不变量：`DmaAllocation` 只能由本 crate 的 `alloc_coherent` 构造，其 `cpu_addr` 来自 `info.cpu_addr.as_ptr()`，非空 |
| F-09 | `unmap_streaming` 收到 `bus_addr == 0` | 对应的 `share` 失败过（F-05） | 直接返回，不做 unmap | 无 | 4 | 显式短路，与 F-05 的哨兵值配对 |
| F-10 | clock provider 永不注册 | 固件引用了不存在的 phandle，或 provider 驱动未编译进来 | 消费者 probe 返回 `ServiceError::Deferred` | 设备 parked；`driver-base::finish_probe_phase` 在启动结束时报告并降级为 `Unclaimed` | 3 | 本 crate 只负责把 `kclk::ClkError::NoProvider` 翻译成 `Deferred`；可见性由 `driver-base` 保证 |
| F-11 | `kclk` 的 prepare/enable 失败 | 时钟树状态错误 | `map_clk_error` → `RuntimeError` → probe `Fatal` 或 `Deferred` | 该设备不被认领 | 3 | `ClkOps` 的每个方法都翻译错误；`unprepare`/`disable` 无返回值，失败只丢弃 |

## 故障管理

- 三层错误词汇，逐层收窄：`memspace::IoMapError` / `kirq::IrqDescError` /
  `kclk::ClkError` → `driver_base::devres::host_api::ResError`（5 变体）→
  `driver_base::RuntimeError`（9 变体）→ `ProbeError`（2 变体）。
  每层都是显式 match，不用 `map_err(|_| ...)` 吞掉分类。
- **唯一的例外是 `ServiceError`**：时钟获取可以返回
  `ServiceError::Deferred(DepSource)`，这是 `RuntimeError` 表达不了的控制流，
  因此时钟契约（`ClockConsumerServices`）单独一个错误类型。这不是重复，是
  "只有能 defer 的能力才需要 defer 词汇"。
- panic 点共 4 处，全部带 `CG-021-no-panic-on-reachable-failure` 例外说明：
  `Host::global()` 的启动顺序断言、`mmio_phys_to_virt` 映射失败、
  `release_irq` 的 `InvalidContext`、`free_coherent`/`unmap_streaming` 的 null 地址。
- `begin_mmio_probe` 的嵌套 `assert!` **不带**例外说明，因为它断言的是设计契约
  而不是不可达路径。

## 隐私分析

无。本 crate 不处理用户数据。日志里会出现物理地址、IRQ 号、时钟频率与设备
BDF——都是硬件描述，与 Linux `dmesg` 的暴露面等价。

## 已知限制

1. **`Hal` 的 `dma_alloc`/`share` 失败只能返回哨兵 0**（F-04/F-05）。根因是
   vendored `VirtIoHal` 的不可失败签名，不是本 crate 的选择。
2. **`Hal::dma_alloc` 不做 cache 维护方向的区分**：`_direction` 与
   `_access_platform` 参数被忽略，一律分配相干内存。非相干平台需要改这里。
3. **`dma_dealloc` 从 `DmaSpec` 重建释放参数**，对称性靠约定（T-07）。
4. **`release_irq` 返回 `()`**，无法把 teardown 期的 kirq 错误传回驱动
   （T-09），`InvalidContext` 只能 panic。已记录 `TODO(cg)`。
5. **threaded IRQ 只到 provider 层**：`IrqOp::request_threaded_irq` 存在，但
   `DeviceServices` 与 `Device` 两层都没有对应入口，驱动无法使用。
6. **MSI-X 只到 provider 层**：`alloc_msix`/`free_msix` 存在，`Device` 未
   暴露；当前唯一消费者是 virtio PCI transport（例外 E-01），且只在 x86_64、
   只对 net 设备启用。
7. **streaming DMA 只覆盖单缓冲区**，无 scatter-gather。
8. **`FALLBACK_MMIO_LEASES` 只增不减**，且没有上限（F-07）。
9. **provider 是进程级单例**，per-domain 的差异只体现在 `Host` 上。
   若将来需要 per-domain 的 IOMMU domain，`VirtIoHal` 的关联函数签名是障碍。

## 审计清单

- [ ] 新增的 `unsafe` 是否有紧邻的 `SAFETY:` 说明，并指名哪个 safe 入口/守卫
      保证不变量？
- [ ] 有没有新增"返回裸指针/裸地址而不持有映射令牌"的路径？（不变量 3）
- [ ] 新增的 DMA 路径是否保持 alloc/free 参数对称？对齐常量是否只有一处？（T-07）
- [ ] 新增的 IRQ 路径是否**先解析固件路由再注册**？（不变量 5）
- [ ] 新增的锁是否是叶锁？如果不是，锁序是否仍是 `ClkRegistry → 域锁` 单向？
- [ ] 新增的 `pub` 项是否真的有外部调用者？（本 crate 已经因为
      `take_mmio_probe_leases` 无人调用而删除过一项）
- [ ] 主机侧错误是否被翻译成**分类后的** `ResError`，而不是 `map_err(|_| ...)`？
- [ ] 新增的 panic 是否带 `CG-021` 例外说明，且论证了"可达即设计错误"？
- [ ] 有没有把设备可控的数据（配置空间、DMA 写回内容）当成可信输入？
      本 crate 不校验内容，校验责任在消费者——新增消费者时要显式确认。
- [ ] 改动 `provider/irq.rs` 的词汇翻译（`to_runtime_controller` /
      `to_runtime_route`）时，是否同步检查 `driver-base::IrqController` 的变体
      集合有没有增加？漏一个变体会静默落到 `Unknown`。
