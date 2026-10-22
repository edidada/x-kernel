# driver-model — 设计文档

## 定位

新驱动设备模型。它拥有 bus/device/driver 三类契约、const id 表、
deferred probe、设备域（`DeviceDomain`）与每设备服务面（`DeviceServices`）。

当前作为独立 workspace member 保留模型与 UT；hwrng 依赖 `driver-class`，entropy 经 hwrng 的封装接口消费能力，不再经此 crate 使用发布能力。host 服务实现、总线枚举和具体
驱动后续通过对应契约接入，旧设备模型仍在迁移中。
`entry` 的 `unittest` feature 显式链接本 crate，保证模型测试没有随着生产依赖拆分而丢失。

它不依赖任何设备家族，也不依赖 `memspace`/`kdma`/`khal`/`kirq`。
IRQ 描述词汇、handler、Op、资源与 token 复用 OS 中立的 `device_res` 契约，
不保留平行的 IRQ provider 定义。`devres::host_api` 仅重新导出这些 IRQ 类型。
现有 `drivers/adapters/xkernel/device-res` 负责与 `kirq` 的转换；model 的
`DeviceResourceServices` 接入该 provider 的生产适配尚未实现，本轮不声称已接通。
`IrqSpec` 保留固件路由/已解析 virq 的区分；适配方在调用 provider 时转换为
`device_res` 描述，不能将 hwirq 当作已完成域映射的 virq。
边界目前依靠代码审查，没有名为 `scripts/ci/check_driver_boundaries.sh` 的检查。

依赖方向为 `driver-model → driver-class`。私有 `DeviceCore` 实现
`PublicationSource`，`Device::publish` 将同一个 `Arc` 身份交给 class。
class 只读身份与在线状态；model 在 remove 回调前标记来源 offline。
`domain` 内部四个引擎仍共享原有状态与锁，没有拆成独立 crate。
发布、drain 和 finalizer 的实现见 [class 设计](../../class/docs/design.md)。

## 职责与拆分边界

model 管设备实例与驱动实现的绑定关系：设备进入模型、选择驱动、probe、
依赖等待与重试、保存驱动私有数据，以及移除时的生命周期编排。
它拥有设备身份、拓扑和绑定状态，不拥有 class 注册表、publication 在途计数
或具体设备家族的数据面接口。

一台设备可以发布多项能力；能力消费者经 class 使用这些能力，无需了解
model 的驱动匹配和 probe 引擎。两套状态机通过同一个活身份协作。
单项 Registration Drop 只撤销该发布，不改变来源或其他发布。设备移除由
model 标记来源 offline，驱动随后 quiesce 并释放注册与资源。

