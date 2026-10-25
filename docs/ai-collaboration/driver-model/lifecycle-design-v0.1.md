# 生命周期与真实接入：设计细化 v0.1

状态：候选，承接 [职责设计](design-v0.1.md)。本稿是机制分析，不代表实现或完整设计验收。
最新结构与命名候选见 [结构设计 v0.2](architecture-v0.2.md)。异步收尾优先复用
已核对的 kfinalize，不再默认新增 driver 通用执行系统；本稿早期候选以该细化为准。
最新许可/收尾交错与重入规则见 [退出协议](exit-protocol-v0.1.md)；其中已撤回
自动拒绝所有重入的过度承诺，改为 Linux 一致的调用前置条件，不新增任务标识系统。
需求依据为已固定的 requirements-v0.2；未引入叶子设备限制或弱化真实解绑要求。

## 1. 本轮核对的事实

- `drivers/base/class/src/publication.rs`：Publication 的 Arc 保留整个 runtime；
  Registration Drop 撤销发布但不等待在途使用，最后一个 InUse 可能直接执行 finalizer。
- `drivers/base/class/src/class.rs`：撤销先移除注册项，后关闭单项准入，回调在表锁外执行；
  当前没有显式等待 class teardown 完成的接口。
- `drivers/devices/virtio/src/rng.rs`：明确不支持移除/重绑定；正在 DMA 的请求析构时
  保留资源避免释放后使用。该保底泄漏不能作为正常解绑方案。
- `drivers/integration/kdriver/src/driver_registry/virtio/mod.rs`：hwrng 通过 attach_legacy
  发布并 forget 注册权，不是新 model 的完整路径。
- `drivers/integration/kdriver/src/driver_registry/virtio/glue.rs`：RNG 注入旧 device_res IRQ
  provider、宿主 VirtIoHalImpl 和 RngCompletion。迁移不能只改 class 注册入口。
- `drivers/integration/kdriver/src/rng_completion.rs`：已有 PollSet/PreparedTaskWait 的
  注册后复查等待模式，可作为宿主等待适配依据，不应把 ktask 依赖直接带入 model。
- `drivers/subsystems/hwrng/src/class.rs`：当前仅提供 legacy 注册入口，内部 Class 私有且
  使用空 hook；尚不能声称已有新模型注册入口或 RNG 设备节点投影。

## 2. 分开记录注册状态与绑定进度

注册状态的逻辑转换：

```text
Registered → Unregistering → Unregistered
```

绑定进度的逻辑转换：

```text
Unbound → Probing → Bound → Unbinding → Unbound
              └→ 清理本次尝试 → Unbound（原因：无匹配/失败/等待依赖）
```

这些是逻辑状态，不要求照搬为两个公开 enum。等待原因和调度记录不再伪装为
设备存在状态；绑定数据仅在对应阶段存在，不形成独立可变的 driver/data/active 三份事实。

注销先提交 Unregistering 以关闭新增绑定和挂接，再协调已有操作及下游退出。
已经执行的 probe 必须完成失败清理，或由管理路径承接成功结果并立即退出；
不能因其提交资格失效就直接丢弃尚未停止硬件的 Data。
所有清理完成后才提交 Unregistered。对子对象的退出须按真实创建/资源依赖关系处理。

## 3. 管理操作的序列化

候选采用短状态锁加每设备独占操作认领，而不是持全局自旋锁运行回调。

1. 在短锁内校验设备归属、注册状态、当前操作，取得本次操作的唯一提交资格。
2. 解锁后执行驱动回调、宿主调用和可能睡眠的等待。
3. 重新持锁核对资格并提交结果；收集通知后解锁执行。

每个设备同时只有一个生命周期执行者，不同设备不因驱动回调而全局串行。
操作令牌只是模型内部提交凭证，不成为新的公共 DeviceId。
跨设备操作不嵌套持有多个短状态锁等待；先封闭新增关系、取得稳定快照，
再按真实依赖顺序处理，返回时重新校验。

