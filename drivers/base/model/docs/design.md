# driver-model — 设计文档

## R1-04 驱动身份与发布来源

注册入口消费 DriverDesc，将只读 DriverIdentity（唯一驱动名）与 ids/ops 分开。
RegisteredDriver、设备当前关联和 BindingSource 共享身份引用，不重复缓存名称。
DeviceAccess 借用当次回调的身份，publication_source() 返回只读来源：设备引用
提供原有 path，身份引用提供该次发布的驱动名。来源不持有 ops、Data 或硬件资源；
A 解绑后换到 B，设备当前关联为 B，而旧发布仍解释为 A。同驱动重绑也重新注册
provider，不把旧消费者转接新 runtime。来源 online 仅表示设备注册，不是操作许可。

probe 失败由驱动先停运行活动并显式注销已发布能力，再释放资源、返回错误；
模型不会对失败 probe 调 remove。成功 probe 若提交校验失败，模型先调用 remove
再销毁返回的 Data，已有 generation 失配 UT 覆盖这个防御分支。正常管理锁串行下
它不是一个可自然制造的并发卸载窗口，不新增公共故障注入接口。

kdriver 外置测试组合模型与 hwrng，使用真实管理锁和 RngDrain、模拟运行对象/资源，
验证发布前后失败、A/B 与同驱动重绑、多设备隔离、身份不保活 ops、最后引用
不重复退出。硬件 IRQ/DMA stop 和真实 MMIO 接入仍属于后续票。

已批准目标：[驱动模型 R1](../../../../docs/ai-collaboration/driver-model/design-proposal.md)。
该目标正在分票实施；除明确标注的实施进度外，本文描述当前代码。
早期[生命周期方案](binding-lifecycle-proposal.md)仅作历史调查记录。

## R1-01 每设备管理与注册

`services.rs` 的 `DeviceManagementLock` 是 model 所需的宿主能力，不是
驱动可用的通用同步 API。每个设备在发布前需要独立实例，宿主实现位于
`kdriver::device_management`，model 不依赖 ktask 或 ksync。

`run_locked` 接受借用的回调；成功时同步执行恰好一次，返回前释放宿主局部
mutex guard。非法上下文返回 `DeviceManagementError::InvalidContext`，
同任务重入返回 `Reentrant`，两种错误都不得执行回调。其他任务竞争应等待，
不能因竞争返回 Busy 后让调用者跳过必要退出。mutex 的竞争等待仍可能分配，
本合同不承诺端到端无分配。

回调不能带着改变后的 IRQ/preempt/RCU 上下文返回，不能等待需要同锁的工作。
模型在获取该锁后仍须检查设备是否注册；锁本身不是注册凭证或硬件访问许可。
`DeviceCore.management` 保存每设备锁，`DeviceCore.domain` 保存域身份 `Arc<()>`。
`add_device` 先在域锁外调用 `DeviceServices::create_management_lock`，再在
`run_locked` 内校验拓扑并入表。工厂或管理入口失败时不发布设备。
每个 entry 保存注册时的 `services` bundle；后续 `set_services` 只影响新注册。
bind、unbind、remove 和 `with_data` 均通过该设备的管理锁执行。

当前已接入模型生命周期与真实宿主锁测试。kdriver 的下游测试控制器以每次绑定
独立的枚举门禁和 owned 集合验证退出：关闭入口并取走 owned 集合后释放门禁，
再退出孩子，最后停止父服务。孩子退出使用的父事务锁独立于枚举门禁；父方
不能持事务锁等待孩子。已开始的枚举提交被纳入退出，重绑也不重新开放旧门禁。
这是总线所有权协议的测试实现，不是 model 新增的递归退出机制或真实总线迁移。
旧 class 接入仍未完全退役；绑定来源已固定为原驱动身份，但 unbind 保留设备在线，
不能据此推导旧发布已被统一撤销或排空。

## R1-02 探测交接验收

R1-01 移除全局 probe owner 后，认领单位是域锁内取走的 pending 批次，
不是一个跨回调存活的全局执行权。入队与取走批次使用同一短锁：入队在前，
当前或其他分派者取得它；判空在前，生产者自己的同步分派取得它。不再有
“观察全局 owner 忙而只入队，owner 却已经准备退出”的交接窗口。
Fatal 和代次变化导致的重试由当前分派循环继续处理，不新增 worker/executor。

