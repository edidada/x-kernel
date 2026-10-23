# driver-model — 安全与可靠性分析

## 设备引用生命周期边界

回调 Device 借用适配层的 DeviceRef；DeviceInfo 只借用 Device，不增加强引用。
类型化 bus_data 只能从该引用的描述符构造，避免身份与总线数据来自不同设备。
这些借用不改变注销协议或 binding 资源释放时机；长期引用仍不授予硬件权限。

公开管理入口只接受设备引用，不再暴露 DeviceId 类型、数字查找或 id 访问器。
内部键定位后还必须核对 Arc 身份；remove 的身份校验与 Removing 声明共用域锁，
查询与 with_data 的校验和读取也在同一持锁区间完成。外域及同键不同对象返回
NotFound/None，且不调用驱动 remove 或数据访问 visitor。
快照与 deferred 报告携带对象引用，只保留身份，不维持硬件可用性。
总线实例保留原控制器身份用于诊断，离线控制器即使仍被引用也不能新增实例成员。

`DeviceRef` 保留身份和静态描述，不保留绑定 Data 或硬件操作权限。
注销使共享核心 offline；已绑定设备在锁外执行 remove 和 Data drop，
未绑定设备也关闭来源并删除域条目。外部引用不能阻止这些动作，
online 仅为快照，不能用作同步或在途操作许可。
已移除对象不会因同一路径的新设备注册而复活；对象同一性使用 Arc 指针比较。
描述符随最后一个核心引用释放，总线数据必须无硬件所有权且允许非睡眠析构。
销毁域前仍须显式完成注销，不把域析构当作驱动 quiesce 协议。

回归覆盖：`retained_reference_does_not_retain_binding_resources` 验证引用保留期间
remove/Data drop 各执行一次、描述符共享且移除后可读；
`unbound_removal_retires_identity_without_invalidating_metadata` 验证轻路径 offline、
引用 Drop 不注销、重复移除拒绝和同位置重新注册的身份隔离；
`device_removal_closes_source_before_driver_quiesce` 验证 DeviceRef 与 class 来源一致。

## 设备服务上下文

设备资源服务通过 `DeviceContext<'_>` 借用当前回调的身份，不接受裸数字键。
上下文只能由模型资源视图构造，不作为在线许可或同步屏障。资源释放闭包不得
捕获短期上下文；clone 身份只延长静态元数据寿命，不延长绑定资源。
UT 验证五类请求指向同一核心、正常注销的四类守卫各释放一次、DMA 失败与时钟
Deferred 时部分获取的资源立即回滚。未配置 DMA 的原有测试继续验证宿主未被调用。

## 总线归属校验

父设备关系使用 `DeviceRef`，但引用存活不授予注册孩子的权限。域在锁内核对
父条目与引用指向同一核心，并拒绝跨域、已删除、Removing/Removed 父对象；
校验与反向 children 更新不可分割，避免级联移除快照遗漏新孩子。
子设备仅强持有父设备，反向列表没有强引用；不支持修改已注册 parent。
保留离线孩子仅保留父设备元数据，不保留硬件资源。
回归覆盖跨域父引用、同键不同对象、Removing/Removed 拒绝、孩子摘链，
以及级联移除后父元数据可读、最终弱引用失效（无拓扑引用环）。

设备仅持有必填的总线实例标识。注册时在域锁内验证实例属于当前 domain，
并检查设备数据词汇的类型与实例类型一致；外域或不存在的实例不能注册设备。
匹配与快照读取实例中的总线类型，避免描述符与实例分别声明归属而发生漂移。
实例标识只是关联键，不是硬件初始化凭据；创建方和枚举方负责传递真实的拓扑。
控制器的 `Removing` 状态在获取级联快照前发布，`add_device` 在同一域锁内
检查它，阻止无 `parent` 的新成员绕过父子拓扑门禁。控制器删除或保留为
`Removed` 后，其实例也不能再接收成员。UT 覆盖 probe 中触发移除而暂时
返回 Busy、remove 回调期间以及最终移除后的挂载拒绝，并验证其他实例不受影响。

