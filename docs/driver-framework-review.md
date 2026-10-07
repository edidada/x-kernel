# 驱动框架架构评审与重构设计（面向 AI 转写 Linux 驱动）

> 评审范围：仅基于当前代码（drivers/ 及相关 crate），不涉及历史 commit。
> 结论分三部分：(1) 现状调用链；(2) 大问题清单及根因；(3) 面向 AI 转写的重构设计。
>
> **修订**：本版基于主线 `c409a9db`（含 !796 devtmpfs/sysfs 动态发现）重新审视，
> 变化与影响见第 5 节；第 2~4 节结论除特别标注外仍然成立。
>
> **修订 II**：第 7 节为重构落地进度；第 8 节为架构债清理、三轮外部评审
> 闭环、明确不修清单（含理由分类）与教训记录，截至 `35ad14c89`。

---

## 1. 现状：框架实际如何运转

启动调用链：

```
init_drivers()                                   [integration/kdriver/src/lib.rs]
 └─ device_manager()                             [manager.rs]
     ├─ default_bus_manager()                    [bus/manager.rs]
     │    ├─ register_bus_type(PCI / PLATFORM)   ← 总线类型是 kdevice 里的常量
     │    ├─ PlatformBackend                     ← DT/ACPI/静态设备枚举
     │    └─ PciBackend                          ← PCI 枚举 + BAR 配置
     └─ register_default_drivers()
          └─ DRIVER_FACTORIES 表（cfg(feature) 拼出来的 fn 列表）
 └─ discover_and_probe()
      └─ backend.enumerate()  → EnumerationContext 收集 DeviceDesc
      └─ probe_pending()      → probe_device_desc()
           ├─ bus_type.match_desc(driver.matcher(), desc)   ← 命令式匹配
           ├─ ops.probe_device(Arc<DeviceObject>)           ← 驱动内部自己 publish_*
           └─ activate_device() / 事件分发 → kclass 通知订阅者
```

关键事实（后续论证的证据）：

- `DeviceDriver::probe_device(&self, device: Arc<DeviceObject>) -> DriverResult<()>`
  —— probe 只拿到一个"只读"设备对象，**没有驱动私有数据（drvdata）机制**。
- `DeviceObject`（contracts/kdevice/src/device/object.rs）字段：id/bus/location/identity/
  resources/lifecycle/usage/devres/state。state 里只有 driver_id、driver_name、device_kind。
- 类设备是**第二个对象**：`kclass::ClassDevice<T>` 包着 parent `Arc<DeviceObject>` + runtime，
  由驱动在 probe 内部调用 `kclass::publish_block/net/...` 注册到宏生成的全局 registry。
- virtio-blk 的运行期状态存放在 `ACTIVATIONS: Mutex<BTreeMap<DeviceId, Box<dyn FnOnce()>>>`
  （integration/kdriver/src/driver_registry/virtio/block.rs）。
- virtio-net 的中断路由依赖三个全局 static：`NET_IRQ_HANDLES`、`REGISTERED_NET_IRQS`、
  `NET_IRQ_GUARDS`（devices/virtio/src/net.rs），全局 handler 按 IRQ 号遍历匹配。
- 全局单例：`DEVICE_REGISTRY`（kdevice/registry/mod.rs）、kclass 的 7 个宏生成 registry、
  kdriver 的 ACTIVATIONS、各 ID 分配器 Atomic 计数器。测试全部 `def_test(serial)`。

---

## 2. 大问题清单（按严重性排序）

### P0-1 缺少 drvdata —— 设备模型的承重墙缺失

Linux driver model 里 `dev_set_drvdata/dev_get_drvdata` + `request_irq(..., dev_id)` 让
probe 创建的私有状态、中断上下文、remove 逆操作天然挂在设备上。本框架的
`Arc<DeviceObject>` 没有任何挂载驱动私有状态的槽位，于是**每个驱动被迫自建全局旁路表**：

- virtio-blk：`ACTIVATIONS: BTreeMap<DeviceId, Box<dyn FnOnce()>>`，remove 时查表执行闭包；
- virtio-net：三个全局 static 做 IRQ 号 → 设备句柄的反查；
- 探测失败回滚 = `device.run_cleanups()` + 驱动自己 add_cleanup。

这不是某个驱动写得不好，而是**模型缺口逼出来的必然结果**：凡 probe 里创建的资源，
要么塞进 devres 闭包黑箱（只能整体撤销、无法交互），要么进全局表（无法测试、无法多实例）。

### P0-2 IRQ 绑定模型错误：按全局 IRQ 号分发，而不是按设备

`IrqHandler::handle(&self, irq: usize) -> IrqEvent` 的签名决定了 handler 拿不到设备上下文
（除非闭包捕获，而框架没有给捕获设备的标准通道）。于是 virtio-net 在驱动层实现了
"共享中断遍历"——这本是总线/中断子系统的职责。`IrqOp` provider 是全局单例
`XKernelResourceProvider`，`Irq` guard 的生命周期靠驱动自己的全局 Vec 兜底。

### P0-3 契约层被具体驱动污染，"分层"名存实亡

- `contracts/block/src/` 里有 `ahci.rs`、`bcm2835sdhci.rs`、`sdmmc.rs`、`ramdisk*.rs`；
- `contracts/net/src/` 里有 `fxmac.rs`（内含裸指针 `*mut FXmac` 的完整驱动）、`ixgbe.rs`；
- 同期的 virtio 驱动却在 `devices/virtio/`，ramdisk 又走 `block/ramdisk` feature；
- kdriver/Cargo.toml 自述这些是 "dormant placeholders ... intentionally undeclared"。

也就是说：**同一个角色（"块设备驱动"）在三个不同的层有三个不同的合法存放位置**，
其中一部分还是编译不进去的死代码。四层目录传递的架构信息是假的。

### P0-4 一个驱动横跨 3~4 个 crate、6+ 文件、外加 feature 矩阵

以 virtio-blk 为例，新增/修改一个驱动要触达：

| 位置 | 内容 |
|---|---|
| `contracts/block/src/lib.rs` | BlockDeviceOperations / Gendisk |
| `devices/virtio/src/blk.rs` + `blk/requests.rs` | 设备逻辑、请求对象池 |
| `integration/kdriver/src/driver_registry/virtio/mod.rs` | DeviceDriver impl、按 DeviceKind 的 match 分发 |
| `integration/kdriver/src/driver_registry/virtio/glue.rs` | HAL 实现（DMA/MMIO 绑定） |
| `integration/kdriver/src/driver_registry/virtio/block.rs` | 激活/IRQ/清理注册表 |
| `integration/kdriver/src/block_completion*.rs`, `block_irq.rs` | 类级补全/softirq 机制 |
| `integration/kdriver/Cargo.toml` | `virtio-blk = ["block","virtio","virtio/block",...]` feature 扇出 |
| `DRIVER_FACTORIES` 列表 | cfg 拼 ctor 列表 |

对 AI 转写而言这是最致命的：**转写产物没有局域性**，模型必须同时正确改 8 处，
任何一处漂移都导致驱动"接不上"，且失败模式是静默的（cfg 关掉就没了）。

### P1-5 总线类型是封闭集合，且总线层掺设备知识

- `BusTypeId::PCI / PLATFORM` 是 kdevice 常量，`new_builtin` 对其它 id **panic**；
  匹配策略（PciBusTypeMatcher 等）硬编码在 kdevice 里。USB/I2C/SPI/SDIO/SCSI 无法
  以"外部 crate 注册"的方式接入——而这些恰是 Linux 驱动存量大头。