依赖通知即使没有 Deferred 等待者，也更新该依赖的代次。probe 检查依赖后
到返回 Deferred 前发生通知时，提交阶段比较代次并重试；已经停车则由 wake
取出相应等待者。无关依赖不能触发该设备重试，普通 Fatal 仍记入 attempted。
已出队候选不是绑定许可：取得设备管理锁后复核注册和 Pending，注销对象
返回 NotFound，已由显式 bind 激活的对象不再 probe。

`ut-tests/probe_progress.rs` 通过真实模型入口验证这三个通知时机、判空后的
wake/注册、出队后的注销/显式绑定。每域一次性 `dispatch_hook_fn` 只在
`cfg(unittest)` 中存在，取走批次之后在域锁外调用；用精确插入操作模拟交错，
不依赖 sleep、调度次数或墙钟。已有 kdriver 真宿主并发用例继续验证管理锁。
非法上下文或同锁重入被拒绝后不会新增后台重试：调用方须在合法上下文
重新调用 probe_pending。同步分派可以等待候选的管理锁；这不是无等待 API，
总线仍须遵守跨设备锁序，不能等待反向依赖本设备锁的任务。

## 设备模块组织

`device/` 按设备描述、注册对象和驱动访问聚合；内部子模块不作为公开路径。
`DeviceDescriptor`、`DeviceRef` 和 `DeviceAccess` 统一从 crate 根导出，原 `desc`
模块入口移除。描述符只保存枚举事实；绑定资源与状态机仍归 domain。
宿主服务契约、通用资源类型和固件属性类型保持原有模块归属。
描述符 UT 仍在 `ut-tests/desc.rs`，不随实现搬进 src。

## 回调视图的借用关系

驱动回调显式接收两个入口：`probe(dev, env)` 与 `remove(dev, env, data)`。
`DeviceAccess` 只提供设备身份、信息、固件、资源和 class 发布接口；
它不拥有设备状态或返回的资源句柄。`Environment` 位于 `environment.rs`，
不携带设备身份或总线类型，只提供 host provider 注册和扩展查询。
两者借用同一宿主 DeviceServices 实现，不增加新的宿主聚合契约。
原 `Device` 名称和 `env()` 入口移除；延迟探测直接构造 `ProbeError::Deferred`。
provider 注册不记录设备来源，不返回撤销句柄，也不随 remove 自动清理；
保持现有静态 lookup 和唤醒语义，不把它描述为设备拥有的 class publication。

`DeviceCore` 是设备描述与身份的唯一存储。回调适配层持有 `DeviceRef`，
`DeviceAccess<'a, B>` 借用该引用与宿主服务，不另存 descriptor 引用。
构造时从同一描述符 downcast 出类型化 bus_data 借用；不允许调用方单独
传入其他设备的总线数据，类型不匹配则构造失败。
`DeviceInfo` 仅借用 DeviceAccess，查询不克隆 Arc，也不能脱离 DeviceAccess 的借用期。
需要跨回调保留身份必须显式调用 `reference()`；class 发布仍持有同一核心。
modalias 保持回调期借用：probe 提供总线派生值，remove 保持 None。
资源、固件视图均读取同一 DeviceRef 的描述符，不持有独立事实副本。

## 总线实例归属

`DeviceDescriptor` 必须携带一个已经注册的 `BusInstanceId`，不再独立保存
`BusId`，也不允许省略实例。总线类型的权威来源是 `BusInstanceEntry`。
平台初始化先创建根实例（包括 platform 逻辑实例）；控制器创建其提供的
下游实例。枚举方沿扫描上下文把实例传给设备描述，不按类型或名称猜测实例。
控制器自己所属的上游实例与它提供的下游实例是两个不同的关系。

`add_device` 在域锁内验证实例存在，并核对 `BusDeviceData::bus_id()` 与
实例类型一致。数据携带的类型用于验证词汇，不是第二个可独立配置的总线归属。
描述符的 `bus_data` 是枚举方提供的总线专属设备描述，不是 probe 返回的
`DriverOps::Data`；后者持有驱动运行状态和资源。
驱动候选筛选和设备快照从实例取得类型；具体驱动匹配仍保留数据 downcast 检查。
`parent` 继续表示设备父子拓扑，不能由实例归属替代。

实例记录随 domain 保留，以支持实例身份查询；记录存在不代表控制器可用。
注册成员时同时检查控制器状态；控制器进入 `Removing` 后，不论新设备是否填写
`parent`，都拒绝向其下游实例挂载。检查和插入与移除状态迁移共用域锁，确保
进入 teardown 后不会再接受新成员。控制器不存在或已移除时也拒绝挂载，
返回 `BusInstanceUnavailable`；保留的实例记录只承担身份查询，不复活硬件。
本次接口迁移的调用点均在 model UT；旧驱动资源链路和 legacy class 接入不变。