## Scope

`drivers/base/model/src/`：`bus/`、`driver.rs`、`device.rs`、`error.rs`、`resource.rs`、
`provider.rs`、`services.rs`（带设备上下文的服务契约）、
`domain/{mod,registry,probe,dependency,lifecycle}.rs`。
测试 `ut-tests/domain.rs` 不在分析范围内，但其中的 `MockServices` 是信任模型的
一个具体实例。

通用 class 已移到独立 `driver-class`，见 [class 安全分析](../../class/docs/security.md)。
模型保有私有 `DeviceCore` 和唯一设备在线状态，通过 `PublicationSource` 提供
只读身份查询。跨 crate 传递同一个 `Arc` allocation，共享实时来源状态；
同源多项发布独立注册，单项撤销不修改来源在线状态。class 不依赖模型或访问其私有字段。

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
| `DriverOps` 实现 → 域 | `drv/**` 里的驱动 | **可信内核代码**。域不防御驱动的错误，但把驱动能造成的破坏限制在"这一个设备"内（generation 守卫、独占移除声明） |
| `DeviceServices` 实现 → 域 | `driver-host::Host` 或测试 mock | **可信**。契约要求返回的 guard 携带正确的 `ReleaseHook` |
| 驱动 → provider 注册表 | `dev.env().provide::<C>(key, lookup)`（经 `ProviderServices`） | 半可信输入：`key` 与 lookup 自称的 id 必须一致（`Host` 校验，不一致返回 `Invalid`）；重复 key 返回 `AlreadyExists`，不覆盖已有 provider |
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
| 驱动宣告的 provider key | `Device::provide` | 与 lookup 自身 id 交叉校验；冲突即失败，不覆盖 |
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
4. **陈旧 probe 结果不会落进 entry**。提交前比对 `lifecycle.generation`；
   不等就 drop 掉那个 `Data`。没有这条，"拆掉再重加"的设备会被上一个 probe 的
   guards 覆盖，两个 `Data` 同时对同一份硬件负责。
5. **`DeviceState` 的迁移只在锁内发生**，且 `Removing` 只能由持有独占声明的
   调用者往前推。因此不存在"两个线程同时跑 teardown"。

## 线程安全

- `DeviceDomain` 是 `Sync`：内部一把 `SpinNoPreempt` + 一个 `SpinNoPreempt<bool>`
  泵标志 + 一个 crate 级 `AtomicU64` id 分配器。
- **禁抢占、不关中断**。因此所有域 API 只能在任务上下文调用。硬中断里调用会
  在同一把锁上自死锁。这条约束靠文档 + review 保证，类型上没有 barrier
  （与 `kdevice` 旧模型的限制相同）。
- `DriverOps: Send + Sync`，`DriverOps::Data: Send + Sync`——因为中断 handler 与
  work item 通过 `Weak` 共享 `Data`。
- `ReleaseHook = Arc<dyn Fn() + Send + Sync>`、`IrqHandlerFn = Arc<dyn Fn() ->
  IrqAction + Send + Sync>`：都要能跨 CPU 执行。
- probe 泵是串行的（`probing` 标志），因此同一时刻只有一个 probe 在锁外运行。
  这不只是性能考虑：`driver-host` 的 MMIO lease collector 是单槽的，依赖这个
  串行化。
- 移除可以与 probe 并发，但结局是确定的：`Probing` 拒绝移除（`Busy`），
  probe 提交时 generation 已变则丢弃结果。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|---|---|---|---|---|