普通任务的竞争请求可以等待当前执行者完成后重新校验；同一回调重入不得
等待自己。按退出协议第 6 节，这属于 Linux 一致的调用前置条件，不承诺通用
自动重入检测，不引入任务 ID 或回调注册表。

## 4. 发布准入与退出不是一个步骤

需要明确区分四件事：

1. **关闭准入**：本次绑定及其发布不再授予新操作许可。
2. **停止并唤醒**：驱动禁止新硬件提交，唤醒需要取消的阻塞调用。
3. **等待收尾**：已有许可、IRQ handler、工作项按各自协议退出。
4. **释放资源**：确认硬件和软件不再访问后释放 IRQ、队列、DMA、映射。

顺序不能写成“关闭准入 → 等全部许可为零 → 调用 remove”。否则阻塞读者
可能需要 remove 唤醒，退出方却等它先完成，形成死锁。

推荐职责：model 关闭本次来源后调用驱动 remove；驱动/子系统显式停机、唤醒、
撤销并等待其操作收尾，随后释放资源；model 最后提交 Unbound。
泛型框架不能自动猜出如何取消驱动 I/O，class 也不能代替硬件停机。

### 来源门与单项发布门

来源门只关闭本次绑定的全部能力；单项发布门允许驱动单独撤销一项能力。
它们表达不同权限，不是设备在线状态的重复缓存。

仅在 acquire_use 中查询来源 is_online，不足以让 model 得到全部使用已排空的
保证。进一步接口推演后，不把许可获取/归还塞进只读 PublicationSource：
class 拥有独立的共享准入作用域，model 为每次尝试创建并持有其关闭权。
class 获取单项发布许可及作用域许可，失败则归还已经取得的许可；成功 InUse
同时保有两者。作用域关闭与其许可取得线性化，退出仅等待已获准的操作。

来源只关联本次设备/驱动身份；共享准入对象拥有计数和完成条件，不强持有
Data 或 publication 集合，避免引用环。具体 Interface 见第 10 节。
这仍需连同 legacy 来源和全部 class 调用方评审，不只是给 is_online 加一个计数。

### 显式撤销完成与 Drop 保底

class 需要可以观察撤销完成的路径，以便任务上下文中的 remove 等待。
Registration Drop 可不等待在途使用，但仍是任务上下文的管理操作，需要完成
表项移除和通知，不能被宣传为任意上下文安全或同步注销。
最后一个 InUse 的释放只应做其上下文允许的收尾；可能阻塞的 hook/资源释放
交给显式退出任务或宿主延迟执行，不能无条件在消费者 Drop 上下文执行。

尚需决定并验证：现有 hook 所需上下文、同步撤销接口、完成通知失败处理和
最后引用释放约束。不得在这些约束未闭合时添加笼统的“自动安全清理”承诺。

## 5. 旧句柄保活不能拖住正常解绑资源

当前 Publication 持有 runtime，runtime 可以是 Arc<具体驱动对象>。因此就算
许可全部退出，旧句柄仍可使整个驱动对象继续存在。只 drop Data 不足以释放硬件资源。

推荐驱动运行对象将可关闭的硬件资源与长期保留的失效状态分开：显式停机
取走并释放队列、IRQ、DMA 等，遗留对象仅保留拒绝操作所需状态及元数据。
取走资源必须晚于硬件停止和在途操作退出，不能在仍可调用的对象中留下悬空引用。
不是要求每个驱动机械增加 Option，而是要求显式退出不依赖最后消费者引用释放。

## 6. probe 失败与延后探测

每次尝试具有独立来源；失败后永久失效，重试不重开旧来源。
驱动若在 probe 内已经启动 IRQ、DMA 或发布能力，必须在返回 Err 前完成
对应停止与回滚。普通局部变量 RAII 不自动等于停止硬件；必要的初始化回滚
由驱动私有的构造守卫承担，不让通用 model 猜测硬件协议。