## 可选 DMA 配置

`DeviceDescriptor::dma` 为 `Option<DmaInfo>`，默认 `None`，表示 DMA 尚未配置，
不代表对硬件能力作否定判断。枚举方或平台代码确认设备的地址与一致性约束后，
通过 builder 的 `dma(info)` 或 `dma_opt(option)` 显式配置；不因缺少固件属性
而自动采用 `DmaInfo::DEFAULT`。
`Resources::alloc_coherent` 在未配置时返回 `Fatal(Unsupported)`，不调用宿主；
已配置时宿主通过借用的设备上下文读取原始 `DmaInfo`，保留地址转换、mask 校验和错误传播语义。
MMIO、IRQ、时钟等非 DMA 能力不受影响。os-api 的 `DmaInfo` 持有经过验证的
`DmaWindow`，替代不能表达范围的 offset；model 的宿主服务接收 `DeviceContext`，
不再单独传 DMA 副本。宿主必须校验完整分配区间，不仅校验起点。

## 描述符构造接口

`DeviceDescriptor` 使用 workspace 依赖 `typed-builder`，只提供一套
`builder() ... build()` 构造接口，不保留旧的 `new` 和 `with_*` 方法。
`bus_instance`、`bus_data` 必填；资源集合默认空，`parent`、`fw`、`dma` 默认
`None`。可选字段提供接收值的 setter 和接收 `Option` 的 `_opt` setter，
方便枚举方传入条件解析结果而不暴露 builder 的泛型状态类型。

builder 通过类型状态检查缺少或重复设置字段，按所有权移动总线数据，不要求
`Box<dyn BusDeviceData>` 实现 `Clone`。它不检查实例是否存在、父设备是否可用，
也不初始化硬件；这些动态约束仍在 domain 的注册边界验证。描述符字段仍然公开，
builder 不是不可绕过的安全边界，也不禁止构造后的字段修改。

相邻的 `DriverDesc::new`、`DmaSpec::new/with_direction` 保留：前者是三个必填
参数的简单构造，后者只有一个可选配置且支持 const 构造。资源所有者句柄的
构造函数承担资源与释放协议的关联，不为统一外观改成 builder。

## 定位

新驱动设备模型。它拥有 bus/device/driver 三类契约、const id 表、
deferred probe、设备域（`DeviceDomain`）与每设备服务面（`DeviceServices`）。

当前作为独立 workspace member 保留模型与 UT；hwrng 依赖 `driver-class`，entropy 经 hwrng 的封装接口消费能力，不再经此 crate 使用发布能力。host 服务实现、总线枚举和具体
驱动后续通过对应契约接入，旧设备模型仍在迁移中。
`entry` 的 `unittest` feature 显式链接本 crate，保证模型测试没有随着生产依赖拆分而丢失。

它不依赖任何设备家族，也不依赖 `memspace`/`kdma`/`khal`/`kirq`。
OS 能力契约、句柄和纯资源描述现位于 `drivers/base/os-api`。
model 通过 driver-os-api 使用 irq-types 的基础 IRQ 词汇，不依赖 kirq 或旧 device_res。
新旧资源模型暂时并存，本轮不迁移旧调用方。生产宿主服务仍待通过契约接入。
model 拥有带借用设备上下文的设备服务、deferred probe、ResourceMap 以及错误转换。
宿主适配方实现 OS Op；普通 IRQ 回调通过 IrqHandlerFn / IrqAction 交接，不暴露内核 handler。
边界目前依靠代码审查，没有专门的自动边界检查脚本。

