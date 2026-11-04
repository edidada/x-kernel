# driver-model — 安全与可靠性分析

## 生命周期名称与权限边界

`Bound` 只证明绑定已提交；`is_online` 只提供注销尚未开始的快照，均不授予
硬件访问许可。`unregister_device` 在驱动 teardown 前关闭来源，最后删除注册；
旧引用仍可读描述。总线位置字符串不是完整 DEVPATH，也不能用作全局授权身份。

## Probe 失败诊断

框架在 Fatal 分支保留错误内容并记录设备身份、path 与候选驱动；Unsupported
为 debug，其他错误为 warn。日志不记录原始指针或 DMA 内容，不增加诊断保活表，
也不改变失败后的生命周期。debug 被过滤时正常拒绝可能不可见；真实初始化失败
在 warn 启用时可诊断。设备注册成功不能被当成已绑定或硬件可用的证明。

## Held DMA capability lifetime

The model checks descriptor DMA presence but does not manufacture an identity
mapping or validate a host's allocation/cache implementation. The unsafe os-api
DeviceDma provider contract owns those invariants. Capturing a capability must
not consult a replaceable global provider on later allocation or release.
Keeping an Arc alive does not keep a binding active or authorize access after
removal. Drivers must stop hardware before coherent or streaming guard Drop,
and keep streaming buffers exclusive and live until cleanup completes.

## R1-04 来源不可漂移

DeviceAccess 只能由模型回调适配层构造，显式借用 RegisteredDriver 的只读身份。
publication_source 不从可变的设备当前驱动名推断历史来源。BindingSource 保存
DeviceRef 与 Arc<DriverIdentity>，没有 ops/Data 指针，path 仍向设备对象查询。
旧来源不授予访问权限；子系统必须拒绝注销后的 read/is_failed。
共享身份的最终析构仅释放元数据，不执行驱动析构或硬件清理。保留 DeviceRef 仍
要求枚举描述和管理锁对象遵守非睡眠最终析构合同。

失败 probe 不返回 Data，模型无法替其执行 remove；驱动必须先停止活动、注销
发布，再释放局部守卫。成功但未接纳的 Data 由模型显式 remove 后 drop；不得以
普通 RAII 替代这个过程。模型保持家族无关，hwrng 组合测试位于 kdriver，未反向
依赖子系统，也未引入通用 devres 动作栈。

## R1-01 管理入口能力边界

`DeviceManagementLock` 的可信宿主实现必须在执行回调前验证可睡眠上下文，
拒绝当前任务已持有同一锁的重入，并保证独占、同步、恰好一次调用和锁释放。
model 不通过任务 ID 自行绕锁，也不在本 crate 实现睡眠同步原语。
`InvalidContext` / `Reentrant` 是零回调、零生命周期副作用的入口错误，
不是退出一半后的可重试错误；不同设备使用不同锁。

调用方不得持全局域短锁进入；回调必须恢复调用上下文，且不能等待需持有
当前管理锁的任务。硬件停机、操作排空、父子锁次序仍由驱动/子系统/总线负责，
不能从“取得管理锁”推导资源已安全退出。公开安全 trait 的错误实现属于
宿主合同违约；类型本身不授予绕过 Rust 借用规则的权限。

`add_device` 在发布前通过 `DeviceServices::create_management_lock` 创建能力，
在其 `run_locked` 内入表；默认工厂拒绝注册。`DeviceCore` 保有管理锁与域身份
`Arc<()>`，bind/unbind/remove/with_data 及队列探测使用同一设备管理入口。
每个 entry 固定注册时的 services；后续替换域默认服务不会改变已有设备的宿主。
真实 kdriver adapter 的模型生命周期 UT 包括测试控制器的 owned-only 退出、
孩子退出期间父服务保活、枚举提交与退出交错、重绑后旧枚举门禁拒绝提交。
测试控制器不是生产总线；真实硬件停机与操作排空仍需各自的集成验收。

## R1-02 调度进度边界