发布时序已根据用户要求“Linux 模型怎么定，和他一致”核对并收敛：通用模型
不提供 probe 成功后统一提交发布的事务。由驱动/总线在具备运行条件时显式
调用子系统注册，实际可见性和使用准入由相应子系统的注册协议决定。不能向
调用者保证 probe 返回前消费者不可访问；注册调用返回前也可能发生初始化、
通知或并发使用。发布之后仍然失败，必须撤销发布并安全收尾已经开始的使用。

源码证据：Linux `drivers/char/hw_random/core.c:562` 的 hwrng_register 直接加入
列表并可能选择为当前随机源；`:96` 的 set_current_rng 可以启动 hwrng 线程。
`devm_hwrng_register`（`:651`）立即调用注册，仅将注销记录加入 devres，不延迟
可见性。`drivers/char/hw_random/bcm2835-rng.c:182` 在 probe 内执行这一注册。
driver core 在回调返回后才继续最终绑定处理（`drivers/base/dd.c:710` 起），没有
统一缓存各子系统发布的步骤。

特例必须保留准确解释：本地 VirtIO RNG 在 `virtrng_scan` 注册 hwrng
（`drivers/char/hw_random/virtio-rng.c:223`）；virtio bus 在驱动 probe 成功且
设备 ready 后调用 scan（`drivers/virtio/virtio.c:347`）。这是该总线/驱动的
显式发布时机，不是通用 driver core 的发布事务，也不表示所有 Linux 驱动
都在自己的 probe 回调内发布。新 RNG 路径同样必须先完成运行准备再发布。

设计后果：采用可回滚的显式发布，不新增通用提交队列。发布失败与之后 probe
失败分别检查回滚；测试须包含消费者在 probe 返回前进入、随后发生失败的
交错。如何利用构造守卫及同步撤销实现完整回滚，仍需在机制设计中闭合。

延后探测的依赖代次与等待登记在同一协调协议内核对；登记前后依赖发生变化
时补排重试。唤醒仅产生待处理请求，执行前重新检查注册状态及当前尝试，
迟到通知不能使注销设备或旧绑定恢复。等待记录不强持有硬件资源。

## 7. hwrng 真实接入方案与硬件收尾

候选首条路径为 aarch64 QEMU VirtIO MMIO RNG；这是实施载体建议，不将其
扩大为同时迁移 PCI，也不删除旧 PCI 路径。具体切换点仍须追踪枚举所有权。

```text
现有发现入口（在旧绑定前选择迁移路径）
  → 新 model 设备注册/匹配
  → VirtIO RNG probe + 真实宿主 OS 能力
  → hwrng 正式发布入口
  → 现有 entropy 消费
```

hwrng 正式入口应继续返回家族注册凭证和操作型消费者句柄，不公开裸 runtime。
需要增加与 model 发布协作的入口，但不让 hwrng 反向依赖 model。
旧 IRQ 契约到 os-api 的适配不能产生两份注册所有权；HAL 的 DMA/MMIO 访问
必须核对设备语义，不能未经检查照搬全局默认 DMA 假设。

VirtIO RNG 必须新增实际停机协议：

- 在请求锁内进入停止状态，读路径此后不再 submit；解锁后唤醒所有阻塞读者。
- 读者检查停止条件并退出，不能收到一次 wake 后又无限等待下一次 IRQ。
- 停止设备队列访问并确认 DMA 已不再访问内存；IRQ handler 退出与注册释放
  必须由宿主保证同步，不能只屏蔽中断便释放队列。
- 等待软件使用收尾后释放资源，即使消费者还保留旧句柄也不保留活动硬件资源。

对实际锁定的 kvirtiodrivers 0.13.4 源码核对后，确认这里需要依赖层补充，
不能仅在 RNG 外层添加 stopped 标志：

- VirtIORng 的 transport/queue 私有，公开接口只有提交、正常完成及中断控制，
  没有显式停机及 reset 后取消未完成请求的入口。