| T-01 | 固件给出一个 `MmioRegion`，其 `base+size` 覆盖内核自身映射；驱动 `iomap` 后写寄存器 → 破坏内核内存 | 高 | 被篡改的 DT / ACPI，或固件描述与真实硬件不符 | **已实现**：driver-os-api 只按 `region.size` 做访问边界检查，不判断物理地址合法性；映射由 `driver-host` 经 `memspace` 建立，`memspace` 拥有物理地址空间的分配与重叠检测。**残余风险**：若 `memspace` 允许映射一段与内核镜像重叠的物理地址，本层无法发现。加固建议在 `driver-host`/`memspace` 侧拒绝已知保留区 |
| T-02 | 驱动用超出实际寄存器窗口的 `offset` 访问 `IoMap` | 高 | 驱动 bug 或转写错误 | **已实现**：`access_ptr` 用 `checked_add` 防溢出 + `assert!(end <= region.size)`，越界立即 panic 而不是静默访问相邻寄存器 |
| T-03 | 驱动以未对齐 offset 做 `read32`/`write64` | 高 | 转写 Linux 驱动时把寄存器偏移算错 | **已实现**：`access_ptr` 总是检查 `is_multiple_of(size)`，不满足即 panic。旧模型此处是 `debug_assert`（release = UB），本 crate 已修正 |
| T-04 | 设备被移除后，上一轮 probe 的结果被提交，导致两份 `Data` 同时对同一硬件负责（双重释放 IRQ / 双重 unmap） | 高 | 移除与 probe 并发 | **已实现**：`lifecycle.generation` 在每次 teardown 自增；提交时不匹配就 drop 陈旧 `Data` 并 `log::warn`。回归测试 `wake_during_probe_window_does_not_lose_the_device`；probe 期间
remove 的 `Busy` 拒绝由 `remove_retryable` 的独占声明在锁内保证 |
| T-05 | 同一设备被两条路径同时移除，teardown 跑两遍 | 中 | 热插拔事件与手动移除竞争 | **已实现**：`Removing` + `remove_retryable` 的独占声明——恰好一个调用者能在锁内翻回 `false`，其余看到 `Busy`；`Removed` 上再移除返回 `BadState`。测试 `removing_twice_rejects_with_bad_state` |
| T-06 | 父设备先于子设备被拆，子设备的驱动仍引用父设备的硬件（PCI bridge eject 后 endpoint 还在跑 DMA） | 高 | 桥设备移除 | **已实现**：`remove_device` 第 1 相先翻 `Removing` 关闭拓扑入口，第 2 相 children-first 级联（device children ∪ 桥接的 bus instance 成员）；任一子设备失败则**中止**，父设备停在 `Removing` 且 binding 完整。测试 `remove_cascades_children_first`、`parent_teardown_waits_for_probing_child` |
| T-07 | provider 在"驱动检查依赖"与"域记录 Deferred"之间注册，唤醒丢失，设备永久 parked | 中 | 启动期时钟 provider 与消费者并发探测 | **已实现**：`dep_generations` 对每个 `DepSource` 计数，**即使无设备 parked 也自增**；提交 Deferred 时发现 generation 前进就重新入队。测试注入 `mid_check_hook` 精确复现该窗口 |
| T-08 | 依赖永不发布，设备静默停在 Deferred，故障不可见 | 中 | 固件引用了一个不存在的 clock phandle | **已实现**：`finish_probe_phase(false)` + `deferred_report()` 在探测阶段结束时把卡住的设备 `log::warn` 出来并降级为 `Unclaimed`。实测启动日志可见 `device 70 gave up waiting on dependency clk:66 (provider never registered)` |
| T-09 | 后注册的驱动抢走一个已被别的驱动认领的设备 | 中 | 驱动注册顺序变化 | **已实现**：`select_batch` 跳过 `Active`/`Removing`/`Removed`；`attempted` 集合防止同一个失败驱动被反复选中。测试 `late_driver_registration_claims_unclaimed_device`、`late_driver_registration_skips_unmatched_unmatched` |
| T-10 | 外域或同键不同对象被误认为本域设备 | 中 | 管理调用、控制器挂载 | **已实现**：内部键不导出；remove/snapshot/with_data/控制器挂载在域锁内核对 Arc 身份。测试 `management_requires_the_registered_object_not_just_its_key` 和 `devices_have_distinct_object_identity_across_domains`；旧引用不会影响同位置新注册 |
| T-12 | `DeviceDescriptor` 的 `compatibles: Vec<String>` 来自固件，超长或超多导致枚举期分配放大 | 低 | 恶意/畸形 DT | **部分实现**：`driver-init` 侧的 `FirmwareNode{id}` 分配器有 u32 耗尽拒绝；本 crate 不限制字符串长度或 compatible 条数。**残余风险**：启动期内存放大，不影响运行期。加固建议：在 `firmware/of` 解析处限长 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|---|---|---|---|---|---|---|
| F-01 | `add_device` 被拒 | 父设备不存在 / 正在拆 / bus instance 缺失、数据类型不符或控制器不可用 | 该设备不进入模型 | 枚举方获得错误，决定是否继续枚举其余节点 | 3 | 返回结构化 `AddDeviceError`，五种原因可区分 |
| F-02 | 探测返回 `Fatal` | 驱动判定自己不该认领（如 virtio-blk 发现 `VirtioDeviceKind != Block`） | 驱动进 `attempted` | 同一轮换下一个候选；穷尽则 `Unclaimed` | 4 | 正常控制流，`log::debug` |
| F-03 | 探测返回 `Deferred` | clock/irq-domain/gpio/regulator provider 未就绪 | 设备 parked | provider 发布时被精确唤醒；阶段结束仍未唤醒则报告 + 降级 `Unclaimed` | 3 | 见 T-07 / T-08 |
| F-04 | 服务未安装就取资源 | `set_services` 之前调用，或某个域没有自己的 runtime | 所有资源获取返回 `RuntimeError::Unsupported` | 该域上所有驱动 probe 失败 → `Unclaimed` | 2 | 刻意的"响亮失败"：`NoServices` 是一个真实类型而不是 `Option::None` 静默路径；启动日志会显示 0 active |
| F-05 | MMIO 访问越界 / 未对齐 | 驱动偏移算错 | **panic** | 内核 panic（`panic = "abort"` 配置下即停机） | 2 | 刻意选择 panic 而不是 UB 或静默截断；panic 消息带 vaddr、offset、要求对齐 |
| F-06 | 级联移除中途失败 | 子设备正在 probe（`Busy`）或已在拆（`BadState`） | 父设备停在 `Removing`，binding 完整，拓扑入口已关 | 父设备不可再挂子设备，但硬件仍被正确持有；下一次 `remove_device` 可独占续做 | 2 | `remove_retryable` + `log::warn`（实测可见 `cascade remove of 77 failed (Busy); teardown ABORTED — retry to continue`） |
| F-07 | `Data` downcast 失败 | 域把 binding 交给了错误的驱动（只能是本 crate 的逻辑 bug） | **panic** | 停机 | 1 | 不变量 3 保证不可达；panic 优于把 `Data` 当错误类型解释（那会是类型混淆） |
| F-08 | 陈旧 probe 结果 | 移除与 probe 并发 | 结果被 drop，guards 就地释放 | `log::warn`，设备保持移除后的状态 | 3 | generation 守卫，见 T-04 |
| F-09 | 固件节点 id 空间耗尽 | 畸形 DT（>4G 节点，实际不可能） | 后续 DT 设备不再枚举 | 已枚举的设备正常工作 | 4 | compose 侧 `next_firmware_node_id()` 拒绝并 `log::error` 一次，不回绕 |