pending 的入队及批次认领在同一域短锁下完成，回调在锁外；没有一个需额外
释放的全局执行者标记。空批次观察之后到达的生产者自己发起分派。通知代次
覆盖检查依赖到提交 Deferred 的窗口，且不作为公开设备身份。
已出队候选仍须取得管理锁并复核对象/状态，不能靠保留 DeviceRef 复活注销设备。
测试覆盖判空后入队、出队后注销和出队后被其他绑定入口消费等交错。

测试 hook 仅 cfg(unittest) 构建具有，按域隔离、锁外一次性调用，不进入生产
能力面。定向模型交错测试不等于证明任意驱动回调的跨设备锁图无环；非法上下文
与重入拒绝也不承诺自动唤起后台执行者。调用者必须遵守 DriverOps 的管理锁
等待合同，并在合法上下文显式重试被拒绝的分派。

## 设备引用生命周期边界

驱动的 DeviceAccess 与 Environment 由回调适配层分别构造，前者关联当前设备，
后者仅暴露框架服务。Environment 不代表设备在线状态或设备资源所有权。
provider 注册仍使用静态 lookup，不随设备移除撤销；不能用它代替具有
撤销句柄和在途许可协议的 class 发布。延迟探测只是错误结果，不是环境状态。

回调 DeviceAccess 借用适配层的 DeviceRef；DeviceInfo 只借用 DeviceAccess，不增加强引用。
类型化 bus_data 只能从该引用的描述符构造，避免身份与总线数据来自不同设备。
这些借用不改变注销协议或 binding 资源释放时机；长期引用仍不授予硬件权限。

公开管理入口只接受设备引用，不再暴露 DeviceId 类型、数字查找或 id 访问器。
内部键定位后还必须核对 Arc 身份；remove 的身份校验与 Removing 声明共用域锁，
查询在域锁内读取；with_data 在管理锁内暂取 Data、域锁外调用 visitor，
归还 Data 后才释放管理锁。域身份先于宿主管理入口校验。外域及同键不同对象返回
NotFound/None，且不调用驱动 remove 或数据访问 visitor。
快照与 deferred 报告携带对象引用，只保留身份，不维持硬件可用性。
总线实例保留原控制器身份用于诊断，离线控制器即使仍被引用也不能新增实例成员。

`DeviceRef` 保留身份和静态描述，不保留绑定 Data 或硬件操作权限。
注销使共享核心 offline；已绑定设备在锁外执行 remove 和 Data drop，
无论是否绑定都删除域条目；同域重复注销通过管理入口后返回 Ok。
unbind 只释放绑定并回到 Unclaimed，保留注册与 online。外部引用不能阻止这些动作，
online 仅为快照，不能用作同步或在途操作许可。
已移除对象不会因同一路径的新设备注册而复活；对象同一性使用 Arc 指针比较。
描述符随最后一个核心引用释放，总线数据必须无硬件所有权且允许非睡眠析构。
销毁域前仍须显式完成注销，不把域析构当作驱动 quiesce 协议。

回归覆盖：`retained_reference_does_not_retain_binding_resources` 验证引用保留期间
remove/Data drop 各执行一次、描述符共享且移除后可读；
`unbound_removal_retires_identity_without_invalidating_metadata` 验证轻路径 offline、
引用 Drop 不注销、同域重复移除幂等和同位置重新注册的身份隔离；
`device_removal_closes_source_before_driver_quiesce` 验证 DeviceRef 与 class 来源一致。

## 设备服务上下文

设备资源服务通过 `DeviceContext<'_>` 借用当前回调的身份，不接受裸数字键。
上下文只能由模型资源视图构造，不作为在线许可或同步屏障。资源释放闭包不得
捕获短期上下文；clone 身份只延长静态元数据寿命，不延长绑定资源。
UT 验证五类请求指向同一核心、正常注销的四类守卫各释放一次、DMA 失败与时钟
Deferred 时部分获取的资源立即回滚。未配置 DMA 的原有测试继续验证宿主未被调用。

## 总线归属校验