- `PciBackend` 从 `driver_registry::virtio::ids` 导入 `VIRTIO_PCI_VENDOR_ID` 并把
  vendor/device 映射成 `TransportInfo::Virtio`：**总线枚举代码知道具体设备家族的 ID 表**。
- `PlatformBackend` 硬编码 `"virtio,mmio"`，并在**枚举阶段** iomap + 读寄存器探测
  transport 类型。枚举与驱动职责倒置。
- PCI host/bridge 这类纯拓扑实体被迫伪装成 `DeviceDriver`（NeverMatcher + 空 probe）
  走 adopt 流程。

### P1-6 匹配模型非数据驱动，priority 是死机制

Linux 的 `pci_device_id` / `of_device_id` 是 **const 数据表**，由 bus core 统一比较，
支撑了 MODULE_DEVICE_TABLE、modalias、uevent 等整套机制。本框架的 `DeviceMatcher` 是
命令式 trait 对象；`MatchResult::Match { priority }` 和 FALLBACK/GENERIC/EXACT 定义了
完整的优先级体系，但**全部现有 matcher 只返回 EXACT**——为未来设计、从未兑现的机制。
AI 转写时无法做"Linux id 表 → 框架数据结构"的机械映射，必须手写 trait impl。

### P1-7 probe 没有 defer 语义，生命周期协议自造

- Linux 的 `-EPROBE_DEFER`（依赖 clock/regulator/gpio 未就绪时挂起重试）是端口驱动
  最高频的控制流之一。本框架用 `attempted: Vec<DriverId>` + `ProbeOutcome::Requeue`
  近似，但 requeue 后**没有任何依赖事件能触发再探测**，实际语义是"耗尽即放弃"。
- `begin_removing()`/usage 计数是一套自造的移除协议，与 devres、驱动的 ACTIVATIONS
  闭包、Irq guard 形成三套并行的清理体系。
- probe 错误路径里残留 `let _ = error;` 这类"先吞掉"的写法，错误分类
  （哪个错误该 requeue、哪个该放弃）没有模型支撑。

### P1-8 全局单例森林，模型不可实例化

DEVICE_REGISTRY、kclass 七个 registry、ACTIVATIONS、NET_IRQ_*、ID 计数器全部是进程级
static。后果：(a) 测试只能串行（`def_test(serial)`），无法并行与隔离；(b) 单内核镜像
不可能有第二个设备域（今后 VM/容器化驱动域直接堵死）；(c) 每个新驱动都有动机再造一个
全局表（P0-1 的放大器）。

### P2-9 双设备模型、双错误模型、类接口不统一

- 拓扑视图 `DeviceObject` 与类视图 `ClassDevice<T>` 是两个平行对象，各自持有名字/IRQ/
  状态，靠 DeviceId 弱关联；
- `DriverError`（10 个变体）与 `KError` 在 `contracts/block` 的 ioctl 边界直接相接；
- block 有 major/minor/Gendisk 全套，net 有 NetRxScheduler 注入，char 只有
  read/write/flush——每个类各造一套发布协议（宏只是复制了样板，没有统一语义）；
- `DeviceKind` 是封闭 enum：新增类（tty/usb/i2c/watchdog/rtc/mtd…）要改 driver_base、
  kclass 宏表、kclass/kdriver 两级 feature。**把"设备类"当成了核心不变量，实际上它是
  开放扩展点。**

### P2-10 Kconfig → Cargo feature 的手工镜像

build.rs 翻译层已删除（Cargo.toml 注释），留下手工维护的三方镜像：Kconfig 项 ↔
kdriver feature 名 ↔ 源码 `cfg(feature)`（kdriver 内 106 处）。还催生了
`any_firmware_driver` 这类只为 debug_assert 服务的伞 feature。一致性完全靠人肉，
对 AI 转写是持续的噪声源。

---

## 3. 根因分析（从代码证据推断）

1. **按"名词"分层，而不是按依赖方向分层。**
   contracts / adapters / devices / integration 是按*工件种类*划分的目录，不是按*职责依赖*。
   当 contract 层表达不了驱动的真实需求（drvdata、per-device IRQ 上下文）时，代码只能
   "越狱"：具体驱动进了 contracts，glue 进了 integration，状态进了全局 static。
   层次在包名上成立，在依赖事实层面破产——`integration` crate 的永久存在本身就是症状：
   组装逻辑本应是 composition root 的一瞬，却成了常驻居民。

2. **仿 Linux 之形，未取 Linux 之神。**
   命名全是 Linux 的，但 Linux driver model 的真正承重点——drvdata、数据表匹配 +
   modalias、devres 单一所有权路径、EPROBE_DEFER、引用计数生命周期——一个都没落地。
   仿形的代价是：每个缺失的不变量都要用别的机制补洞（全局表、闭包 cleanup、伞 feature），
   于是问题看起来散落在各驱动里，实则是核心抽象缺角。

3. **多个技术谱系的缝合。**
   代码里能看到三种血统：device-res/adapter 是 Linux 风格的（devm_*、threaded irq、MSI）；
   kdevice/kclass 是自研的"统一设备模型"；virtio 栈寄生在外部 crate
   `virtio-drivers(kvirtiodrivers)` 的 Hal/Transport 泛型上（嵌入式 no_std 谱系，伴随
   SpinNoIrq/lazyinit/crate_interface 等基础设施）。三种谱系各自解决一部分问题，在
   kdriver 里缝合。IRQ 路由进全局表，正是因为内核不拥有 virtio 的 transport/queue/中断层。

4. **一次性同步启动流。**
   `init_drivers()` 是单线程 boot 流，没有 module_init/late_init 分层，注册顺序问题只能用
   "先注册驱动再扫描总线"+伞 feature+debug_assert 兜底，defer probe 无处安放。

5. **约束与需求长期冲突后双方妥协。**
   dormant placeholder、被删除的 build.rs、"先吞错误"的残留，说明"保持 contracts 纯净"与
   "驱动要能跑"两条约束反复拉锯，最后两边各退半步：代码既不纯净也不完整。
   这是架构边界定错位置（P0-3 的因）的典型结局。

---

## 4. 重构设计：面向 AI 转写 Linux 驱动的框架

### 4.0 设计目标：什么叫"适合 AI 转写"

转写 Linux 驱动本质上是一个高度模式化的变换。框架的任务是**把变换空间压到最小**：

1. **局域性**：一个驱动 = 一个 crate（或一个目录），注册 0~1 行代码；
2. **结构同构**：Linux 每个概念在本框架有一一对应、语义一致（同名或近名）；
3. **数据驱动**：驱动描述符是 const 数据，不是命令式 trait 实现；
4. **可验证**：mock 总线 + 实例化模型，转写结果可以在无硬件环境下单测；
5. **映射表即规范**：Linux API → 框架 API 的对照表是规范性文档，不是事后总结。

### 4.1 目标 crate 图（依赖方向自上而下）