## 故障管理

- **错误词汇刻意分成两套**，防止控制流被混淆：
  - `ProbeError`（`Deferred(DepSource)` / `Fatal(RuntimeError)`）——probe 的
    **控制流**，域据此决定 park 还是换候选；
  - `RuntimeError`（errno 风格，9 个变体）——**服务调用**的失败。
    `ServiceError` 在两者之间做转换，让"某一次获取被 defer"能折叠成
    "这次 probe 被 defer"，而不需要驱动自己写 `should_retry()` 式判断。
- 域 API 一律返回 `Result<_, RuntimeError>`，不 panic——除了三处刻意选择：
  MMIO 越界/未对齐（F-05）、`Data` downcast（F-07）、以及 id 分配器溢出
  （u64，实际不可达）。三处都带 `CG-021-no-panic-on-reachable-failure`
  例外说明。
- 没有锁中毒概念：`SpinNoPreempt` 不可重入，重入即死锁（见 T-09 的相关分析）。
- 移除路径**不可回滚**：一旦翻到 `Removing` 且级联成功，即使 `DriverOps::remove`
  内部出错也继续走到 `Removed`（Linux 的 "remove never fails" 约定）。
  驱动的错误只记日志。

## 隐私分析

无。本 crate 不处理任何用户数据、不接触用户内存、不产生日志之外的可观测输出。
`DeviceSnapshot::identity` 会包含固件 compatible 字符串与 PCI vendor/device id，
这些是硬件描述，经 `/proc/devices`、`/proc/bus/devices` 暴露给本机 root——
与 Linux `/proc/bus/pci/devices` 的暴露面等价，不含用户信息。