父设备关系使用 `DeviceRef`，但引用存活不授予注册孩子的权限。域在锁内核对
父条目与引用指向同一核心，并拒绝跨域、已删除或处于 Removing 的父对象；
校验与反向 children 更新不可分割，阻止在父设备已进入 teardown 后继续挂载。
子设备仅强持有父设备，反向列表没有强引用；不支持修改已注册 parent。
保留离线孩子仅保留父设备元数据，不保留硬件资源。
回归覆盖跨域父引用、同键不同对象、Removing 或已注销对象拒绝、孩子摘链，
以及显式注销后父元数据可读、最终弱引用失效（无拓扑引用环）。

设备仅持有必填的总线实例标识。注册时在域锁内验证实例属于当前 domain，
并检查设备数据词汇的类型与实例类型一致；外域或不存在的实例不能注册设备。
匹配与快照读取实例中的总线类型，避免描述符与实例分别声明归属而发生漂移。
实例标识只是关联键，不是硬件初始化凭据；创建方和枚举方负责传递真实的拓扑。
控制器进入 `Removing` 后，`add_device` 在同一域锁内检查它，阻止无 `parent`
的新成员绕过门禁。控制器删除后，其实例也不能再接收成员，但已有成员不会
自动注销。总线必须停止枚举并安排下游退出；模型门禁不替代控制器的枚举/退出
协议。该协议由测试控制器覆盖，真实总线与硬件退出仍需独立集成验收。

## Scope

`drivers/base/model/src/`：`bus/`、`driver.rs`、`device/{mod,descriptor,access,source}.rs`、`error.rs`、`resource.rs`、
`provider.rs`、`environment.rs`（框架服务入口）、`services.rs`（带设备上下文的服务契约）、
`domain/{mod,registry,probe,dependency,lifecycle}.rs`。
测试 `ut-tests/domain.rs` 不在分析范围内，但其中的 `MockServices` 是信任模型的
一个具体实例。

通用 base/class 层已退役。模型拥有 PublicationSource 只读来源契约；私有
BindingSource 保留同一 DeviceRef 与原绑定驱动的不可变身份，不保留 ops/Data。
设备注册在线状态由模型唯一维护；普通 unbind 不关闭设备在线位，子系统必须
自行管理操作准入与排空。来源的最终析构不要求睡眠，路径不作为发布唯一性键。

## 信任模型

描述符的 builder 只保证构造入口的必填字段完整性；它不会验证传入的实例标识、
父子拓扑或 DMA 参数真实性。字段公开，所有注册路径仍须经过 domain 的动态
校验；不得因为使用了派生 builder 而删除这些检查。可选 DMA 的默认值仍为
`None`，派生构造不自动开启 DMA。

DMA 默认未配置：model 在调用宿主分配器前拒绝缺少 `DmaInfo` 的请求，避免把
描述缺失解释成无限地址范围或缓存一致性保证。`Some(info)` 来自可信枚举／平台
代码，不是硬件能力探测结果，也不代替宿主的地址合法性、mask 和缓存维护检查。
UT 验证未配置时宿主调用次数为零，配置后保留地址转换和分配错误传播。

| 边界 | 谁在外面 | 本 crate 假设什么 |
|---|---|---|
| 固件 → 描述符 | DT / ACPI / PCI 配置空间（`driver-init`、`bus/pci`、`firmware/of` 转换后交给 `add_device`） | **半可信**：地址、大小、中断号、compatible 字符串都可能与真实硬件不符或被篡改。本 crate 只做结构校验，不做"这段物理地址真的是 MMIO"的判断——那是 `driver-host` + `memspace` 的职责 |
| `DriverOps` 实现 → 域 | `drv/**` 里的驱动 | **可信内核代码**。设备管理锁串行化同设备管理并拒绝同任务重入；不宣称能隔离任意错误驱动造成的破坏 |
| `DeviceServices` 实现 → 域 | `driver-host::Host` 或测试 mock | **可信**。契约要求返回的 guard 携带正确的 `ReleaseHook` |
| 驱动 → provider 注册表 | `env.provide::<C>(key, lookup)`（经 `ProviderServices`） | 半可信输入：`key` 与 lookup 自称的 id 必须一致（`Host` 校验，不一致返回 `Invalid`）；重复 key 返回 `AlreadyExists`，不覆盖已有 provider |
| 驱动 → 类发布 | `dev.publish(&CLASS, …)` → `Registration` | **可信**；返回的所有者存进 `Data`（drop 即撤销），`InUse` 计数保证 class 侧 teardown 不在在途操作下运行；consumer 拿到的是无撤销权限的 Publication |