- VirtIORng::drop 只调用 queue_unset；MMIO 实现和 PCI 实现并不等价，PCI 的
  queue_unset 是空操作。transport 自身 Drop 另有设备 reset，不能把“不支持
  显式取消”误述成“完全没有 reset”。
- 正常 pop_used 路径会 recycle_descriptors 并 unshare 请求 buffer/间接描述符。
  设备 reset 不会自动为未完成请求生成 used entry；仅丢弃 queue 不替代这些配对
  清理。现有 RNG 外层的 Pending 保底泄漏正是未完成退出协议时的安全兜底。

推荐在依赖库增加有明确前置条件的显式 shutdown/取消路径：先验证设备停止
DMA，再回收未完成 token 对应的映射与间接描述符，每项恰好一次；不伪造 used
完成，不让外层通过布局访问私有 queue。具体接口须连同 transport reset 完成
语义及 fake transport 测试一起审查。通过正式依赖源码/版本更新交付，不修改
Cargo registry 缓存冒充项目补丁。正常解绑不以 emergency leak 作为完成条件。

## 8. 系统投影和 provider 的边界

保留既有 device_node/sysfs/uevent 基础设施，但不能为复用旧 DeviceId 而注册
一个影子旧设备。核对 `drivers/contracts/kdevice/src/device_node.rs` 后，当前
DeviceNodeClass 仅含 Input/Drm，注册接口接收旧 DeviceId；hwrng 使用空 class
hook，没有通过这条路径注册 RNG 节点。因此本阶段不为了假定的 /dev/hwrng
兼容而新造节点或强迫搬迁整个 device_node。

实际兼容要求是：原 input/DRM 节点与事件不退化，hwrng 现有 entropy 消费
保持正常；后续需要节点的子系统迁移时沿既有机制扩展来源关联，不把旧 ID
传播进新 model/class。仍须盘点通用设备投影路径，不能把“没有 RNG class
node”扩大为“整个旧框架不存在与 RNG 有关的可观察信息”。

当前 Environment::provide 返回无撤销凭证且要求最后一个可失败步骤调用，
不是可回滚 provider 模型。其 UT 说明已有抽象，但不能当成已支持动态 provider
退出的证据。当前 model 的 ProviderServices 使用者为 UT MockServices 和
NoServices，未找到生产驱动调用 env.provide。按已确认的 Q13，不为假想家族
提前实现通用可撤销 provider 系统；既有 UT 所展示的能力与限制须明确记录，
不能把静态 provider 留在“任何可解绑驱动都可随意发布”的公共承诺中。
控制器拓扑是独立问题，不能据此一律禁止控制器解绑。

## 9. 宿主同步、等待和迁移接线

### IRQ：复用同步机制，补齐契约，不能用屏蔽代替释放

现有 `drivers/adapters/xkernel/device-res/src/irq.rs` 调用 kirq 的
try_free_irq/try_free_irq_action。`arch/kirq/src/runtime/manager.rs` 的释放路径
先移除 action、防止新 dispatch 快照引用它，再等待在途 dispatch，最后完成
teardown。共享 IRQ 仅移除自己的 action，不为退出一个 RNG 禁用其他设备。

新 os-api 的 release_irq 目前只声明“释放”，DevIrq 的 ReleaseHook 也没有
明确同步与上下文保证。设计必须明确：成功完成同步释放意味着旧 handler 已
不能进入且已退出；调用处于可等待上下文，不能持 handler 所需的锁。返回错误
不得冒充释放成功后继续销毁运行资源。共享 token 的所有权不可复制。

现有 kirq 尚有明确 TODO：action 已移除之后，等待登记可能分配失败，路径会
panic；旧 provider 的 void release 接口也无法传出上下文错误。不能在新接口
文档中凭空承诺这些问题已解决。推荐将必要的等待准备移到不可逆操作之前，
为正常解绑提供成功可证明的同步释放；这属于依赖的前置修正，不由 model
复制一套 IRQ 同步机制。错误类型和资源保留路径须随适配设计闭合。