## 已知限制

1. **新模型无 threaded IRQ 入口**。`IrqAction` 只有 `Handled`/`NotHandled`；
   `driver_os_api::IrqOp`、`DeviceServices` 与 `Device` 均没有线程化注册入口。
   普通设备 IRQ handler 不得阻塞或作为 pseudo-NMI handler 使用。
   Linux `devm_request_threaded_irq` **禁止机械映射**。
2. **无 MSI-X per-vector 暴露**。`driver_os_api::MsiResource` 有分配接口，`Device`
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
7. **probe 批次仍有集合复制和字符串生成成本**。`select_batch` 只克隆共享核心的
   Arc，不再复制描述符或 `fw`；attempted 集合仍按批次复制，`modalias` 每次 probe 重算。
   probe 非热路径，本轮不改变匹配策略。
8. **`PlatformBus` 的参考实现住在本 crate**（`src/bus/platform_bus.rs`）。
   它应该在独立的 `bus-platform` crate 里；只要枚举只由组合根做，暂时留在这里。

## 审计清单

改动本 crate 时逐项检查：

- [ ] `grep -rni 'virtio|nvme|ahci|mmc|i2c|usb' src/` 是否仍只命中注释？（L0-a）
- [ ] 新增的域状态是否落在 `DomainInner` 的**一把锁**下？有没有引入第二把锁
      从而重新需要锁序文档？
- [ ] 新增的 check-then-act 序列是否整体在锁内？（父设备校验、批选择、独占
      移除声明都是这类）
- [ ] 有没有在持锁期间调用驱动代码、做 MMIO 或分配内存？
- [ ] 新的 teardown 步骤是否遵守 `quiesce → 撤发布 → 释 IRQ → drop transport
      → unmap` 的顺序？类发布侧是否有在途 `Publication` 需要 `finish_or_defer`
      延迟 `hooks.unpublish`？
- [ ] 新增的 guard 是否 `take()` 掉 `ReleaseHook`（保证至多释放一次）？
- [ ] 新的 `DeviceState` 迁移是否在所有并发入口（probe 提交、remove 三相、
      wake）下都被考虑？是否需要 bump `generation`？
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

PublicationSource 不再提供 mark_offline。model 在 remove 回调前关闭来源，
class 的独立撤销仅关闭该发布。InUse 只直接延迟 class unpublish hook，不延迟
Data 的独立字段释放；驱动必须先停止并 join 操作，或由共享 runtime 保有资源。
类型拆分不等同于跨所有 class 的自动资源 drain，当前模型不提供此保证。

## IRQ 基础词汇边界

model 经 driver-os-api 与 kirq 共享 irq-types 的三个值类型，不共享注册表、handler 或资源状态机。
IrqDomainId 只是宿主域命名空间中的编号，构造不代表注册成功或有效性校验；
宿主映射实现必须验证来源和域。model 不依赖 kirq 或旧 device_res。
边界目前由代码审查保证，不存在专门的自动边界检查脚本。