本 crate 负责设备绑定、probe 状态、发布来源与退出协调。MMIO 访问器与资源句柄
已移到 [driver-os-api](../../os-api/docs/security.md)，本模型必须安排正确的使用与释放顺序。

## 外部边界 / 攻击面

| 输入源 | 入口 | 说明 |
|---|---|---|
| 设备树 / ACPI | `DeviceDomain::add_device(DeviceDescriptor)` | `MmioRegion{base,size}`、`IoPortRange{base,size}`、`IrqSpec::FirmwareRoute{hwirq,controller,domain,trigger}`、`ClkSpec{phandle,index}`、`compatibles: Vec<String>` 全部来自固件 |
| PCI 配置空间 | 同上（`bus/pci::discover` 产出） | `PciIdentity{vendor_id,device_id,class,subclass}`、BAR 派生的 `MmioRegion`、MSI-X 派生的 irq |
| 板级静态表 | 同上（`driver-init::enumerate_static_devices`） | x86 COM 口 `IoPortRange{base, size: 8}`、ramdisk alias |
| MMIO 寄存器 | driver-os-api 的 `IoMap::read*/write*` | 访问器不在 model 内；设备服务返回句柄，不直接操作寄存器 |
| 用户内存 / 用户指针 | 无 | 本 crate 不接受任何用户态指针 |
| 驱动宣告的 provider key | `Environment::provide` | key 校验与冲突处理由宿主注册实现负责 |
| DMA 缓冲 | 无（`CoherentMem` 只是所有者句柄） | 分配与映射在 `driver-host`/`kdma` |
| 中断上下文 | **不接受** | 见"线程安全" |

固件输入的校验分工：

- **本 crate 校验**：`add_device` 的父设备存在性与可用性
  （`AddDeviceError::ParentMissing` / `ParentUnavailable`）、bus instance 存在性
  与类型一致性及控制器可用性（`BusInstanceMissing` / `BusInstanceTypeMismatch` /
  `BusInstanceUnavailable`）、
  `DeviceLocation::FirmwareNode{id}` 的唯一性（由 compose 侧的单调分配器 +
  耗尽拒绝保证）。
- **本 crate 不校验、由下游校验**：`MmioRegion` 是否与真实硬件相符、是否与其他
  设备重叠（`driver-host` → `memspace`）；`hwirq` 是否在该 controller 的合法区间
  （`kirq` 的 domain 映射，失败返回 `ResError`）；`ClkSpec.phandle` 是否存在
  （`ClkRegistry`，不存在则 `ServiceError::Deferred`，不是错误）。

## unsafe 代码清单

model 生产源码当前没有 unsafe。原 IoMap 构造、指针计算与 volatile 访问器
已迁到 driver-os-api，其前置条件和检查没有改变，见该 crate 的安全清单。
model 的测试 mock 仍使用 unsafe 构造测试映射，不代表生产宿主已完成适配。

## 内存安全不变量

1. **driver-os-api 的 `IoMap` 的 `region` 不可变且与 `vaddr` 同时确立**。构造后没有任何 API 能改
   `region.size`，因此 `access_ptr` 的边界检查不可能被后续操作绕过。
2. **driver-os-api 的每个 guard 的 `ReleaseHook` 至多跑一次**。`IoMap::drop` 用
   `self.release.take()`；`DevIrq`/`CoherentMem`/`ClkHandle` 同构。
   `EnabledClock` 不持有 ReleaseHook，而是独立平衡一次 prepare/enable。