依赖方向为 `driver-model → driver-class` 和 `driver-model → driver-os-api`。
`DeviceAccess::publication_source` 构造 BindingSource，保留共享设备与当次驱动身份；
旧 `publish` 入口也使用该来源，不再读取设备后来绑定的驱动名。
model 在注销的 remove 回调前标记设备 offline，普通 unbind 保持设备在线；
家族操作准入和同步排空由子系统负责，不属于只读来源。
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
| `src/device/descriptor.rs` | 描述符：`DeviceDescriptor`（`bus_data` 总线数据 + `resources` + `fw` + `dma` + 拓扑） |
| `src/firmware.rs` | 固件节点快照（`FirmwareNodeInfo` / `FirmwareProp`）：设备事实，与总线无关，经 `dev.fw()` 访问 |
| `src/driver.rs` | `DriverOps`（含 `type Data`）与 `DriverDesc`；`declare_driver!`（`module_*_driver()` 的等价物：一行声明 id 表 + ops + 注册入口） |
| `src/device/mod.rs` | `DeviceCore` 持有注册描述符与共享状态；`DeviceRef` 管理同一对象的共享引用 |
| `src/device/access.rs` | `DeviceAccess` 与 info/res/fw 视图：回调期间的设备访问、资源申请和 class 发布 |
| `src/device/source.rs` | `BindingSource`：共享设备事实与固定驱动身份，不保活 ops/Data |
| `src/environment.rs` | 不关联设备身份的框架环境：host provider 注册与扩展查询 |
| `src/services.rs` | 设备侧服务契约：`DeviceResourceServices` / `ClockConsumerServices` / `ProviderServices` / `DeviceServices` / `ExtensionService` / `ServiceError` |
| `../os-api/src/{mmio,irq,dma,clock}.rs` | 已抽出的 OS 能力描述、Op 与句柄，按能力与生命周期聚合；不属于 model 实现 |
| `src/error.rs` | `ProbeError`（Deferred/Fatal）、`DepSource`/`DepKind`、`RuntimeError`、`ServiceError` |
| `src/resource.rs` | 设备枚举集合 ResourceMap / ResourceEntry / ResourceKind；纯描述类型从 driver-os-api 引用 |
| `src/provider.rs` | provider 注册惯例（**唯一一条**）：`ProviderClass`（一个 provider 家族 = key/lookup/`DepKind` 描述符）、`ProviderRegistration`（类型擦除的一次发布）、`ProviderServices`（host 侧落点） |
| `src/extension.rs` | 家族扩展：按类型注册/查找（`ExtensionRegistry` + `ExtensionService` + `ExtensionServices`），替掉 `DeviceServices::as_any` |
| `driver_class`（独立 crate） | `Class<T,H>`（registry + hooks）+ `Registration<T>`（注册权）+ `Publication<T>`（消费者引用）+ `InUse<T>`（在途计数 guard）；投影与撤销由 class hooks 管理。`Class::attach_legacy` 是过渡门：仅供仍在旧设备模型上 probe 的驱动（virtio 家族迁移期）以合成身份发布，随最后一个遗留发布者一起删除 |
| `src/error.rs` | OS 能力错误到 RuntimeError / ProbeError 的转换，ServiceError 转换位于 services.rs |
| `src/domain/mod.rs` | `DeviceId` 管理键与 `DeviceDomain` 门面：域身份、短状态锁、entry 的四个状态域、`global_domain()`（组合根与 `declare_driver!` 之外不应出现） |
| `src/domain/registry.rs` | 注册引擎：device/driver/bus-instance 表、entry 类型、add 时校验、快照、拓扑链接 |
| `src/domain/probe.rs` | 探测引擎：批选择、锁外执行、generation 守卫的提交 |
| `src/domain/dependency.rs` | deferred 等待器：park、精确 wake（含 late-provider 重扫）、丢唤醒 generation 守卫 |
| `src/domain/lifecycle.rs` | 每设备管理入口、解绑与注销、锁外 Data visitor；teardown 为 `remove()` → drop `Data`，不自动处理下游设备 |
| `ut-tests/domain.rs` | 模型单测（`MockServices` 上运行），含 `env.provide` provider 门与 defer/wake 链路 |
| `ut-tests/lifecycle.rs`、`ut-tests/management.rs` | 管理锁入口、服务 bundle 归属、绑定/解绑/注销及重入回归 |
| `ut-tests/domain.rs` 中的 publication 契约测试 | 模型测试身份构造与跨 crate 发布生命周期验证 |
| `ut-tests/firmware.rs`、`ut-tests/platform_bus.rs` | firmware 与 platform bus 单测 |

## 架构

四个引擎共享 `SpinNoPreempt<DomainInner>`，保护注册表、队列和短状态迁移。
每设备管理锁负责跨回调的串行化，锁序为管理锁 → 域短锁；等待管理锁时不持域锁。

```text
                    DeviceDomain  (域身份 + 短状态锁 + 管理入口)
                             │
        ┌────────────┬───────┴────────┬─────────────────┐
        ▼            ▼                ▼                 ▼
    registry      probe          dependency        lifecycle
   （三张表 +    （队列分派 →    （park / wake /   （设备管理锁 →
    entry 的      锁外匹配执行 →   generation 守卫   unbind / remove
    四个状态域）   守卫式提交）     / late provider）  → teardown）

    DomainInner { bus_instances, drivers, devices, sched, services,
                  next_device_id, next_driver_key }

    DeviceEntry { core          ← Arc<DeviceCore>：描述符 + 身份 + management + domain
                  services      ← 注册时选定的宿主 bundle
                  lifecycle     ← 状态机 + generation
                  binding       ← driver / Data / attempted
                  topology      ← children
                  probe         ← last_deferred_dep / dep_seen_generations }
```