### 等待：由 os-api 表达能力，宿主复用已有机制

model/class 需要的是“等待某条件改变且不会错过通知”，不是自带调度器。
候选在 os-api 定义小范围等待契约，由宿主通过现有 PollSet/PreparedTaskWait
实现；不让 model/class 依赖 ktask、kwork，也不把 hwrng 专属 RngCompletion
反向提升为所有模块的依赖。

协议应是准备等待者 → 登记通知 → 再检查条件 → 休眠 → 醒后重查。条件由
生命周期/许可状态拥有者维护，通知不冒充状态。准备和登记可能失败，必须在
不可逆撤销前解决所需资源；不能关闭 gate 之后才发现没有安全等待手段。
notify 不在持状态锁时执行任意回调。最终许可 Drop 仅做计数/唤醒等允许的动作，
需要睡眠的 teardown 由已准备好的退出执行者负责。

这条能力同时服务管理操作串行化与发布排空，但不因此合并它们的状态或计数。
目前是契约方案，具体 Rust Interface 与重入判定仍待核对，不提前增加通用任务框架。

### 枚举分流：必须早于旧注册，不只是早于旧 probe

`drivers/integration/kdriver/src/enumeration.rs` 的 register_device_with_parent
已经调用旧 kdevice::device_desc_add_with_parent，然后才放入 pending；因此
仅在 probe_pending 选择新驱动仍然留下旧注册，不能满足单一管理者要求。

推荐在宿主发现结果进入旧注册入口之前分流目标 RNG：发现事实先作为输入
描述，迁移目标交新 model，其他设备沿旧入口。platform_backend 中的
virtio_mmio_registration 已可识别设备类型，可在这一发现流程与注册入口之间
接线。探测后的失败不能再自动回退旧模型二次绑定，除非完整注销且有明确切换协议。

发现阶段的临时 MMIO/transport 不应直接变成长期运行所有者；新 probe 按
契约重新取得运行资源。真实路径还须核对固件 DMA 视图、IRQ 映射与 transport
创建责任，不能仅把旧 DeviceDesc 字段重新包装后沿用全部旧默认值。

## 10. Interface 草案：权限分开，而不是再造元数据容器

以下名字是供设计 review 的候选，不修改现有 Rust 类型。接口以语义表达，
不先锁定泛型、trait object 或函数签名细节。

### 10.1 class 的共享准入与只读来源

| 角色 | 对调用者提供什么 | 保留什么 | 不允许什么 |
|---|---|---|---|
| PublicationSource | 查询本次发布的设备位置、驱动来源 | 关联权威身份对象 | 修改设备状态、获取运行权限、拥有 Data |
| PublicationScope（候选名） | 创建同次尝试的发布；关闭全组准入；准备并等待排空 | 单调关闭状态、组内许可计数、完成通知 | 管设备注册、驱动匹配、硬件 stop 或资源释放 |
| Registration | 单项撤销，以及撤销完成的等待入口 | 单项发布的唯一注册权 | clone 后复制退出所有权 |
| Publication | 获取 InUse、只读来源与可用性快照 | 已发布运行对象及准入关联 | 凭裸引用执行数据面操作 |
| InUse | 在许可范围内执行数据面操作 | 单项及组内已取得许可 | 重新打开关闭的发布/作用域 |

PublicationScope 属于 class，而不是 model 的第二套绑定：独立 class 调用方
也可创建它，model 每次探测创建一个并在失败/解绑时关闭。它不保存设备 ID、
驱动名或路径；不是之前删掉的 PublicationNode 元数据容器。
需要区分可克隆的内部准入关联与唯一的关闭权，不能让消费者拿到关闭接口。
来源 `is_online` 的含混职责由准入作用域接替；最终迁移接口需同步所有调用者，
不保留两份分别可修改的在线状态。