3. **`Data` 的装箱与拆箱由同一个驱动类型完成**。`AnyDriver::remove` 里
   `data.downcast_mut::<P::Data>()` 失败会 panic（带 `CG-021` 例外说明），
   而它不可能失败：域只把 binding 的 `Data` 交回**创建它的那个** `RegisteredDriver`
   （`entry.binding.driver` 与 `entry.binding.data` 在同一把锁下同时写入）。
4. **probe 与 teardown 不能在同设备上交错提交**。管理锁覆盖匹配、probe 到
   绑定提交，注销/解绑必须等待；同任务重入在入口拒绝。成功提交检查对象、
   Probing 状态及空绑定这些内部不变量，不维护额外生命周期版本或过期回收。
5. **`DeviceState` 的迁移只在域锁内发生**；管理锁覆盖同设备 teardown、
   Data 析构和 entry 删除，防止并发调用重复清理。

## 线程安全

- `DeviceDomain` 使用一把域状态 `SpinNoPreempt`，每设备持有独立宿主管理锁，
  id 使用 crate 级 `AtomicU64` 分配器。等待管理锁时不得持域状态锁。
- **禁抢占、不关中断**。因此所有域 API 只能在任务上下文调用。硬中断里调用会
  在同一把锁上自死锁。这条约束靠文档 + review 保证，类型上没有 barrier
  （与 `kdevice` 旧模型的限制相同）。
- `DriverOps: Send + Sync`，`DriverOps::Data: Send + Sync`——因为中断 handler 与
  work item 通过 `Weak` 共享 `Data`。
- `ReleaseHook = Arc<dyn Fn() + Send + Sync>`、`IrqHandlerFn = Arc<dyn Fn() ->
  IrqAction + Send + Sync>`：都要能跨 CPU 执行。
- 队列直接分派，没有全局 probe 标志；不同设备的 matching/probe/remove
  可以并行，宿主不能依赖域级串行来保护单槽资源收集器。
- 同设备其他任务等待管理锁，同任务重入被拒绝。matching、驱动回调、
  Data visitor 和析构在域锁外执行，但仍持管理锁；跨设备依赖环由调用方避免。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|---|---|---|---|---|