跨引擎的顺序（probe ↔ remove ↔ wake）只在 `DeviceDomain` 门面里出现一次。

`DeviceDomain` **可实例化**：crate 内没有进程级注册表，`global_domain()` 只是
组合根用的那一个 LazyInit 实例，测试各自 `DeviceDomain::new()`。设备 id 从
crate 级 `DEVICE_ID_ALLOCATOR` 取，继续作为内部索引与日志关联键。
`DeviceId` 定义在 `domain/mod.rs`，是注册表、拓扑及调度使用的运行时管理键，
不是硬件编号或持有设备的引用，已取消 crate 根部导出和公开 id 访问器。
`add_device` 返回 `DeviceRef`；remove、snapshot、with_data 接收借用对象引用。
这些入口在域锁内同时核对内部键与 Arc 对象身份，外域或旧对象不能借相同数字键
访问当前条目。数字只用于内部索引、调度及错误/日志诊断，没有公开按数字查找入口。

`DeviceSnapshot::device`、总线成员列表及 deferred_report 携带对象引用；
快照的状态和驱动名是获取时刻的值，不是并发操作许可。注销统一删除 entry，
之后 snapshot 返回 None；同域已注销对象通过管理入口后重复 remove 返回 Ok，
foreign 对象在进入宿主锁前即被域身份检查拒绝。
同一路径重新注册产生新对象，旧引用只保留旧元数据，不会匹配新注册。

`add_bus_instance` 的控制器参数为 `Option<&DeviceRef>`，同样在锁内验证身份和
可挂载状态。实例记录强持有控制器身份，以便未绑定控制器删除后仍返回原控制器
的离线元数据；它不持有绑定 Data，也没有设备核心到域/实例对象的反向强引用。
总线实例自身仍由 BusInstanceId 标识，本轮不把总线实例也改造成引用对象。

## 设备引用与注销

`DeviceCore` 是已注册描述符的唯一持有者，同时保存管理键、物理路径、
当前 probe/绑定的驱动名和 offline 状态；失败探测与解绑会清空驱动名。
域条目、`DeviceRef` 与 class 发布
共享同一个核心。probe 批次及 remove 只克隆 Arc，不再克隆描述符；
`DeviceAccess<'a, B>` 仍是回调期间借用核心中描述符和服务的类型化视图。
`DeviceAccess::reference()` 可保留对象身份，不延长回调服务借用。

`DeviceRef` 只提供不可变描述、路径、驱动名、online 快照和对象同一性比较，
不暴露核心 Deref、不允许外部构造，不提供硬件操作或资源申请入口。
`same_device` 按 Arc 对象身份判断；相同总线位置再次注册也是另一个对象。
online 不表示已绑定，也不是并发操作许可；Probing 期间仍允许发布 class。

`unbind_device` 取消 pending/deferred 工作，在域锁外调用 remove 并销毁 Data，
清空驱动名后回到 `Unclaimed`；注册、身份和 online 状态保留，可用 `bind_device` 重试。
`remove_device` 只注销指定设备，在 teardown 前关闭来源，完成后从域表删除 entry，
entry 析构也在域锁外。父子设备和控制器下游成员的顺序由总线负责，模型不递归退出。
外部引用不会推迟 remove 或 Data 释放，最后一个引用的 Drop 不触发注销。

无论是否绑定过，注销后外部引用均可继续读取静态信息，域中不保留 Removed 快照。
只有域与所有外部持有者都释放核心后，
静态元数据才销毁。总线数据必须是无硬件所有权的描述，其 Drop 不得依赖可睡眠
上下文，因此这里使用普通 Arc，不引入 DeferredArc。驱动资源仍由绑定 Data 或
class 在途 runtime 持有，不能藏进描述符。销毁整个域不能替代显式注销协议。

## 父设备对象引用

`DeviceDescriptor::parent` 是 `Option<DeviceRef>`，由枚举者传入父设备注册所得
引用的 clone。子设备可通过描述符直接读取父设备，无需用 DeviceId 再查注册表。
描述符克隆保留同一个父对象，不复制父设备身份。

注册在同一域锁内完成查找、Arc 身份核对、状态校验及反向 children 更新。
外域对象、已从表中删除的对象或同键不同对象返回 `ParentMissing`；
域内 Removing/Removed 条目返回 `ParentUnavailable`。仅持有引用或观察到
online 不能证明可以挂载子设备。Probing 父设备仍可枚举孩子，保持原有行为。