本轮不继续拆分 `domain` 的四个引擎，它们共同维护 `DomainInner` 和同步协议。
资源契约与 `PlatformBus` 暂留本 crate 不代表最终归属；后续迁移顺序和判断依据
见 [base 的职责与停止点](../../README.md#当前停止点与后续工作)。

## 背景

本 crate 是对旧 `kdevice` 的重写，动机是旧模型缺一个 drvdata 槽位：
`DeviceDriver::probe_device(Arc<DeviceObject>)` 不给驱动留私有数据，于是每个
驱动被迫自建全局旁路表（virtio-blk 的 `ACTIVATIONS: BTreeMap<DeviceId,
Box<dyn FnOnce>>`、virtio-net 的 IRQ 号反查表），资源生命周期只能挂进
`Vec<Box<dyn FnOnce()>>` 形式的 devres 闭包黑箱。

新模型把 Linux 的四个承重构件搬过来：`struct platform_driver` → `DriverDesc`、
`dev_get_drvdata` → `DriverOps::Data`、`of_device_id[]` → `&'static [B::IdEntry]`、
`-EPROBE_DEFER` → `ProbeError::Deferred`。完整映射表见 `src/lib.rs` 的 crate
文档与 `docs/linux-porting-map.md`。

## 范围

| 文件 | 职责 |
|---|---|
| `src/lib.rs` | crate 门面 + Linux↔x-kernel 映射表 + 所有权模型说明 |
| `src/bus/mod.rs`, `src/bus/platform_bus.rs` | 总线抽象：`Bus` / `BusId` / `BusInstanceId` / `MatchScore`；参考实现 `PlatformBus` + `OfMatch`/`AliasMatch`（`driver_base` 根上各导出一次，不再有用 `platform::` 二次路径） |
| `src/desc.rs` | 描述符：`DeviceDescriptor`（`ident` 总线数据 + `resources` + `fw` + `dma` + 拓扑） |
| `src/firmware.rs` | 固件节点快照（`FirmwareNodeInfo` / `FirmwareProp`）：设备事实，与总线无关，经 `dev.fw()` 访问 |
| `src/driver.rs` | `DriverOps`（含 `type Data`）与 `DriverDesc`；`declare_driver!`（`module_*_driver()` 的等价物：一行声明 id 表 + ops + 注册入口） |
| `src/device.rs` | 设备本体：`DeviceId`（domain 分配的运行时身份）、`DeviceCore`（活身份卡——id/驱动名/物理路径/offline 的唯一权威归宿，`add_device` 创建、Arc 共享给类发布）、`Device`（**唯一**设备句柄：probe 收 `&mut Device<'_, B>`，remove 收同一句柄 + `&mut Data`；`publish(&CLASS, …)` 即从它取 core） |
| `src/devres/services.rs` | 设备侧服务契约：`DeviceResourceServices` / `ClockConsumerServices` / `ProviderServices` / `DeviceServices` / `ExtensionService` / `ServiceError` |
| `src/devres/` | 一根 devres 轴：驱动侧句柄 `iomap.rs`（`IoMap`）/ `irq.rs`（`DevIrq` + `IrqAction`/`IrqHandlerFn`）/ `dma.rs`（`CoherentMem`）/ `clk.rs`（`ClkHandle` / `EnabledClock`），`host_api.rs` 为 host 侧 Op 契约（`#[doc(hidden)]`），`services.rs` 是两者的缝合面 |
| `src/error.rs` | `ProbeError`（Deferred/Fatal）、`DepSource`/`DepKind`、`RuntimeError`、`ServiceError` |
| `src/resource.rs` | `ResourceMap`（命名资源表）、`ResourceKind`、`MmioRegion`、`IoPortRange`、`IrqSpec`、`ClkSpec`、`Virq`（OS 可见中断号，与固件 hwirq 结构性区分） |
| `src/provider.rs` | provider 注册惯例（**唯一一条**）：`ProviderClass`（一个 provider 家族 = key/lookup/`DepKind` 描述符）、`ProviderRegistration`（类型擦除的一次发布）、`ProviderServices`（host 侧落点） |
| `src/extension.rs` | 家族扩展：按类型注册/查找（`ExtensionRegistry` + `ExtensionService` + `ExtensionServices`），替掉 `DeviceServices::as_any` |
| `driver_class`（独立 crate） | `Class<T,H>`（registry + hooks）+ `Registration<T>`（注册权）+ `Publication<T>`（消费者引用）+ `InUse<T>`（在途计数 guard）；投影与撤销由 class hooks 管理。`Class::attach_legacy` 是过渡门：仅供仍在旧设备模型上 probe 的驱动（virtio 家族迁移期）以合成身份发布，随最后一个遗留发布者一起删除 |
| `src/devres/host_api.rs` | 主机实现的 Op 契约（每资源家族一个 vtable）：`MmioOp`/`IrqOp`/`DmaOp`/`ClkOps` + `IrqRouteDesc`/`MsiResource`；host 面，`drv/**` 不得命名 |
| `src/domain/mod.rs` | `DeviceDomain` 门面：一把锁、entry 的四个状态域、`global_domain()`（唯一的进程级域名入口，组合根与 `declare_driver!` 之外不应出现） |
| `src/domain/registry.rs` | 注册引擎：device/driver/bus-instance 表、entry 类型、add 时校验、快照、拓扑链接 |
| `src/domain/probe.rs` | 探测引擎：批选择、锁外执行、generation 守卫的提交 |
| `src/domain/dependency.rs` | deferred 等待器：park、精确 wake（含 late-provider 重扫）、丢唤醒 generation 守卫 |
| `src/domain/lifecycle.rs` | 移除编排：独占声明、children-first 级联、teardown 分相（`remove()` → drop `Data`；没有观察者/通知旁路） |
| `ut-tests/domain.rs` | 模型单测（`MockServices` 上运行），含 `dev.provide` provider 门与 defer/wake 链路 |
| `ut-tests/domain.rs` 中的 publication 契约测试 | 模型测试身份构造与跨 crate 发布生命周期验证 |
| `ut-tests/firmware.rs`、`ut-tests/platform_bus.rs` | firmware 与 platform bus 单测 |

## 架构

一把锁，四个引擎。所有状态在一个 `SpinNoPreempt<DomainInner>` 下；
"引擎"是模块边界，各自拥有自己的状态域和逻辑，**没有一个拥有锁**：

```text
                    DeviceDomain  (薄门面：持锁 + 排序跨引擎动作)
                             │
        ┌────────────┬───────┴────────┬─────────────────┐
        ▼            ▼                ▼                 ▼
    registry      probe          dependency        lifecycle
   （三张表 +    （批选择 →      （park / wake /   （独占声明 →
    entry 的      锁外执行 →      generation 守卫   children-first
    四个状态域）   守卫式提交）     / late provider）  级联 → teardown）

    DomainInner { bus_instances, drivers, devices, sched, services,
                  next_device_id, services_installed }

    DeviceEntry { desc          ← 身份（不可变）
                  lifecycle     ← 状态机 + generation + remove_retryable
                  binding       ← driver / Data / attempted
                  topology      ← children
                  probe         ← last_deferred_dep / dep_seen_generations }
```

跨引擎的顺序（probe ↔ remove ↔ wake）只在 `DeviceDomain` 门面里出现一次。

`DeviceDomain` **可实例化**：crate 内没有进程级注册表，`global_domain()` 只是
组合根用的那一个 LazyInit 实例，测试各自 `DeviceDomain::new()`。设备 id 从
crate 级 `DEVICE_ID_ALLOCATOR` 取，因此多个域同时存活时 id 仍全局唯一——
以 owner id 为键的外部注册表（class 发布、devnode）不会被跨域误撤。

## 调用约束 / 执行上下文

- 域锁是 `kspin::SpinNoPreempt`：**禁抢占、不关中断**。因此 `add_device` /
  `remove_device` / `probe_pending` / `wake` / 各类快照都**不得在硬中断上下文
  调用**——持锁期间来中断会在同一把锁上自死锁。
- **驱动代码在域锁外运行**（`probe.rs` 的 `select_batch` 之后释放锁），所以
  `DriverOps::probe` / `remove` 可以睡眠、可以回调域、可以做慢速 MMIO。
- `remove_device` 的 teardown 相（`DriverOps::remove` → drop `Data`）
  同样在锁外，且按构造运行在任务上下文——这是各驱动 `Drop` 里
  `block_wait::prepare_block_wait()` 之类可睡眠调用的前提。
- 没有 lifecycle observer / remove notifier：域状态机是移除的唯一权威，
  发布回收由 `Data` 持有的 `Registration` 完成。类发布的 `ClassHooks::unpublish`
  （`del_gendisk`、transport close）在仍有在途 `InUse` 时推迟到最后一个许可 drop
  ——见 `../class/src/publication.rs` 的 `finish_or_defer`。
- `global_domain()` 可在早期启动调用（`LazyInit::call_once`）；但
  `set_services` 之前所有资源获取都落到 `NoServices`，返回
  `RuntimeError::Unsupported`。这是刻意的"响亮失败"，不是静默降级。
- `IoMap` 的访问器带边界与对齐检查，越界/未对齐**总是 panic**（不是
  `debug_assert`）——release 下同样是 panic，因为 MMIO 上的未对齐 volatile
  访问是 UB。

整条跨 crate 链路（枚举 → probe → 发布 → quiesce → 释放）只在
`docs/device-life.md` 描述一次；本文件只讲这一站。

## 公开名词预算

驱动面只允许出现这些名词，其余都是实现细节：`Device`、`Driver`/`DriverDesc`、
`Bus`、`Resource`（devm guard 族）、`Class`（发布）、`Provider`（家族描述符）、
`Host`（进程级根，driver-host 侧）。新增一个公共名词要说明它归并/删除了哪个既有
名词；只有单个使用者的抽象不进公共面。

## 状态机

`DeviceState`，7 态：

```text
                add_device
                     │
                     ▼
   ┌────────────  Pending ◄──────────────────────┐
   │                 │ select_batch              │ wake(dep) 命中
   │        ┌────────┴─────────┐                 │
   │        ▼                  ▼                 │
   │   无候选驱动            Probing ────────────┼──────┐
   │        │                  │                 │      │
   │        ▼         ┌────────┼────────┐        │      │
   │   Unclaimed ─────┘        │        │        │      │
   │   （late driver /          ▼        ▼        │      │
   │     late provider       Ok(Data)  Deferred ──┘      │
   │     可再次唤醒）           │        │               │
   │                           ▼        └── Fatal ──────┘
   │                        Active          （记入 attempted，
   │                           │              同轮换下一个候选）
   │        remove_device      │
   │      ┌────────────────────┴──────────────┐
   │      ▼                                   ▼
   └─  Removing ──（级联子设备成功）──►   Removed
         │
         └─（子设备 Busy/BadState）──► 停在 Removing，
                                        remove_retryable = true，
                                        下一个调用者可独占续做
```

要点：

- `Probing` 中的设备**拒绝移除**（返回 `RuntimeError::Busy`）。probe 正在锁外
  产生副作用（class 发布、IRQ 注册、provider 发布），此时摘掉 entry 会让这些
  副作用失去归属者。
- `Removing` 有两种：正在进行的（`remove_retryable == false`，并发移除者看到
  `Busy`）与级联中断后可续做的（`true`，**恰好一个**调用者能在锁内翻回 false）。
  这是 Linux `begin_removing` 独占声明的等价物。
- `Unclaimed` 不是终态：后注册的驱动（`late_driver_registration_*` 测试）与
  后发布的 provider（`late_provider_wakes_unclaimed_device` 测试）都能把它捞回来。
  `ProbeState::last_deferred_dep` 因此要跨 `finish_probe_phase` → Unclaimed 保留。
- `Removed` 的 entry 保留供拓扑查询；`Removing`/`Removed` 上再次 remove 返回
  `BadState`。

## 算法流程

### 探测（`probe_pending`）

1. `probing` 标志做泵级串行化：已有泵在跑就直接返回默认 `ProbeStats`
   （它的批次覆盖了此前入队的设备）。`ProbePumpGuard` 在 panic 展开时也释放标志。
   这同时保护了主机侧的单槽资源（MMIO lease collector 断言串行）。
2. `select_batch`（持锁）：取走 `sched.pending`，对每个 entry 选出**未永久失败
   且得分最高**的驱动（`MatchScore`：`Exact > Generic > Fallback > NoMatch`；
   先比 `desc.bus == B::ID` 再比 `B::accepts`，一条总线的驱动永远看不到另一条
   总线的设备）；置 `Probing`；无候选则置 `Unclaimed`。
3. 锁外跑 `driver.probe(&desc, id, services)`。probe 前先记下 entry 的
   `lifecycle.generation`。
4. 提交（重新持锁）：
   - `Ok(data)` 且 generation 未变 → `Active`，`Data` 由域持有；
   - `Ok(data)` 但 generation 变了（设备被拆过又重加）→ **drop 这个陈旧 Data**
     并告警，绝不写进 entry；
   - `Err(Deferred(dep))` → `park_deferred`；
   - `Err(Fatal)` → 驱动进 `attempted`，同一轮的下次 `select_batch` 换候选。
5. 循环到 `select_batch` 返回空。

### 依赖等待（`dependency.rs`）

- `wake(dep)` 对每个 `DepSource` 维护一个 generation，**即使没有设备 parked 也
  自增**。设备提交 `Deferred(dep)` 时若发现该 dep 的 generation 已经前进，就
  重新入队探测而不是 park——这关闭了"检查依赖 → 提交 Deferred"之间 provider
  恰好注册的丢唤醒窗口（`wake_during_probe_window_does_not_lose_the_device`）。
- `wake` 还会重扫 `Unclaimed`（late provider 场景）。
- `finish_probe_phase(false)` 在探测阶段结束时把仍 parked 的设备报告出来并降级
  为 `Unclaimed`，而不是让它们静默停在那里。

### 移除（`lifecycle.rs`）

三相，**先关拓扑入口**：

1. 持锁：校验、翻 `Removing`（此后 `add_device` 不能再挂子设备，所以下面的
   级联快照不会看到之后新增的 entry）、`generation += 1`、快照 teardown 集合
   （device children ∪ 该设备桥接的 bus instance 成员）。**此时还不取 driver/Data。**
2. 锁外：children-first 级联。任一子设备失败即中止——父设备停在 `Removing`、
   `remove_retryable = true`、binding 完整，错误返回给调用者重试。
3. 重新持锁（只有这个流程会把该 id 从 `Removing` 往前推）：取出 driver/Data，
   然后锁外跑 teardown：`DriverOps::remove()`（quiesce）→ drop
   `Data`（类发布在其中撤销，guards 按字段序释放；没有观察者/通知旁路）。

未绑定过的设备（Pending/Deferred/Probing 之外/Unclaimed）走轻路径：从
pending/deferred 工作表摘掉、直接释放 entry。

## 并发模型

- **一把 `SpinNoPreempt`**。check-then-act 协议的原子性（add 时的父设备校验、
  批选择、独占移除声明）是**域不变量**，不是靠调用者纪律。旧模型用 5 类锁 +
  一段手写锁序文档来得到同样的性质。
- 驱动代码全部在锁外，所以锁内没有 MMIO、没有分配、没有回调。
- `probing: SpinNoPreempt<bool>` 是独立的泵级标志，不与域锁嵌套获取。
- `DEVICE_ID_ALLOCATOR` 是 crate 级 `AtomicU64`，`fetch_add`，跨域唯一。
- 已知代价（记录在 `docs/driver-framework-review.md` §8.3 类别 B）：
  `select_batch` 对每个待探测设备深拷贝 `DeviceDescriptor`（含
  `Vec<String>` compatibles），`modalias` 每次重算。probe 是每设备一次性路径，
  不是热路径；优化需要改 `matches()` 的返回形状，等有真实吞吐需求再做。
- class 注册表（`driver-class` 的私有 `ClassRegistry`）仍是 `Vec` + 线性扫描，与本 crate 无关
  但同样是已知的、被接受的小集合线性扫描。

## 设计决策

### 类型化 `Data` 而不是 devres 闭包表

`DriverOps::Data: Send + Sync + 'static`，probe 返回它、框架装箱持有、remove
以 `&mut` 交回、remove 返回后 drop。于是"probe 失败回滚"就是"drop 局部值"，
"设备移除"就是"先 quiesce 再 drop"，两者都由 Rust 的所有权保证，不需要一张
`Vec<Box<dyn FnOnce()>>`。

**被拒绝的替代方案**：保留 devres 闭包表并加类型化 drvdata。拒绝理由：两套
并行的清理体系（闭包表 + Data drop）之间的顺序无法在类型里表达，而这正是旧
模型出问题的地方。

### `IoMap::new` 是 `unsafe fn`

```rust
pub unsafe fn new(vaddr: usize, region: MmioRegion, release: Option<ReleaseHook>) -> Self
```

一个 safe 构造器会让任意 `usize` 到达 volatile 访问器——那是 soundness 洞，
不是使用不便。全树只有两个调用点：`driver-host`（provider 刚映射完）和
`ut-tests/domain.rs` 的 `MockServices`。

### 一把锁，而不是分层锁

见"并发模型"。取舍是：更粗的锁粒度换"协议原子性由结构保证"。因为驱动代码
一律在锁外，锁内只做表操作，实测启动期 56 个描述符的枚举 + 探测没有可观测的
争用。

### `Bus` 是开放 trait，`IdEntry` 是关联类型

```rust
pub trait Bus: Sized + Send + Sync + 'static {
    type IdEntry: IdMatchEntry + Sync;
    const ID: BusId;
    fn accepts(desc: &DeviceDescriptor) -> bool;
}
```

新总线在自己的 crate 里实现，`driver-model` 不枚举任何东西。旧模型的
`BusTypeId` 虽然是 `&'static str` newtype，但 `BusTypeObject::new_builtin` 对
非 PCI/PLATFORM 直接 panic，匹配策略硬编码在模型 crate 里。

### 扩展按类型注册/查找，而不是按 host 类型 downcast

```rust
// host 安装时注册一次（boot-static）：
extensions.register(Box::leak(Box::new(PciTransportService::new(&provider))));
// 驱动按类型取：
let Some(service) = dev.env().extension::<PciTransportService>() else { … };
```

家族 transport 需要超出能力契约的主机机制时，扩展自己是**一个值**，按类型
住在 host 的 `ExtensionRegistry` 里；core 不认识家族，host 不再需要
`as_any`，驱动写错的类型就是"没注册"。调用方仍然命名
`driver_host::virtio_transport::PciTransportService`（E-02a），那是"驱动的
家族知识"本身，不是框架的泄漏。
登记为例外 E-02。

### `IrqAction` 没有 `WakeThread` 变体

`device_res::IrqOp` 已有 threaded IRQ 契约，但当前 X-Kernel resource adapter
仍使用返回 `Unsupported` 的默认实现；model 的 `DeviceResourceServices` 与
`Device` 也尚未暴露该能力。当前 `IrqAction` 只表达
同步 handler 是否处理了中断，不得将唤醒线程请求静默降级为 `Handled`。
未来应连同线程注册、退出同步和驱动侧入口一起接入。

### 驱动身份是注册键，不是对象地址

`DriverKey` 是域内单调分配、永不复用的注册序号，`BindingState::attempted`（设备
的永久失败历史）以它为键。身份安全性因此不依赖"驱动永生"假设：地址只能作
生命周期限定的对象借用，一旦允许注销，地址复用（ABA）会把一次失败注册静默
别名到它的继任者身上；键不复用使历史里的死键无害。键是域内的——`attempted`
和注册表都是域内部簿记，不像 `DeviceId` 因 owner-keyed 全局注册表而必须全局
唯一。

驱动注册按（总线类型， 名字）去重并拒绝（Linux `driver_register` 对同一情形
返回 `-EBUSY`）：同一 probe 体出现两个键，一次 Fatal 失败会留下仍匹配的孪生
键，让每个设备把同一个 probe 跑两遍；per 总线名字唯一同时保证 `DeviceCore`
驱动名身份卡无歧义。同分 tie-break：`select_batch` 用 `max_by_key` 在注册序
Vec 上取最高分，平分时后注册者胜——这是显式决定，由
`same_score_later_registration_wins` 钉住。

## Drop / 资源释放

**约定：所有 guard 存进 `Data`，字段声明顺序就是释放顺序**（Rust 按声明顺序
drop 字段）。因此中断 guard 要声明在它服务的 MMIO 映射**之前**。

```rust
pub struct Pl011Data {
    _registration: Option<Registration<SerialRuntime>>,  // 先撤可见性
    _irq: Option<DevIrq>,                               // 再停中断
    _port: Arc<SerialPort>,
    _regs: Option<IoMap>,                               // 再解映射
    _clk: Option<ClkHandle>,                            // 最后放时钟
    _enabled_clk: Option<EnabledClock>,
}
```

RAII **不替代**显式 quiesce：`DriverOps::remove` 是驱动停硬件（drain I/O、
mask 设备侧中断、停 worker）的窗口，它在 guards 还活着的时候运行，返回之后
框架才 drop `Data`。

域侧的释放责任：

- probe 提交时发现 generation 变了 → 就地 drop 陈旧 `Data`，guards 对着创建
  它们的 services 释放；
- 移除路径 → `remove()` 先跑，然后 drop `Data`，然后才把 entry 置 `Removed`；
- 轻路径（未绑定设备）→ 没有 `Data`，只清工作表和 entry。

`ReleaseHook = Arc<dyn Fn() + Send + Sync>` 是每个 guard 携带的延迟释放钩子，
由 `driver-host` 在构造 guard 时注入（例如 `move || provider.unmap_mmio(mapping)`）。
`driver-model` 自己从不实现释放动作。

## 注册与资源清理约束

Device::publish 返回不可克隆的 Registration；publication() 返回不撤销注册的
消费者引用。remove_device 在调用驱动 remove 之前使 DeviceCore offline，拒绝
新使用；remove 必须停止并等待已获准操作，或者把所需资源保留在共享 runtime 中。
Registration Drop 只延迟 class hook，不阻塞 Data 的后续字段析构。
当前 model 不持有所有 class 注册的 drain/join 集合，不能声称字段顺序自动保证
MMIO、IRQ、DMA 生命周期安全。具体资源析构不得先于依赖它们的操作退出。

Device::publish 仅接收 Class 与 runtime，不接收发布名。通用 class 允许同源多项
独立注册；这不改变 model 自己的驱动注册去重或 provider 的业务键检查。