| T-01 | 固件给出一个 `MmioRegion`，其 `base+size` 覆盖内核自身映射；驱动 `iomap` 后写寄存器 → 破坏内核内存 | 高 | 被篡改的 DT / ACPI，或固件描述与真实硬件不符 | **已实现**：driver-os-api 只按 `region.size` 做访问边界检查，不判断物理地址合法性；映射由 `driver-host` 经 `memspace` 建立，`memspace` 拥有物理地址空间的分配与重叠检测。**残余风险**：若 `memspace` 允许映射一段与内核镜像重叠的物理地址，本层无法发现。加固建议在 `driver-host`/`memspace` 侧拒绝已知保留区 |
| T-02 | 驱动用超出实际寄存器窗口的 `offset` 访问 `IoMap` | 高 | 驱动 bug 或转写错误 | **已实现**：`access_ptr` 用 `checked_add` 防溢出 + `assert!(end <= region.size)`，越界立即 panic 而不是静默访问相邻寄存器 |
| T-03 | 驱动以未对齐 offset 做 `read32`/`write64` | 高 | 转写 Linux 驱动时把寄存器偏移算错 | **已实现**：`access_ptr` 总是检查 `is_multiple_of(size)`，不满足即 panic。旧模型此处是 `debug_assert`（release = UB），本 crate 已修正 |
| T-04 | probe 结果在 teardown 后提交，造成绑定资源归属错误 | 高 | 同设备绑定与注销竞争 | **已实现**：每设备管理锁覆盖 probe 与提交，解绑/注销等待；同任务重入拒绝；提交检查对象、状态与空绑定不变量 |
| T-05 | 同一设备被两条路径同时移除，teardown 跑两遍 | 中 | 热插拔事件与手动移除竞争 | **已实现**：管理锁使竞争者等待；entry 删除后同域重复注销返回 Ok，不重复 remove/Data drop；同任务重入在回调前拒绝 |
| T-06 | 父设备先于子设备释放硬件，子设备仍访问它 | 高 | 控制器退出而下游仍活动 | **当前边界**：模型只注销本设备，Removing/删除状态阻止新成员挂载；总线负责停止枚举、下游退出与锁序。没有自动 parent/bridge 递归；测试控制器覆盖父服务保活和枚举交错，但不证明真实硬件退出安全 |
| T-07 | provider 在"驱动检查依赖"与"域记录 Deferred"之间注册，唤醒丢失，设备永久 parked | 中 | 启动期时钟 provider 与消费者并发探测 | **已实现**：`dep_generations` 对每个 `DepSource` 计数，**即使无设备 parked 也自增**；提交 Deferred 时发现 generation 前进就重新入队。测试注入 `mid_check_hook` 精确复现该窗口 |
| T-08 | 依赖永不发布，设备静默停在 Deferred，故障不可见 | 中 | 固件引用了一个不存在的 clock phandle | **已实现**：`finish_probe_phase(false)` + `deferred_report()` 在探测阶段结束时把卡住的设备 `log::warn` 出来并降级为 `Unclaimed`。实测启动日志可见 `device 70 gave up waiting on dependency clk:66 (provider never registered)` |
| T-09 | 后注册的驱动抢走已绑定设备 | 中 | 驱动注册顺序变化 | **已实现**：队列探测仅处理 Pending；显式 bind 对 Bound 不变；候选按注册顺序匹配而非跨驱动评分，成功即停止；attempted 避免同轮重复选择失败驱动，显式重绑可清除该历史 |
| T-10 | 外域或同键不同对象被误认为本域设备 | 中 | 管理调用、控制器挂载 | **已实现**：内部键不导出；remove/snapshot/with_data/控制器挂载在域锁内核对 Arc 身份。测试 `management_requires_the_registered_object_not_just_its_key` 和 `devices_have_distinct_object_identity_across_domains`；旧引用不会影响同位置新注册 |
| T-12 | `DeviceDescriptor` 的 `compatibles: Vec<String>` 来自固件，超长或超多导致枚举期分配放大 | 低 | 恶意/畸形 DT | **部分实现**：`driver-init` 侧的 `FirmwareNode{id}` 分配器有 u32 耗尽拒绝；本 crate 不限制字符串长度或 compatible 条数。**残余风险**：启动期内存放大，不影响运行期。加固建议：在 `firmware/of` 解析处限长 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|---|---|---|---|---|---|---|
| F-01 | `add_device` 被拒 | 管理工厂/入口失败，或父设备、bus instance 校验失败 | 该设备不进入模型 | 枚举方获得错误，决定是否继续枚举其余节点 | 3 | 返回结构化 `AddDeviceError`；管理失败不发布设备 |
| F-02 | 探测返回 `Fatal` | 驱动判定自己不该认领（如 virtio-blk 发现 `VirtioDeviceKind != Block`） | 驱动进 `attempted` | 同一轮换下一个候选；穷尽则 `Unclaimed` | 4 | 正常控制流，`log::debug` |
| F-03 | 探测返回 `Deferred` | clock/irq-domain/gpio/regulator provider 未就绪 | 设备 parked | provider 发布时被精确唤醒；阶段结束仍未唤醒则报告 + 降级 `Unclaimed` | 3 | 见 T-07 / T-08 |
| F-04 | 未安装管理服务就注册 | NoServices 或宿主未覆盖默认工厂 | 工厂返回 Unsupported | 无 entry、拓扑链接或 pending probe 被发布 | 2 | add_device 返回 Management(Unsupported)，必须先安装可用工厂 |
| F-05 | MMIO 访问越界 / 未对齐 | 驱动偏移算错 | **panic** | 内核 panic（`panic = "abort"` 配置下即停机） | 2 | 刻意选择 panic 而不是 UB 或静默截断；panic 消息带 vaddr、offset、要求对齐 |
| F-06 | 管理入口拒绝 | 非可睡眠上下文或同任务重入 | 回调不执行、绑定与注册不改变 | 调用方获得入口错误 | 3 | RuntimeError::Management 保留 InvalidContext/Reentrant；批量探测计入 rejected 并重新入队 |
| F-07 | `Data` downcast 失败 | 域把 binding 交给了错误的驱动（只能是本 crate 的逻辑 bug） | **panic** | 停机 | 1 | 不变量 3 保证不可达；panic 优于把 `Data` 当错误类型解释（那会是类型混淆） |
| F-08 | 同设备 probe 期间请求解绑/注销 | 合法跨任务竞争或同任务重入 | 跨任务等待 probe 提交后再清理，同任务返回 Reentrant | 不撤销正在执行的 probe；无未提交结果回收协议 | 3 | 设备管理锁，见 T-04 |
| F-09 | 固件节点 id 空间耗尽 | 畸形 DT（>4G 节点，实际不可能） | 后续 DT 设备不再枚举 | 已枚举的设备正常工作 | 4 | compose 侧 `next_firmware_node_id()` 拒绝并 `log::error` 一次，不回绕 |