引用方向是子设备强持有父设备，反向 children 仅保存域内部管理键，父设备不持有
孩子的强引用。注册后描述符不可变、只能引用已注册父对象，不提供 reparent，
因此正常构造不能形成拓扑强引用环。移除孩子从父条目的反向列表摘除，
但不清空静态父引用；移除父设备不自动移除孩子或控制器所提供实例的成员。
孩子的外部引用可保留父设备离线元数据，不延迟父设备的 remove 或绑定资源释放。
总线负责停止枚举、处理下游设备并确定锁序；控制器关联同样使用设备对象引用。

## 设备服务上下文

MMIO、IRQ 注册、IRQ 映射、DMA 和时钟获取统一接收 `DeviceContext<'_>`，
不再接收裸 DeviceId。上下文定义在服务契约旁，由 `DeviceAccess::res()` 的资源视图
在同步调用时构造，借用当前回调的 DeviceRef；没有公开构造器，也不能从长期
DeviceRef 转换而来。宿主通过 `context.device()` 直接读取同一设备的描述符、
父设备和来源信息，无需额外查表。宿主需要保留身份时可 clone DeviceRef，
但资源守卫的释放闭包必须拥有释放所需状态，不能借用短期上下文。

资源视图也直接读取共享描述符，移除单独保存的资源表、DMA 副本和管理键。
DMA 缺失时模型在调用宿主前返回 Unsupported；宿主从上下文取得 DMA 配置，
防御性拒绝缺失配置，继续执行地址转换、mask 校验和错误传播。
失败 probe 必须先停运行活动、显式注销已发布能力，再由 RAII 释放无活动的局部守卫；
模型无法替未返回的 Data 调用 remove。成功绑定的守卫由 Data 持有并在 remove 后释放。

上下文只表达请求归属与借用范围，不是并发准入令牌；offline 快照不能替代
同步、IRQ 屏蔽、quiesce 或 class 在途计数。本步不新增运行期资源操作接口，
不改变 provider/extension 契约，不新增生产 V2 host adapter，也不迁移旧 device-res。

## 调用约束 / 执行上下文

- 域锁是 `kspin::SpinNoPreempt`：**禁抢占、不关中断**。因此 `add_device` /
  `remove_device` / `probe_pending` / `wake` / 各类快照都**不得在硬中断上下文
  调用**——持锁期间来中断会在同一把锁上自死锁。
- matching、`DriverOps::probe` / `remove` 和 `with_data` visitor 在域锁外、
  设备管理锁内运行，可以睡眠，但不能重入同设备管理或等待需要该锁的工作。
  不同设备可以并行；跨设备调用仍须由调用方避免锁依赖环。
- `remove_device` 的 teardown 相（`DriverOps::remove` → drop `Data`）
  同样在锁外，且按构造运行在任务上下文——这是各驱动 `Drop` 里
  `block_wait::prepare_block_wait()` 之类可睡眠调用的前提。
- 没有 lifecycle observer / remove notifier：域状态机是移除的唯一权威，
  发布回收由 `Data` 持有的 `Registration` 完成。类发布的 `ClassHooks::unpublish`
  （`del_gendisk`、transport close）在仍有在途 `InUse` 时推迟到最后一个许可 drop
  ——见 `../class/src/publication.rs` 的 `finish_or_defer`。
- `global_domain()` 的初始化不代表设备已可注册。`NoServices` 使用默认工厂，
  `add_device` 返回 `AddDeviceError::Management(RuntimeError::Unsupported)`，
  不入表也不探测。真实宿主工厂与管理入口要求已初始化的可睡眠任务上下文。
- `IoMap` 的访问器带边界与对齐检查，越界/未对齐**总是 panic**（不是
  `debug_assert`）——release 下同样是 panic，因为 MMIO 上的未对齐 volatile
  访问是 UB。

整条跨 crate 链路（枚举 → probe → 发布 → quiesce → 释放）只在
`docs/device-life.md` 描述一次；本文件只讲这一站。

## 公开名词预算

驱动面只允许出现这些名词，其余都是实现细节：`DeviceAccess`、`Driver`/`DriverDesc`、
`Bus`、`Resource`（devm guard 族）、`Class`（发布）、`Provider`（家族描述符）、
`Host`（进程级根，driver-host 侧）。新增一个公共名词要说明它归并/删除了哪个既有
名词；只有单个使用者的抽象不进公共面。

## 状态机