```
            ┌────────────────────────────────────────────┐
            │ composition root（原 kdriver 的合法残余）   │
            │  init: 收集链接段注册的驱动 → 枚举 → probe  │
            └──────┬───────────────┬──────────────┬──────┘
                   │               │              │
             ┌─────▼─────┐  ┌──────▼──────┐  ┌────▼─────┐
             │ bus-pci   │  │ bus-platform│  │ drv/xxx  │  ← 每驱动一 crate
             │（只做枚举/ │  │（DT/ACPI，  │  │ (virtio, │
             │  BAR/资源) │  │  数据表匹配)│  │  serial…)│
             └─────┬─────┘  └──────┬──────┘  └────┬─────┘
                   │               │              │
        ═══════════╪═══════════════╪══════════════╪════════ 驱动 API 稳定边界
                   │               │              │
             ┌─────▼───────────────▼──────────────▼─────┐
             │ driver-core：设备/驱动/总线/匹配/生命周期  │
             │  + driver-runtime：per-device 服务        │
             │    (mmio/irq/dma/time/work/completion)    │
             └─────┬──────────────┬──────────────┬─────┘
                   │              │              │
             ┌─────▼────┐  ┌──────▼─────┐  ┌────▼─────┐
             │class-api │  │ class-net  │  │ khal/…   │
             │(注册点)   │  │ class-block│  │ (现有内核)│
             └──────────┘  └────────────┘  └──────────┘
```

要点：

- **driver-core 吸收 kdevice + device-res + kclass 的注册功能**，成为唯一的设备模型；
- **driver-runtime 把 device-res 的 devm 语义变成 Device 的方法**（不再是独立 provider
  全局单例 + 线传 provider 参数）；