## 故障管理

- **错误词汇刻意分成两套**，防止控制流被混淆：
  - `ProbeError`（`Deferred(DepSource)` / `Fatal(RuntimeError)`）——probe 的
    **控制流**，域据此决定 park 还是换候选；
  - `RuntimeError`——服务调用及管理入口失败，`Management` 保留宿主入口错误。
    `ServiceError` 在两者之间做转换，让"某一次获取被 defer"能折叠成
    "这次 probe 被 defer"，而不需要驱动自己写 `should_retry()` 式判断。
- 域 API 一律返回 `Result<_, RuntimeError>`，不 panic——除了三处刻意选择：
  MMIO 越界/未对齐（F-05）、`Data` downcast（F-07）、以及 id 分配器溢出
  （u64，实际不可达）。三处都带 `CG-021-no-panic-on-reachable-failure`
  例外说明。
- 域短锁不可重入；宿主管理锁必须显式拒绝同任务重入。不承诺从内核 abort 恢复。
- DriverOps::remove 不返回错误；正常返回后继续 drop Data。unbind 恢复 Unclaimed，
  remove 删除 entry；模型没有下游递归失败续做协议。

## 隐私分析

无。本 crate 不处理任何用户数据、不接触用户内存、不产生日志之外的可观测输出。
`DeviceSnapshot::identity` 会包含固件 compatible 字符串与 PCI vendor/device id，
这些是硬件描述，经 `/proc/devices`、`/proc/bus/devices` 暴露给本机 root——
与 Linux `/proc/bus/pci/devices` 的暴露面等价，不含用户信息。

## 已知限制

1. **新模型无 threaded IRQ 入口**。`IrqAction` 只有 `Handled`/`NotHandled`；
   `driver_os_api::IrqOp`、`DeviceServices` 与 `DeviceAccess` 均没有线程化注册入口。
   普通设备 IRQ handler 不得阻塞或作为 pseudo-NMI handler 使用。
   Linux `devm_request_threaded_irq` **禁止机械映射**。
2. **无 MSI-X per-vector 暴露**。`driver_os_api::MsiResource` 有分配接口，`DeviceAccess`
   没有方法。旧 virtio transport 使用的是旧资源模型，不代表新模型已接入。
   driver-os-api 的 provider 与 model 的设备服务均使用 `Fn()->IrqAction`；宿主负责内核回调转换。
3. **无 runtime PM / suspend-resume**。`DriverOps` 只有 `probe`/`remove`，
   **没有** `suspend`/`resume` 预留位。
4. **无热插拔再枚举入口**。`add_device` 是增量的，但没有旧模型
   `DeviceManager::rescan()` 的等价物；bus instance 级别的 rescan 也未实现。
5. **设备快照不提供 class 分类**。`DeviceSnapshot` 返回设备引用、总线类型、
   生命周期状态、绑定驱动名和总线身份描述；按 class 分类的观测面由具体子系统提供。