注册与绑定分开：`Unclaimed` 仍在域表中，注销则删除 entry。
枚举中保留的 `Removed` 变体不表示当前注销路径会留下该状态的条目。

```text
add_device ──► Pending ── matching ──► Probing ── Ok(Data) ──► Active
                  │                     │
                  │ 无候选               ├─ Fatal ──► Pending（记录 attempted）
                  ▼                     └─ Deferred(dep) ──► Deferred
               Unclaimed ◄── finish_probe_phase(false) ───────┘
                  │                                           │
                  └─ bind / late driver / late provider ──► Pending ◄─ wake

Pending / Deferred / Unclaimed / Active
                  │ unbind_device 或 remove_device
                  ▼
               Removing ── unbind ──► Unclaimed（保留注册与 online）
                  └─────── remove ──► 删除 entry（offline）
```

要点：

- 同设备其他任务的管理请求等待当前 probe/teardown 释放管理锁；同任务重入
  返回 `Management(Reentrant)`，非法上下文返回 `Management(InvalidContext)`。
  `Removing` 表示当前管理操作正在 teardown，没有可续做标记或自动下游回滚。
- `Unclaimed` 不是终态：后注册的驱动（`late_driver_registration_*` 测试）与
  后发布的 provider（`late_provider_wakes_unclaimed_device` 测试）都能把它捞回来。
  `ProbeState::last_deferred_dep` 因此要跨 `finish_probe_phase` → Unclaimed 保留。
- `bind_device` 对未绑定设备清除 attempted/deferred 记录后重新尝试，Active 不变。
  注销后的引用不能再次绑定；同域重复注销通过上下文检查后返回 Ok。

## 算法流程

### 探测（`probe_pending`）

1. 在域短锁内取走 `sched.pending` 并取得设备引用，随后逐设备进入管理锁。
   没有全局 probe 标志；并发分派者可处理不同设备，同一设备仍串行。
2. 核对注册对象与 Pending 状态，快照驱动列表、attempted、注册时的 services
   及 generation。域锁外 matching 选择未永久失败且得分最高的驱动。
3. 重新持域锁置 Probing；无候选且驱动注册表未变化时置 Unclaimed，
   若注册表有变化则重新入队。probe 在域锁外使用该 entry 的宿主 bundle。
4. 提交时校验 generation 与 Probing 状态：成功转 Active；未能提交的 Data
   先经 driver.remove 再 drop，并返回 BadState；Deferred 进入依赖等待，
   Fatal 记入 attempted 并重新入队以尝试其他候选。
5. 循环至队列为空。管理入口被拒绝的设备计入 `ProbeStats::rejected`，
   本轮结束时仍为 Pending 的对象重新入队，不在同轮无限重试。

### 依赖等待（`dependency.rs`）

- `wake(dep)` 对每个 `DepSource` 维护一个 generation，**即使没有设备 parked 也
  自增**。设备提交 `Deferred(dep)` 时若发现该 dep 的 generation 已经前进，就
  重新入队探测而不是 park——这关闭了"检查依赖 → 提交 Deferred"之间 provider
  恰好注册的丢唤醒窗口（`wake_during_probe_window_does_not_lose_the_device`）。
- `wake` 还会重扫 `Unclaimed`（late provider 场景）。
- `finish_probe_phase(false)` 在探测阶段结束时把仍 parked 的设备报告出来并降级
  为 `Unclaimed`，而不是让它们静默停在那里。

### 移除（`lifecycle.rs`）

1. 核对域身份，进入设备管理锁，再在域短锁内核对注册对象。注销已完成的
   同域对象返回 Ok；foreign 返回 NotFound。
2. 置 Removing、增加 generation、取消 pending/deferred，取出 driver/Data
   与 entry.services。只有注销关闭来源；普通 unbind 保持 online。
3. 域锁外运行 `DriverOps::remove`，然后 drop Data、清空驱动名。
4. unbind 将状态恢复为 Unclaimed；remove 从父反向列表摘链并删除 entry，
   在域锁外释放条目。管理锁覆盖整个流程，包括资源析构。

流程只作用于本设备，不遍历 parent children 或 bridge 实例成员。
总线/控制器负责停止枚举、安排下游退出及跨设备锁序。

### 数据访问（`with_data`）

持设备管理锁，在域短锁内验证 Active 并暂取 Data；释放域锁后运行类型化 visitor，
再持域锁归还 Data，最后释放管理锁。visitor 不可重入同设备管理。
管理入口失败、foreign、未绑定或类型不匹配均返回 None，不执行 visitor。