一个 publication 的准入和该次绑定全部 publications 的准入是不同事实：
前者支持单项撤销，后者阻止失败/解绑后遗漏发布继续运行。两级计数的作用
分别是单项 hook 收尾和整次尝试的操作排空，不是复制设备状态。

许可取得候选协议：先取得单项许可，再取得作用域许可；任何一步发现关闭
就回退，不调用 runtime。每一级在其短锁内同时检查 gate 和增加计数，不嵌套
两级锁；关闭也在对应短锁内修改状态。只有两级都成功才允许访问 runtime。
这样关闭后的新请求不能先看到计数零、随后又合法进入；计数归零唤醒发生在锁外。
模型不直接检查计数并自行释放资源，等待的是 class 对外提供的排空完成条件。

作用域关闭不等于从各 class 表中删完所有记录：注册权仍需由驱动/子系统撤销。
不因加入作用域就假定可以回收驱动遗忘、转移或泄漏的 Registration。

### 10.2 撤销分阶段接口

建议用具体动作而非一个含义不清的 cleanup：

- `prepare_drain`：在可睡眠任务上下文准备当前调用者的等待资源；失败不关闭准入。
- `close`：单调关闭准入，不等待、不执行驱动回调；重复关闭不重复收尾。
- `wait_drained`：使用已准备凭证等许可归零；只证明操作退出，不证明硬件 DMA 停止。
- 单项 `withdraw`：关闭单项 gate、移出注册表并发送移除通知；只执行一次。
- 单项同步收尾：排空后在规定任务上下文执行 hook，并发布完成结果。

准备凭证仅用于当前退出流程，不可跨任务随意转移，不成为公共设备身份。
等待者不能持有自己正在等待的 InUse，也不能等待同一回调的管理操作完成；
接口文档和调用路径必须说明这些前置条件，不能笼统声称可任意重入。

Registration Drop 在任务上下文完成关闭、表项移除和通知，不等待在途使用。
最后 InUse 的释放不应内联可能睡眠的 hook；显式任务收尾为正常路径，异步
保底复用发布时准备的 kfinalize 义务。通知结束之前保持该义务强引用，防止
fallback 越过通知提前执行 hook；详见退出协议第 3—4 节。

### 10.3 不可逆退出后的等待必须不再依赖临时分配

本轮进一步核对发现：PollSet::wake 会移除等待登记，因此简单复用
RngCompletion::wait_until 的循环不能证明不可逆退出后永远不会再次分配。
准备 PreparedTaskWait 也不等于已经准备 PollSet 的登记资源。

推荐退出专用的一次性完成协议：完成条件为“已关闭且计数为零”，每个作用域
只由未完成变为完成，绝不重置用于后续绑定。等待登记在 close 前完成，登记后
复查，只有终态触发完成通知。虚假唤醒仍复查终态，不能耗尽一次登记后又要求
分配；重复退出者可在操作认领前准备各自登记。其他线程先完成时，后到者直接
观察终态，不必再次安排收尾。具体宿主适配必须验证取消与 detached wake 的生命周期。

普通管理操作的完成通知同样属于那一次操作，不用“一个全局唤醒事件”代替
操作结果。不向 class 传递 ktask 对象；OS 等待/执行实现位于宿主适配层。

### 10.4 IRQ 与 VirtIO 依赖变更契约

| Interface | 成功保证 | 失败与上下文 | 所有者 |
|---|---|---|---|
| IRQ 同步释放 | 本 action 不再接收新调用，在途调用结束；共享线其他 action 保留 | 可睡眠任务上下文，不持 handler 所需锁；准备失败不撤销 action | kirq 实现，os-api 描述，host adapter 转换 |
| VirtIO RNG 显式 shutdown | 停止新提交，确认设备不再访问 DMA，回收未完成映射与描述符 | 必须可处理 Pending，无需等待设备再生成完成 IRQ；不能以失败后直接 drop 活动资源兜底 | 具体驱动协调，VirtIO 库实现设备/queue 操作 |
| reset 后请求回收 | token 与原 buffer 配对，仅回收一次，正常完成路径不再接管 | 仅在设备已停止且软件访问已序列化后调用；不伪造正常完成 | VirtQueue 内部能力，由 RNG 封装调用 |