6. **provider 家族仍是闭集**。`ProviderServices` 的 host 实现只认 `DepKind::Clk`
   对应的 `ClkProvider`；新增 gpio/regulator 要在 `driver-host` 增加匹配分支与其
   `ProviderClass` 描述符，但**不需要**新注册路径、新 wake 机制或驱动侧新 API。
   未知家族返回 `RuntimeError::Unsupported`（响亮失败，不静默丢弃）。
7. **probe 批次仍有集合复制和字符串生成成本**。队列分派只克隆共享核心的
   Arc，不再复制描述符或 `fw`；attempted 集合仍按批次复制，`modalias` 每次 probe 重算。
   probe 非热路径，本轮不改变匹配策略。
8. **`PlatformBus` 的参考实现住在本 crate**（`src/bus/platform_bus.rs`）。
   它应该在独立的 `bus-platform` crate 里；只要枚举只由组合根做，暂时留在这里。
9. **旧 class 接入尚未完全退役**。unbind 保留 DeviceCore online，
   独立来源已固定当次驱动身份，但驱动仍须撤销发布并排空操作，不能把管理锁当成
   跨 class drain。下游枚举交错的测试控制器不替代真实总线的硬件验收。

## 审计清单

改动本 crate 时逐项检查：

- [ ] `grep -rni 'virtio|nvme|ahci|mmc|i2c|usb' src/` 是否仍只命中注释？（L0-a）
- [ ] 注册表/队列/状态是否由域短锁保护？是否始终先进入设备管理锁再取得域锁？
- [ ] 注册校验与入表是否原子化？matching 快照后的提交是否重新核对状态？
- [ ] 驱动回调、matching、visitor 和 Data 析构是否都在域锁外？锁内分配是否必要？
- [ ] 新的 teardown 步骤是否遵守 `quiesce → 撤发布 → 释 IRQ → drop transport
      → unmap` 的顺序？类发布侧是否有在途 `Publication` 需要 `finish_or_defer`
      延迟 `hooks.unpublish`？
- [ ] 新增的 guard 是否 `take()` 掉 `ReleaseHook`（保证至多释放一次）？
- [ ] 新的 `DeviceState` 迁移是否在所有并发入口（probe 提交、unbind/remove、
      wake）下都被考虑？调度是否只改变允许的非 Probing 状态，管理是否持设备锁？
- [ ] 新的失败模式是 `Deferred` 还是 `Fatal`？分类错了会导致设备永久 parked
      或过早 `Unclaimed`。
- [ ] 新的 `unsafe` 是否有紧邻的 `SAFETY:` 说明，并写清哪个 safe 入口保证不变量？
- [ ] 新的公开 API 是否有 rustdoc（含 `# Errors` / `# Panics` / `# Safety`）？
- [ ] `ut-tests/domain.rs` 是否覆盖了新的状态迁移？特别注意：涉及关联/ID 的修复，
      回归测试必须覆盖那条关联链本身（`docs/driver-framework-review.md` §8.4 教训 2）。
- [ ] 是否引入了对某个设备家族的命名？如果是，应该改成 `ExtensionService`。
- [ ] 新增的公共名词能否归入 Device / Driver / Bus / Resource / Class /
      Provider / Host？不能归入就是设计问题，不是命名问题。

## 注册与在途操作的资源约束

PublicationSource 不再提供 mark_offline。model 在注销的 remove 回调前关闭来源，
class 的独立撤销仅关闭该发布。InUse 只直接延迟 class unpublish hook，不延迟
Data 的独立字段释放；驱动必须先停止并 join 操作，或由共享 runtime 保有资源。
类型拆分不等同于跨所有 class 的自动资源 drain，当前模型不提供此保证。
普通 unbind 保持设备在线；R1-04 的来源隔离不替代显式注销。旧 class 链路尚待退役。

## IRQ 基础词汇边界

model 经 driver-os-api 与 kirq 共享 irq-types 的三个值类型，不共享注册表、handler 或资源状态机。
IrqDomainId 只是宿主域命名空间中的编号，构造不代表注册成功或有效性校验；
宿主映射实现必须验证来源和域。model 不依赖 kirq 或旧 device_res。
边界目前由代码审查保证，不存在专门的自动边界检查脚本。