## 并发模型

- `SpinNoPreempt<DomainInner>` 保护表、队列及短状态迁移；每设备管理锁
  保护跨回调的绑定操作。必须先取得管理锁，再短暂获取域锁。
- matching、probe、remove、Data drop 和 visitor 均在域锁外。表和队列操作
  仍可能分配，不能声称所有域锁内路径无分配。
- 不存在全局 probe 泵锁；宿主服务必须支持不同设备并发，不得依赖单槽串行假设。
- `DEVICE_ID_ALLOCATOR` 是 crate 级 `AtomicU64`，`fetch_add`，跨域唯一。
- 队列分派只克隆共享核心的 Arc，不深拷贝描述符和 compatibles。
  attempted 集合仍按批次复制，`modalias` 每次 probe 重算；本次不改变匹配策略。
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
不是使用不便。实现已迁到 driver-os-api，安全契约见其安全文档。
model 的 MockServices 仍按这个契约构造测试映射，生产适配必须同样满足它。

### 域短锁与每设备管理锁

域短锁使注册表校验与状态变更原子化；可睡眠管理锁覆盖单设备回调与资源清理，
让同设备请求等待、不同设备独立推进。总线自己的跨设备依赖与锁序不由域推断。

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
let Some(service) = env.extension::<PciTransportService>() else { … };
```

家族 transport 需要超出能力契约的主机机制时，扩展自己是**一个值**，按类型
住在 host 的 `ExtensionRegistry` 里；core 不认识家族，host 不再需要
`as_any`，驱动写错的类型就是"没注册"。调用方仍然命名
`driver_host::virtio_transport::PciTransportService`（E-02a），那是"驱动的
家族知识"本身，不是框架的泄漏。
登记为例外 E-02。

### `IrqAction` 没有 `WakeThread` 变体

本模型的 `IrqAction` 只表示同步处理结果，Op、设备服务和 DeviceAccess 均没有
threaded IRQ 注册入口。这是 driver-os-api 尚未接入的能力，不是对底层能力的判断。
未来需要连同线程注册、退出同步和驱动侧入口一起设计，不得静默降级唤醒请求。

### 驱动身份是注册键，不是对象地址

`DriverKey` 是域内单调分配、永不复用的注册序号，`BindingState::attempted`（设备
的永久失败历史）以它为键。身份安全性因此不依赖"驱动永生"假设：地址只能作
生命周期限定的对象借用，一旦允许注销，地址复用（ABA）会把一次失败注册静默
别名到它的继任者身上；键不复用使历史里的死键无害。键是域内的——`attempted`
和注册表都是域内部簿记；`DeviceId` 也已封装为内部键。设备管理接口核对对象
身份，class 发布不依赖数字键管理发布身份。

驱动注册按（总线类型， 名字）去重并拒绝（Linux `driver_register` 对同一情形
返回 `-EBUSY`）：同一 probe 体出现两个键，一次 Fatal 失败会留下仍匹配的孪生
键，让每个设备把同一个 probe 跑两遍；per 总线名字唯一同时保证 `DeviceCore`
驱动名身份卡无歧义。同分 tie-break：锁外 matching 用 `max_by_key` 在注册序
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

- probe 结果无法提交 → 使用注册时的 services 调用 remove，再 drop 未提交 Data；
- unbind → remove、drop Data、清驱动名、恢复 Unclaimed；
- 注销 → 关闭来源、清工作表、remove、drop Data、删除 entry；未绑定时无驱动清理。

`ReleaseHook = Arc<dyn Fn() + Send + Sync>` 是每个 guard 携带的延迟释放钩子，
由 `driver-host` 在构造 guard 时注入（例如 `move || provider.unmap_mmio(mapping)`）。
`driver-model` 自己从不实现释放动作。

## 注册与资源清理约束

DeviceAccess::publish 返回不可克隆的 Registration；publication() 返回不撤销注册的
消费者引用。remove_device 在调用驱动 remove 之前使 DeviceCore offline，拒绝
新使用；remove 必须停止并等待已获准操作，或者把所需资源保留在共享 runtime 中。
Registration Drop 只延迟 class hook，不阻塞 Data 的后续字段析构。
当前 model 不持有所有 class 注册的 drain/join 集合，不能声称字段顺序自动保证
MMIO、IRQ、DMA 生命周期安全。具体资源析构不得先于依赖它们的操作退出。

DeviceAccess::publish 仅接收 Class 与 runtime，不接收发布名。通用 class 允许同源多项
独立注册；这不改变 model 自己的驱动注册去重或 provider 的业务键检查。