不另加一套 model IRQ 工作计数来弥补 kirq 的缺口。VirtIO 库变更只补充实际
shutdown 所需能力，不顺手重写所有设备家族。正式依赖交付方式、reset 失败
与资源保留策略在库补丁设计中明确，不修改 registry 缓存。

### 10.5 正常退出与失败回滚的调用责任

正常路径：model 准备退出/取得操作权 → 关闭作用域 → 驱动停止提交并唤醒读者
→ 撤销各发布并完成通知 → 等待已进入操作结束 → 同步停止硬件/IRQ
→ 完成仍需访问 runtime/资源的 hook → 回收其依赖资源 → model 提交未绑定。

硬件 reset 与 IRQ 同步的具体相对次序由驱动协议决定，不能将上述描述当成
所有设备通用的寄存器操作顺序；共同约束是释放晚于所有 DMA/IRQ/软件访问终止。
RNG 在等待读者前先设置停止条件并唤醒，不等待新的硬件完成来结束取消。

失败路径使用同一个具体驱动停机/撤销逻辑：初始化守卫在开始硬件活动前准备
必要退出能力，发布后任何失败先关闭准入、唤醒和收尾，再将 Err 返回模型。
probe 返回 Err 不会得到一次补偿性 remove；因此不能把未完成回滚丢给 model。
若需要睡眠，清理必须在 probe 的任务上下文显式完成，不能把该守卫跨 IRQ
传递后指望任意上下文 Drop 安全。正常路径成功后把唯一所有权交给 binding Data。

## 11. 前置变更与验证顺序（非开发 tickets）

1. OS 等待/完成能力：验证准备失败无副作用、关闭前后通知、虚假唤醒、无二次分配。
2. class 显式撤销/排空：验证两级许可、单项与组关闭、旧句柄保留、hook 上下文。
3. kirq 同步释放准备协议：验证 allocation failure 不在 action 已移除之后失控，
   共享 IRQ 其他 action 继续工作，handler 与释放竞争安全。
4. VirtIO 库 shutdown：fake transport 覆盖 Pending、间接描述符、IRQ 交错、重复清理；
   正式依赖版本/补丁可复现，再进入真实设备退出验收。
5. model 绑定协议：接入前述契约，验证 probe 失败/注销竞争、延后探测及重新绑定。
6. hwrng 真实链路：旧注册前分流，接真实宿主资源，验证 entropy、多实例和反复解绑。

上述是设计依赖关系，不表示允许单独修改其他子系统或发布外部 issue。
每项后续拆票需列清改动边界、依赖和验证，按需求保留结构重构与行为变更分离。

## 12. 验证与剩余事项

设计测试应包括：注销与 probe 提交交错、来源关闭与许可获取交错、读者需要
remove 唤醒、旧句柄长期保留、失败发生在发布之后、多实例退出隔离、依赖
通知迟到。分别检查停止准入、清理次数、顺序、资源回收和其他设备继续运行。

进入可实施完整设计前还须关闭：

1. 已确定显式发布时序；继续闭合可回滚构造与同步撤销机制。
2. 等待/重入识别/完成通知的具体 Interface，class hook 和最终 Drop 上下文。
3. 已证实 VirtIO 缺少未完成请求取消入口，且 kirq 同步释放有等待分配失败缺口；
   需给出正式依赖变更方案及验证，不以新框架包装掩盖。
4. 已定位旧注册前的枚举分流位置及 RNG 无现有 class node；继续核对通用投影、
   DMA 视图和控制器/下游责任。provider 当前无生产使用，不扩张为全家族实现。

本稿已排除“关一个在线标志”“等 Arc 全部释放”“先排空再唤醒读者”三个
不成立的简化方案，但不声称以上缺口已经解决。未执行代码或 QEMU 验证。