- **class-api 只剩 trait + 注册函数**，一个类一个文件，无全局 registry、无宏工厂；
- **bus-* 与 drv/* 平级**，都是 driver-core 的消费者，新增总线不改核心。

### 4.2 核心接口（示意）

```rust
// driver-core —— 驱动描述符是数据，不是 trait 对象
pub struct DriverDesc<B: Bus, P: DriverOps<B>> {
    pub name: &'static str,
    pub ids: &'static [B::IdEntry],        // 机械翻译自 pci_device_id / of_device_id
    pub ops: P,
}

// probe 携带类型化私有数据（= linux device private data）
pub trait DriverOps<B: Bus>: Sync {
    type Data: Send + Sync;

    fn probe(&self, dev: &mut DeviceCtx<'_, B>) -> Result<Self::Data, ProbeError>;
    fn remove(&self, dev: &mut BoundDevice<'_, B, Self::Data>) {}
    fn suspend/resume/shutdown(&self, ...) {}          // 同构映射
}

pub enum ProbeError {
    Deferred(DeferredReason),   // = -EPROBE_DEFER，框架挂起，依赖就绪事件再探测
    Fatal(DriverError),
}

// per-device 服务：全部挂设备上，probe 失败自动回滚，remove 统一逆序释放
impl<'_, B: Bus> DeviceCtx<'_, B> {
    fn iomap(&mut self, r: MmioRegion) -> DevResult<IoMapping<'_>>;
    fn request_irq<F: FnMut(&Self::Data) -> IrqAction>(
        &mut self, r: IrqResource, handler: F) -> DevResult<IrqGuard<'_>>;
    fn alloc_coherent(&mut self, spec: DmaSpec) -> DevResult<CoherentMem<'_>>;
    fn defer(&self) -> ProbeError;                     // 显式 defer
    fn set_drvdata(self, data: Self::Data);            // drvdata 一等公民
}

// 注册：链接段收集（等价 module_init），一个驱动文件 0 行注册改动
pub fn register_driver<B: Bus, P: DriverOps<B>>(desc: &'static DriverDesc<B, P>);
// 或声明宏：xdriver!(pci = IDS, probe = FooOps);  /  xdriver!(of = MATCH, ...)
```

匹配示例（转写 `static const struct pci_device_id e1000_pci_tbl[]`）：

```rust
// Linux:  { PCI_DEVICE(0x8086, 0x100E) }, ...
static IDS: &[PciId] = &[PciId::device(0x8086, 0x100e), PciId::device(0x8086, 0x100f)];

// Linux:  static const struct of_device_id foo_of_match[] = { { .compatible = "x,y" } };
static OF: &[OfMatch] = &[OfMatch::compatible("vendor,dev")];
```

总线 core 统一做表比较（可再生成 modalias 字符串供 uevent/sysfs 使用），
`DeviceMatcher` 命令式 trait 降级为逃生舱（`ids()` 返回 `IdTable::Custom(&dyn ...)`），
priority 删除或推迟到有真实需求再加。

### 4.3 关键机制设计决策

| 议题 | 决策 |
|---|---|
| drvdata | `Device::Data` 关联类型 + `BoundDevice<B, D>` 携带 `Arc<D>`；IRQ/工作队列闭包捕获 `Weak<D>`，设备移除自动失效 |
| IRQ | `request_irq(dev, res, handler)`，handler 拿 `&D`；共享中断由 irq 子系统按 controller 能力处理，驱动不感知；MSI/MSI-X 走 `dev.alloc_msi()` |
| defer | `ProbeError::Deferred` + 依赖源（irq domain/clock/另一个设备）就绪时框架自动 re-probe；取代 attempted 集合 |
| 生命周期 | 唯一所有权路径 = devres 树；probe 失败自动回滚全部已申请资源；remove = 逆序释放，删除驱动侧 ACTIVATIONS/闭包黑箱；引用计数只留在 Device 句柄上 |
| 设备类 | `ClassTag(&'static str)` 开放集合；类 crate 只含 trait + `register_class_device()`；block 的 major/minor、net 的 rx scheduler 变成类 crate 内部细节，不进核心 |
| 总线 | `trait Bus: RegisterBus`，bus crate 自带 IdEntry 匹配与枚举；kdevice 不再内置 PCI/PLATFORM 常量（迁移为 bus crate 注册） |
| 全局状态 | 模型实例化为 `DeviceDomain`（组合 root 持有一个；测试可 new 任意个）；ID 分配器归 domain |
| 配置 | 每驱动恰好一个 feature = 依赖其 crate，禁止伞 feature；`cfg(feature = "drv-<name>")` 只出现在该驱动 crate 内部；check-cfg 进 CI |
| virtio | `kvirtiodrivers` 降级为 transport 实现细节，包在 `drv/virtio` 内；IRQ/队列归 driver-runtime 管，删除 NET_IRQ_* 全局表 |

### 4.4 资源模型的详细变换（ResourceDesc / Provider → 什么）

现有 resource 模型由四部分组成：固件资源描述（`ResourceDesc`/`ResourceSet`）、
service provider（`MmioOp/DmaOp/IrqOp/TimeOp` → `ResourceProvider` 全局单例）、
devm 闭包机制（`DeviceResource::register_cleanup` + `devm_*_with_provider`）、
RAII guard（`Io`/`Irq`/`DmaCoherent`/`CoherentBox`）。重构做三个反转：

**反转一：Provider 从驱动可见的全局单例 → 域内部 backend。**
四个 ops trait 合并为内部 `RuntimeBackend`，由 `DeviceDomain` 持有实例；每个方法
携带设备上下文（`map_mmio(dev, …)`、`alloc_coherent(dev, attrs, spec)`、
`request_irq(dev, spec, action)`），为 per-device DMA 属性/IOMMU/中断域和资源审计
打开空间。`adapters/xkernel/device-res` 更名为 backend crate，桥接逻辑不变。
`TimeOp` 移出资源体系 → `dev.runtime()` 服务（sleep/timer）。
驱动侧 `resource_provider()`、`DeviceResourceExt`、`map_res_err`、以及直连
memspace 的 `iomap_mmio()` 旁路全部删除，只剩 `dev.*()` 一个入口。

**反转二：devm 闭包黑箱 → 类型化所有权（Data 即 devres）。**
probe 申请的一切 guard 必须存进 `Self::Data`；probe 失败 = drop(Data) 按字段逆序
自动回滚；remove = quiesce → drop(Data) → 释放残余 devres。闭包列表退化为框架
内部 + `dev.devres_add(guard)` 逃生舱。`DeviceResource` trait 解散：`resources()`
变成 DeviceCtx 类型化访问器，`register_cleanup` 不再公开。ACTIVATIONS/
NET_IRQ_GUARDS 等驱动侧全局清理表随之消失。

**反转三：`ResourceSet` → 命名 `ResourceMap`，DMA 剥离。**
`ResourceDesc::Dma(DmaSpec)` 是范畴错误（分配请求 ≠ 固件事实），删除该变体：
DMA 走 `dev.alloc_coherent(spec)`，平台 DMA 窗口/一致性变成设备属性 `DmaAttrs`
（对应 `of_dma_configure`/`dma_set_mask`）。条目获得名字
（DT 的 `reg-names`/`interrupt-names`，PCI 的 "BAR0"/"INTA"），访问从 `first_mmio()`
钓鱼变成 `dev.mmio(n)` / `dev.mmio_by_name("regs")` / `dev.irq(0)`，对齐
`platform_get_resource_byname`。IRQ 编号语义修正：枚举产出 `IrqSpec`
（controller/domain/hwirq/trigger，现有字段设计保留），`dev.request_irq()` 时解析为
虚拟中断号；`map_irq(route)` 变为总线/中断域内部职责。

**保留不变：** `Io` 的 volatile 访问器（边界检查 + fence）、`CoherentBox<T>`/
`CoherentArray<T>`、`IrqResource` 字段设计、adapter 到 kirq/kdma/memspace 的桥接
逻辑——它们成为新 API 的返回类型与内部实现。

| 现状 | 目标 | 理由 |
|---|---|---|
| `ResourceDesc::Mmio/IoPort/Irq` | `ResourceMap` 命名条目 | Linux byname 访问同构 |
| `ResourceDesc::Dma(DmaSpec)` | 删除；`DmaAttrs` + `alloc_coherent()` | 分配请求≠固件资源 |
| `ResourceSet` 快照 ×2（desc+device） | 设备内单一 `ResourceMap` | 消除双拷贝 |
| `first_mmio()/first_irq()` | `dev.mmio(n)/by_name/irq(n)` | 位置钓鱼→命名访问 |
| `DeviceResource` trait | 解散（访问器进 ctx，cleanup 内部化） | 黑箱→类型化所有权 |
| `MmioOp/DmaOp/IrqOp/TimeOp` | 内部 `RuntimeBackend`（带 dev）；time 移出 | 全局单例→per-domain |
| `ResourceProvider` 组合 trait | 删除 | 驱动不再见 provider |
| `Io::map_with` 等显式构造器 | `dev.iomap()` 等方法 | 单一入口 |
| `devm_*_with_provider` 闭包黑箱 | guard 存 Data，drop 即回滚/释放 | 可审计、可排序 |
| `XKernelResourceProvider` static | `DeviceDomain` 持 backend 实例 | 可测试、多域 |
| `map_res_err`（ResError→DriverError） | 单一 ProbeError/RuntimeError | 错误词汇统一 |
| kdriver `iomap_mmio` 直连 memspace | 删除，统一走 `dev.iomap` | 旁路消灭 |

### 4.5 面向 AI 转写的工程配套（与框架同等重要）

1. **`docs/linux-porting-map.md`（规范性映射表）**：
   `probe/remove → DriverOps`、`pci_device_id → PciId 表`、`of_device_id → OfMatch 表`、
   `devm_ioremap_resource → dev.iomap()`、`request_threaded_irq → dev.request_irq()`、
   `dma_alloc_coherent → dev.alloc_coherent()`、`completion → driver-runtime::Completion`、
   `-EPROBE_DEFER → ProbeError::Deferred`……每条给出两侧代码片段。
2. **两个 golden 模板驱动**（一个 platform/DT、一个 PCI）作为转写 few-shot 范本，
   同时是框架的集成测试。
3. **无硬件验证闭环**：mock 总线（注入 DeviceDesc）+ 现有 fuzz/ + QEMU 平台跑
   冒烟（现有 xkernel_aarch64-qemu 目标）。
4. **可选声明宏 `xdriver!`**：把注册样板收到一行，同时静态检查 ids 表类型。
5. **拒绝静默失败**：未匹配设备/feature 缺失在 boot log 中输出可诊断信息
   （比 cfg 静默编译掉友好得多，AI 迭代时能自我纠错）。

### 4.6 迁移路线（strangler，不做 big-bang；按"重构先行"修订）

- **Phase 0（最小脊柱，以 clk 落地为验收）**：新建 driver-core/driver-runtime，
  只做四件事——DeviceCtx + `Self::Data`、`ProbeError::Deferred`、ResourceMap
  命名条目、RuntimeBackend（可先包裹现有全局 provider，域实例化后置）+ clk
  service crate。验收：kclk 本体与 PL011 消费逻辑从 `clk/tail` 分支移植到新
  API（glue 不合入主线），产出跨设备服务消费者 golden 模板。clk 此时以原生
  形态合入主线。
- **Phase 1**：搬 virtio-blk/net 到新 API 作为范式驱动，删除其全局表；建立
  "Linux→x-kernel 映射表"文档 + 其余 golden 样例（platform/DT + PCI 各一）。
  同时完成 Phase 0.5：devnode/uevent 平移进 driver-core（见 5.3）。
- **Phase 2**：清洗 contracts：ahci/fxmac/ixgbe/sdhci 等移出或删除死代码；
  class 层重排为 class-* 纯 trait crate；DeviceNodeKind 数据驱动化 + block
  投影 + modalias 生成。
- **Phase 3**：bus 层去设备知识（virtio vendor 表移入 virtio 驱动 ids）；
  PCI/PLATFORM 常量改为 bus crate 注册；platform 枚举不再探测 virtio transport。
- **Phase 4**：测试域化（并行单测）、feature 矩阵 CI 化、`integration/kdriver`
  收缩为纯 composition root（只剩 init 顺序编排）。

每阶段可独立合入、独立回滚，旧驱动按"碰到即迁移"节奏推进。

## 5. 主线同步修订（!796 之后重新审视）

主线新增 `!796 feat(device): add dynamic devtmpfs and sysfs discovery`，
在四个位置落点：`kdevice/src/device_node.rs`（779 行，新增）、新 crate
`kdevice-uevent`（324 行）、`fs/filesystems/sysfs`（792 行，重写）、
`fs/filesystems/devfs/src/dynamic.rs`（208 行）。启动链：`fs/boot` 先装
sysfs 投影和 uevent publisher，再枚举设备；kclass 发布 input/display 类设备时
经 `ClassRuntimeMetadata::device_node_class` 顺带注册 `DeviceNode`，投影层
订阅事件创建 `/dev/input/eventN`、`/dev/dri/cardN`、sysfs 目录树和
`uevent` 属性文件，并向 netlink KOBJECT_UEVENT 广播 Linux 格式的
`ACTION/DEVPATH/SUBSYSTEM/MAJOR/MINOR/DEVNAME`。

### 5.1 对问题清单的增量影响

| 结论 | 变化 |
|---|---|
| P2-9 双设备模型 | **恶化为三重**：DeviceObject（总线拓扑）+ ClassDevice<T>（类视图）+ DeviceNode（用户态投影），三套身份各自持有 name/driver/location，仅靠 DeviceId 弱关联 |
| P1-8 全局单例 | 恶化：新增 `DEVICE_NODES: LazyInit<SpinNoPreempt<...>>` 全局注册表 + 订阅者列表 + 单 publisher 槽位 |
| P0-4 驱动分散 | 恶化：类发布路径现在在宏内隐式串联 class registry → DeviceNode 注册 → 事件分发 → uevent，控制流更加不可见 |
| 新增问题 | `DeviceNodeKind` 又是一个封闭 enum（Input/Drm），INPUT_MAJOR/DRM_PRIMARY_COUNT/devname 格式硬编码在 kdevice 里；投影不可扩展（block/net/tty 想投影必须改核心）；**block 设备完全缺席**（Gendisk 已有 major/minor 却无 devnode 路径）；**没有 MODALIAS**（Linux 冷启动 udev 依赖它触发模块加载） |

### 5.2 对重构设计的修订（方向验证 + 三点调整）

**验证**：!796 把"Linux 结构同构"原则用在了**消费侧**（用户态看到什么：
major/minor、devname、uevent 属性格式、能力位图字序），做得很对——这恰好
证明该原则有效。重构要做的只是把同一原则补到**生产侧**（驱动作者写什么）。

调整一：**devnode/uevent 升级为一等模块并数据驱动化**。新架构中
`DeviceNodeDescriptor { subsystem, major, minors, devname_fmt, properties }`
由类 crate 注册，取代封闭的 `DeviceNodeKind` enum；block（sda/vda）作为
第一个泛化验证目标接入投影（它已有 major/minor，是验证"投影可扩展"的
最短路径）；`ClassRuntimeMetadata::device_node_class` 从宏内隐藏钩子变为
类 trait 的显式方法。

调整二：**uevent 载荷格式成为 API 锚点**。它是全仓最严格的 Linux 兼容面
（有测试锁定字序/路径/属性顺序），重构必须保持字节级不变——这为 driver-core
重写提供了现成的回归护栏：重写后跑 uevent 断言即可验证行为等价。
迁移顺序上，devnode/uevent 应**最先**搬到新核心（Phase 0.5）。

调整三：**补 MODALIAS 强化数据驱动匹配的收益**。id 表数据化（4.2 节）后
modalias 字符串可由 bus core 机械生成，随 uevent 发布——为将来动态驱动加载
（AI 转写产物热插拔）铺路。这是数据驱动匹配从"代码整洁"升格为"功能必要"
的依据。

### 5.3 迁移路线追加项

- Phase 0.5：devnode 注册表 + uevent publisher 平移进 driver-core（行为不变，
  全局 static → 域内实例），uevent 载荷测试作为等价性护栏先行建立；
- Phase 2 追加：DeviceNodeKind 数据驱动化 + block 投影接入 + modalias 生成；
- golden 模板驱动的验收标准追加：发布类设备后 `/dev`、sysfs、uevent 三面
  可见（现有链路作回归基线）。

### 5.4 clk 子系统接入推演（设计验证）

以在制分支 `clk/tail`（51 文件 +9446 行）为样本做对照。其中 `core/kclk`
（~4400 行 CCF 风格实现）+ DT 解析是子系统本体，自洽且与框架无关，
重构原样保留。真正的接入摩擦是散在十个框架文件里的 ~2500 行：

| clk/tail 被迫做的框架改动 | 重构设计中的既有构件 |
|---|---|
| `ProbeOutcome::Deferred`，用 `DriverError::should_retry()`（WouldBlock/ResourceBusy）重载 defer 语义 | `ProbeError::Deferred(依赖源)`，显式依赖源精确唤醒 |
| enumeration.rs 多轮重试循环（“每轮须有进展否则放弃”启发式） | 依赖就绪事件 → 定向 re-probe，无需轮询 |
| `ResourceDesc::Clk/ClkProvider` 新变体 + `clk_at/clk_by_name` | ResourceMap 命名条目（4.4 反转三；clocks/clock-names 即其典型用例。注意：Clk spec 是固件事实，留在 ResourceMap 是正确设计，与 DmaSpec 的范畴错误不同） |
| `ClkOp` trait + `Clk::get_with(&'static dyn ClkOp, ..)` 显式线程传 provider | RuntimeBackend 内部化 + `dev.clk("apb_pclk")` |
| kdriver/resource.rs +452 行 `devm_clk_get` | DeviceCtx 服务方法，guard 存 Data 自动托管 |
| `ClkProviderDesc` 塞进 DeviceObject 资源表 | provider 驱动 probe 时向 clk registry 注册（= `of_clk_add_hw_provider`），“是 provider”是驱动行为而非固件资源 |

结论：**按重构方向，clk 是增量接入（一个 service crate + DeviceCtx 一个方法 +
kclk 本体），不动 driver-core；不重构，则每个跨设备子系统（regulator/
pinctrl/reset/gpio/iommu）都要重复 clk/tail 的六处框架改动**。clk/tail 与重构
方向同构，可视为 Phase 0 先导：其 Deferred/命名资源构件应被吸收进 driver-core
正式 API，迁移点仅 `should_retry` → 显式 `Deferred`（机械替换）与轮询 →
事件驱动（可后置增强）。重构窗口宜早：若后续子系统继续扩 ResourceDesc，
迁移面会持续变大。

**合入决策（修订版，按“重构先行”确定）**：重构先行，clk 以原生形态落在
新核心上，clk/tail 的框架 glue **从不合入主线**。理由：垫片一旦成为主线路径，
会固化"子系统如何接入"的组织记忆，后续 regulator/pinctrl 会照抄范式，重构
届时对抗的不仅是代码还有惯例；且重构对 clk 的需求（defer/命名资源的真实用例）
由分支即可满足，无需合入。执行方式：Phase 0 裁剪为"最小脊柱"——DeviceCtx+
Data、ProbeError::Deferred、ResourceMap 命名条目、RuntimeBackend（可先包现有
全局 provider）、clk service crate——以"kclk 本体 + PL011 消费逻辑从分支移植到
新 API"为验收标准，同时产出第三个 golden 模板（跨设备服务消费者）。风险
对策：脊柱范围硬边界（clk 不需要的不进 Phase 0）；唯一回退触发条件是外部
硬件 bringup 日期早于脊柱完成且硬依赖 clk。clk/tail 分支应 rebase 到当前
main 保持可编译，作为活的需求文档。

---

## 6. 一句话总结

当前框架模仿了 Linux 驱动模型的外观，却漏掉了让它成立的四个承重点
（drvdata、数据表匹配、defer probe、devres 单一所有权），导致每个驱动都要用
全局表和跨 crate 胶水补洞；重构的目标不是"更漂亮的分层"，而是把
**Linux 概念到本框架的映射变成一一对应、数据驱动、单文件局域**的机械变换——
这既是软件工程的收敛，也是 AI 转写可行性的充要条件。


## 7. 实施进度（2026-09-23，分支 xiaohui/driver-core-phase0）

基于主线 `c409a9db` 的 7 个提交，Phase 0 + Phase 1 主体已落地并在 QEMU 实机验证：

| 提交 | 内容 | 评审问题闭环 |
|---|---|---|
| cc9b107b | driver-core 脊柱：typed Data、Deferred+精确唤醒、命名 ResourceMap、可实例化域 | P0-1 / P1-6 / P1-7 / P1-8 |
| 1d61e737 | kclk 入主线 + 设备模型投影（68 测试全过，clk/tail 的 ~2500 行 glue 零合入） | 5.4 节推演兑现 |
| e3f1d5ad | 组合根 + PL011/fixed-clock golden 驱动（defer→发布→同步唤醒实机验证） | P0-4 单文件局域性 |
| 2bfd9d90 | 类层解耦：ClassParent 双变体 + ClassDeviceId（新旧模型同注册表共存） | P2-9 三重视图收敛 |
| ec744ed6 | virtio-blk 迁移：**ACTIVATIONS 全局表删除**，根文件系统从新模型块设备挂载 | P0-1 最大实证 |
| a242b1db | virtio-net 迁移：**NET_IRQ_* 全局路由表删除**，旧模型 glue 同步改为 per-device | P0-2 闭环 |

新模型已驱动的真实硬件：PL011 console（含 clk 消费）、virtio-blk（根 fs I/O
路径）、virtio-net（eth0）。旧消费者（knet / fs_boot / kext4 / tty）无感消费新
模型设备。回归全绿：virtio 21、knet 241、kclk 68、driver-core 4、kdriver 6。

**P0 清单全部闭环，P2 devnode 三项完成**（d8a6ebb1c：DevNodeType 数据注册取代
封闭 enum、block 投影 + /dev/vda 用户态可见、MODALIAS 端到端；uevent 载荷锚点
在回归中实际抓住了一次格式偏差）。

**本轮重构收官**（a1574a004）：契约层清洗完成（ahci/sdmmc/fxmac/ixgbe 等休眠
占位驱动删除，contracts 只剩契约）；virtio-rng 与 ramdisk 迁移为 golden 模板
（rng 被 entropy 无感消费、ramdisk 被 fs_boot 选中）；`make check-crate` 固化
特性包装；**Linux→x-kernel 映射表**（docs/linux-porting-map.md）作为转写规范
交付。AI 转写所需的全部要素齐备：单文件驱动局域性（6 个范本覆盖
platform/PCI/provider/静态/最小/中断所有权六种形态）、数据驱动 id 表、
defer/精确唤醒、guards-in-Data 所有权、类发布 + 用户态投影。

**旧模型已全部移除**（fe25cb0d）：kdriver/kdevice 整体删除，virtio 全家族
（blk/net/gpu/input/vsock/9p/rng/mmio）、PL011、NS16550（MMIO+IOPORT）、ramdisk
均迁至 driver-core；kclass 只剩 detached 发布；kfeat 特性直连新 crate；procfs
消费新域快照。系统只剩一套设备模型。

---

## 8. 实施进度 II：架构债清理与三轮外部评审闭环（截至 35ad14c89）

第 7 节之后的工作分两段：先是架构债的主动清理（rebase 到 c271f4361 后），
随后是三轮外部代码评审驱动的修复。每轮评审都先逐条对照源码核实再动手——
包括核实评审自身的错误（如"msix 是死代码"实为扫描误判、"vendored trait
要求 &mut self"与源码不符），以及确认我们自己引入的问题（见 8.2 的教训记录）。

### 8.1 架构债清理（评审间隙完成）

| 主题 | 提交 | 内容 |
|---|---|---|
| 设备家族知识出核心 | f023c0615 | virtio_pci_transport 从 DeviceServices 移到 ExtensionService（driver-core 零设备家族知识，编译期类型检查不用 Any） |
| 生命周期状态机 | 0951650da / 4711585f6 | Probing→Busy 拒绝移除、未绑定设备轻量移除路径、延迟诊断（finish_probe_phase）、9P 移除缺口、Probing 期间锁外 probe 的竞态拒绝 |
| 类移除完整路径 | f430f7d21 | remove_detached_* 裸摘除 → find_detached + class_remove（回调 + devnode::withdraw + unpublish） |
| uevent 格式/传输分层 | 6ef890c94 | 格式层归 devnode，传输层独立为 uevent-netlink |
| 目录重组 | 06c873276 等 | contracts→subsystems/ + common/，12 个平铺 drv crate→4 个子系统 crate（drv-virtio/serial/clk/block） |
| PCI 层归位 | 057391ddf / 3d77a8391 / dfb7f13a6 | bus/pci 实现 Bus trait 并持有发现管线；删除与 vendored MmioCam 逐字节重复的 PciConfigAccess；总线类型权威反转（PCI marker 归 bus/pci，PlatformBus 留 driver-core——对齐 Linux pci_bus_type 在 drivers/pci、platform_bus_type 在 drivers/base 的分野）；RegisteredDriver::matches 增加 desc.bus == B::ID 硬检查 |
| 文档清扫 | db93e9e5d 等 | 旧框架引用全仓清零 |

### 8.2 三轮外部评审的修复记录

**第一轮（生命周期 + 资源所有权）**：

| 修复 | 提交 | 说明 |
|---|---|---|
| ECAM 唯一 owner | 8865eb3bd | iomap_device 是引用计数幂等映射（评审所述"每次新映射"不准），真实缺陷是引用无主 + 'static 别名无人保活。单一 boot-lifetime owner（Linux pci_remap_iospace 模型）；明确不做 per-handle Drop——transport 的 'static 别名活得比句柄久 |
| DT 唯一 location + 多资源 | 5773dcfc0 | FirmwareNode{id:0} 全体冲突 → 单调 id；FirmwareDevice 单资源 → 全量 reg/interrupt |
| BAR panic → 跳过设备 | 56bdf629f | 外部输入（固件 range/Kconfig）不应击倒 composition root |
| ClassDevice offline | 9e0e1aea7 | is_available() 从硬编码 true 变真值；完整 drain 协议明确留后 |
| request_irq 单一入口 | 2010f81fa | virtio-blk 的 dispatch 旁路（历史路径，非能力差异）删除，request_block_irq 消失 |

**第二轮（owner 错位 + IRQ 链）**——本轮同时暴露了第一轮修复的三个自伤，全部当场
抓住并写入提交说明：

| 修复 | 提交 | 说明 |
|---|---|---|
| class owner 显式化 | 645070a8a | P0：publication id（独立计数器，清 tag 位还原）与 domain id（从 0）错位一位——remove(0) 泄漏、remove(1) 删错设备。DetachedParent 加显式 owner 字段（Linux 用显式 parent 指针不做 id 算术）；**补了 kclass 回归测试**（此前 remove 路径从未被端到端执行过，bug 永不触发是它存活的唯一原因） |
| IRQ 域路由三连修 | a6ae5dcf5 | ① interrupt_descs 读 parent 的 #interrupt-cells（我自己上一轮抄了旧代码的 [3,1] 猜测——PLIC 三条单 cell 中断被吞两条）；② DT/PCI 路由按 IrqSpec 字段契约表示（number=0 + controller + hwirq，request 时过 irq domain——Linux irq_create_of_mapping 时机）；③ adapter 的 with_virq 只在真有 OS-resolved 号时携带。**第一版破坏了注册（MappingConflict）而启动照常过**（console 输出是轮询的），靠 LOG=debug 抓到——QEMU 恒等映射掩盖表示错误的现场版 |
| with_available 门禁 | cc643ae4a | 22 个数据面类转发 offline 后返回 BadState；查询类方法保持 with（读缓存态，无法报错）；teardown 顺序（驱动回调 → 置位）保证驱动自身不受限 |
| reg/interrupt-names | d1fe7306e | of::string_list → push_named；byname 转写目标可达 |
| location id 防回绕 | 40978a333 | u16 → u32 + 耗尽拒绝（唯一性是该 id 的存在理由，静默回绕是自否定） |

**第三轮（纯代码评审）**：

| 修复 | 提交 | 说明 |
|---|---|---|
| 5 个 mmio 路径编译错误 | 2cfc421bc | **我第二轮 owner 脚本的机械替换波及 activate_mmio（无 dev 参数）**——mmio×family 组合从未被编译所以没炸；ns16550 ioport 的 Ok(SerialData) 同类（braced struct 无参构造）。全矩阵编译验证补上 |
| HAL MMIO 映射所有权 | 9e8a390b4 | 评审最有价值的发现：vendored Hal 在 trait 边界丢弃 MmioMapping token，BAR 映射引用永不释放——不是文档说的"HAL 持有"，是泄漏。修法拒绝手工对齐计数（拆东墙）：probe 作用域收集器 → RAII lease 存进 transport 包装 → **映射生命周期 = transport = 最后一个 class clone**。泄漏消除的同时 clone-after-unmap 悬垂窗口一并闭合（unmap 不可能先于最后使用者）；失败 probe 立即释放 |
| drv-serial 去 khal | 443bf97e0 | PhysAddr/VirtAddr 就是 memaddr re-export，import 替换 + 依赖删除 |
| time 服务面化 | 35ad14c89 | rng 的 driver_runtime::time_ops() 逃生舱口（核实为活服务——超时死线源，非一次性种子）→ DeviceServices::time()/DeviceCtx::time()（device-res 纯契约，与已有 driver_base 依赖同类）；rng mmio 变体统一 blk 式 dev 签名。附带：add_device 在 services 未装时响亮报错（NoServices 的 Fatal-Unsupported 不可重试，原为无声陷阱） |

**本轮验证基线**：aarch64 QEMU 启动到 shell（rootfs=新模型 virtio-blk、console=PL011
含 clk 消费）；mmio×{rng,gpu,input,vsock,9p} + ns16550-ioport + x86_64 全部编译通过；
单测 driver-core 10 / virtio 21 / kclass 2 / devnode 2 / sysfs 5 / devfs 18 /
knet 244 / kclk 68 全绿；LOG=debug 确认零 MappingConflict。

### 8.3 明确不修清单（带理由）

以下各项**不是遗漏**，是评审后有意推迟的决定。理由分四类：

**类别 A：需要独立设计，塞进修复轮会做成半吊子**

| 项 | 评审来源 | 理由 |
|---|---|---|
| ClassDevice 完整 drain/quiesce 协议（等待已过 with_available 门禁的在途操作结束、等待所有 clone 释放） | 二轮 #6 | 需要 per-op 完成计数或引用协议设计；当前 offline + 门禁已把"静默错"变"显式错"，剩余窗口（门禁通过后设备被移除）比原来的"永远可用"小一个量级。这是驱动框架最深的一块，值得单独一轮 |
| MSI-X per-vector 暴露 + irq 号穿透 handler 签名 | 二轮 3.2/三轮 3.2 | 三层 handler 签名并存（Fn(usize)/Fn(usize)->IrqEvent/Fn()->IrqAction）是真问题，但改 driver-core 签名牵动全部现有 handler，而 per-vector 消费者（多队列 net/blk）尚未迁移——现在改是纯折腾。与 MSI-X 暴露（映射表 §9 已记录）一起做 |
| threaded IRQ | 三轮 3.3 | provider 层有接口、adapter/driver-core/DeviceCtx 三层里两层完全没有概念——不是"补个方法"是设计决策（worker 模型、优先级、与 reclaimer 的关系），且无消费者 |
| unpublish_block(DeviceNumber) 与其它 unpublish_*(ClassDeviceId) 形状不一致 | 三轮 §6 | gendisk 的 major/minor 是块子系统自己的身份体系（Linux 同样如此），与 class id 是两个概念域。"统一"要么消灭设备号要么引入双 key 查找，是真重设计而非整洁问题。转写块驱动时映射表里写明即可 |

**类别 B：当前正确、改动是加机器不加安全**

| 项 | 评审来源 | 理由 |
|---|---|---|
| Hal::dma_dealloc 重构 DmaSpec 释放（对称性靠约定） | 三轮 2.2 | alloc/dealloc 参数确定性对称（同 pages、同 4K 对齐），今日正确；把 spec stash 进表增加查账机器，收益为零。若 alloc 端对齐策略变化时此约定自然会被编译期/测试暴露 |
| select_batch 深拷贝 / modalias 每次重算 | 三轮 5.2/5.3 | probe 是每设备一次性路径，非热路径；优化需要在 matches() 返回命中条目（API 变化）或缓存——等有真实 probe 吞吐需求再说 |

**类别 C：QEMU 掩盖但修复成本高于当前收益（已记录观察点）**

| 项 | 评审来源 | 理由 |
|---|---|---|
| pl011 handler 恒返回 Handled（共享线场景会吞中断） | 三轮 3.1 | PL011 在所有支持的 DT 上是独占 SPI；修法明确（先查 RIS 再 ack 再报 Handled），等真有共享线平台时一并做——不是不会修，是现在改 console 中断路径的风险大于假设场景的收益 |

**类别 D：外部条件未具备**

| 项 | 评审来源 | 理由 |
|---|---|---|
| runtime PM / suspend-resume | 映射表 §9 | 无消费者，无验证面 |

### 8.3.1 第三轮验证评审的翻案与新增（e743a777e 全部闭环）

第三轮验证评审正确推翻了 8.3 的两个分类，并发现一个新问题，三项已全部修复：

| 项 | 原分类 | 评审裁决 | 修复 |
|---|---|---|---|
| 多 compatible 匹配 | D"外部条件未具备" | **翻案成立**：不是等消费者，是枚举入口主动丢弃固件数据（`compatibles().next()`），真实 DT 上"soc 特化 + 通用 IP"两条拼写的节点会被 Linux 认领、这里 Unclaimed——转写正确性缺口 | PlatformIdentity 携带完整列表（firmware_id 保持首条供 modalias）；OfMatch::matches 改 any 命中；单测钉住第二条命中（e743a777e） |
| feature 矩阵 CI 化 | D"CI 在外部" | **部分翻案**：本地验证本轮已证明可行，缺的只是固化成可重复命令，"外部 CI"不该成为不做的挡箭牌 | scripts/ci/check_feature_matrix.sh（mmio×family + ioport 全组合），CI 侧只需加一个 stage 调它 |
| remove_all_detached 同 owner 只清第一个 | 文档未覆盖（新发现） | 属实：find_detached 首中即返，同 owner 多发布泄漏其余——8.4 教训第 2 条（remove 路径只测过单设备）的又一例，我们自己的测试甚至构造过同 owner 双发布形状 | 各类分支 while-let 排干；ClassRegistry 层 drain 回归测试（e743a777e） |

**e743a777e 自身又留了一处**（第四轮验证评审抓到，第四次 feature 组合教训）：
PlatformIdentity 加必填 compatibles 字段时漏了 ramdisk 静态设备构造点
（`#[cfg(feature = "ramdisk")]` 路径，base 配置不含、旧矩阵也不含）。已补
`compatibles: Vec::new()`（板级静态设备无 compatible，按 alias 认领）。更重要的
机制化：**check_feature_matrix.sh 改为从 drivers/Kconfig 的 KFEAT_DRIVER_*
符号自动派生矩阵**（每加一个驱动符号自动多一行），arch 绑定符号从 Kconfig 的
`depends on ARCH_X86_64` 解析（ioport 双符号正确排除出 aarch64、由 x86 pass 覆盖），
ramdisk_static 用临时 stub 镜像满足其编译期 `include_bytes!(env!(...))` 依赖。
当前全矩阵：21 个 aarch64 组合 + x86_64 pass 全绿。手工枚举的矩阵永远追不上
驱动增长——把覆盖从"人肉记忆"升级为"机制保证"是唯一的根治法。

分类修正（未动代码）：pl011 恒返回 Handled 从 C 类移入**已知正确性债务**（评审指出它不是
"QEMU 掩盖"而是"当前硬件配置下不触发"——独占 SPI 上无害，共享线上会吞中断，修法明确：
先查 RIS 再 ack 再报 Handled）；unpublish_block 的"真重设计"表述修正为**适配层级别**
（外观统一、内部保留 gendisk 设备号体系，工作量远小于 drain 协议，待第一个块设备
转写时一并做）。另记录一个低优先观察点：ECAM owner 的 cam 一致性检查是运行时
log（pci_cam_kind 是 feature 常量，实际不会漂移；类型化 cam 属编译期强化，收益低）。

### 8.4 教训记录（写给后续维护者）

三轮评审暴露的不是随机 bug，是两类系统性失误，均已在发生当场识别并修正流程：

1. **机械修改不做调用面验证**：owner 脚本按"前 40 行找到 dev"替换 17 个调用点，
   其中 5 个目标函数没有 dev 参数——恰好都在从未编译的 feature 组合里。同一模式
   后来又发生三次（ns16550 ioport 的结构构造、compatibles 加字段漏 ramdisk 构造
   点）。对策已机制化：check_feature_matrix.sh 从 Kconfig 自动派生组合，改共享
   struct 的必填字段后跑它即可覆盖全部构造点。
2. **在未验证的地基上施工**：class owner 关联从未正确过，remove 完整路径建在其上
   只验证了编译与启动——remove 路径根本没被执行过。对策：涉及关联/ID 的修复，
   回归测试必须覆盖那条关联链本身（kclass 测试即为此而加）。
3. **QEMU 恒等映射会掩盖表示层错误**（IRQ 三连修的现场）：IRQ 域路由第一版
   破坏注册而启动照常通过。对策：中断类改动必须 LOG=debug 验证注册路径，
   不能只看启动到 shell。

### 8.5 主线 rebase 与 hwrng 语义移植（截至 6cb2b1229）

Rebase 到 main `f178b7a81`（21 个主线提交）。其中 !912（hwrng 设备契约）+ !916
（IRQ 驱动熵收集）写在本分支已删除/重组的旧框架路径上——典型的语义冲突而非文本
冲突。处理：

- **rename 追踪自动携带的部分**：设备层 rng 实现（RngRequest/IRQ 机制）、mock
  增强、entropy 消费者、Kconfig 的"硬件驱动/可信熵源"拆分、`DeviceKind::Hwrng`。
- **手工移植的部分**（6cb2b1229）：hwrng 契约落位 drivers/common（跨子系统契约
  层，与 device-res/driver_base 同类）；kclass 注册 + `with_available` 门禁转发；
  设备构造与 IRQ 注册分离——驱动层走 `DeviceCtx::request_irq` 单一入口而不是
  main 的 provider 直传签名；HostRngCompletion 落在驱动层（无全局逃生舱口）；
  DevIrq guard 由 RngData/mmio activation 持有（撤 IRQ 先于撤发布）。
- **发现 main 自身的同模式问题**：vendored `MmioTransport` 只声明 `Sync` 没有
  `Send`，!916 的 `T: Send` 约束让 main 自己的 mmio+rng 组合编译不过——main
  无人启用 mmio，"从未编译组合"在主线同样发生。我们的处理：`SendMmioTransport`
  包装（SAFETY 论证写明：Sync 已断言跨线程共享，Send 是更弱主张），并由 feature
  矩阵覆盖该组合。
- **验证**：启动日志 `entropy pool initialized (virtio-rng, jitter,
  hardware_ready=true)`（IRQ 驱动链路端到端）；矩阵 21 组合 + x86 全绿；单测
  entropy 33（含移植后的 collector 回归）、virtio 28、driver_core 11。

rebase 期间的教训补充：手工解冲突时一次 `&&` 链把带 3 个冲突标记的 kclass 提交
了进去（grep 计数在链中间、退出码 0 继续走）——下一轮验证评审抓到后 amend 修复。
教训：**解冲突脚本必须以"标记计数为零"为继续的前置条件**，不能靠链式副效应。

### 8.6 第六轮验证评审修复 + 潜伏死锁的发现（含教训升级）

第六轮评审的三项修复（Kconfig 重复符号、rng mmio 空 close、remove_all_detached 缺
hwrng 分支）在修复过程中暴露了一个**更有价值的潜伏 bug**：

**`remove_all_detached` 的 while/if-let 自死锁**（全部 8 个类分支，自诞生起存在）：
Rust 的 while-let/if-let scrutinee 临时量（这里是 registry 的 SpinNoPreempt guard）
存活贯穿整个循环体，体内 `class_remove` 重取同一把非重入锁 → 自死锁。旧 `if let`
版本同样有此 bug——**从未触发只因 remove 路径从未被执行**（8.4 教训第 2 条的第
四个实例，也是最有说服力的一个：不是"测试没覆盖"，是"整条路径零执行"）。新加的
drain 测试是它第一次运行，softlockup backtrace 符号化直接定位到 lib.rs:864 的
lock()。修复：全部分支改 `loop { let Some(id) = ... else { break; }; ... }`——
`let` 语句的临时 guard 在分号处释放。

其余修复：
- **Kconfig 重复符号**：rebase 残留的 `KFEAT_DRIVER_VIRTIO_RNG` 双定义（192/396，
  depends/default 各不相同）合并为一处（保留 main 的拆分说明 + 我们的 QEMU
  default y，删除过时的 depends on KFEAT_CHAR——rng 现在发布 hwrng 类）；
  check_feature_matrix.sh 加重复符号 lint（`uniq -d` 非空即 fail）。
- **rng mmio 清理缺口**：activate 返回 (publication, irq)；mmio close 显式
  `unpublish_hwrng`（与 PCI 移除路径同序：撤发布 → 释 IRQ → 撤映射）；
  remove_all_detached 补 hwrng 分支。
- **drain 回归测试**：向每个无投影副作用的启用类（hwrng + net）发布 sentinel、
  断言 remove_all_detached 排干——新类忘加分支时测试变红。char/display/input
  排除原因记录在测试注释（发布/移除路径带 devnode 投影与 console 切换，需要
  完整组合根，由启动时移除覆盖）。

评审建议的"remove_all_detached 收进 class_api! 宏成对生成"记为中期项：宏展开的
registry 泛型可以生成 drain fn，但 console hook 特例与聚合调用列表仍在——drain
测试已提供等价的回归护栏，宏化等真正加第二个特例时再做。