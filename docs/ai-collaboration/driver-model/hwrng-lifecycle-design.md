# hwrng 生命周期与资源归属草案

状态：责任分层已确认；已完成第 13 节命名/布局调整，生命周期行为尚未改造。
本稿更新 class-withdrawal-design.md 的分层方向：同步读取排空属于 hwrng，
不作为所有 class 的强制语义；DeferredArc 不是必需依赖。

## Linux 目标模型复核（2026-10-24）

用户明确要求以 Linux 驱动模型为目标，而非仅参考其部分实现。
核对本地 include/linux/device/class.h、include/linux/hw_random.h 和
drivers/char/hw_random/core.c 后，须进一步修正术语：

- Linux class 分类和组织 struct device，关联属性、devnode、uevent 等行为；
  它不是按任意 T 泛型构造的能力注册、使用许可和业务排空容器。
- Linux hwrng_register 管理 struct hwrng 的子系统列表、使用引用和清理完成；
  hwrng 核心通过 misc_register 提供公共设备入口，并非每个 RNG provider
  都注册为一个独立的 hwrng class device。
- 因此现有 Class<T>/Publication<T> 不能直接宣称对应 Linux struct class。
  即使移出使用计数，仅留下泛型能力注册表也不自动成为 Linux 的 class。

下一步先把“设备分类关系”与“子系统服务注册”在总设计及 tickets 中分别表达。
hwrng 的注册表和消费者生命周期先由 hwrng 拥有；不因原代码存在泛型 class
就保留一层通用能力发布框架。真正的 class 依据设备模型及具体节点需求设计，
不为迁移 hwrng 凭空实现全部 sysfs/misc 功能，也不擅自改变已有可见节点行为。
现有类型迁移、命名和 crate 划分须在这项对应关系明确后决定，不做机械改名。
以下历史“通用 class 管发布注册表”的表述受本节修正，不视为已冻结实现。

## 1. 责任与引用

- model 保有成功绑定，串行调用 remove，完成后释放绑定数据。
- 通用 class 管理发布注册表及可发现性，不规定所有子系统的使用排空协议。
- hwrng 管理消费者读取准入、在途操作及同步 unregister。
- 具体驱动管理停止状态、阻塞读取退出、IRQ、队列和 DMA。
- 消费者引用只保活对象，不取得退出权；旧发布永久关闭，不连接新绑定。

优先用普通 Arc。最终选择须审计整个析构链：只有存在必须睡眠的最终析构，
且最后引用可能在不可睡眠上下文释放时，才论证 DeferredArc 等执行方式。
不能把延迟析构当作未完成显式停机的替代品。

## 2. 当前代码证据与目标

| 当前字段/关系 | 当前行为 | 目标责任 |
|---|---|---|
| HwrngDeviceImpl = Arc<dyn HwrngDevice> | 发布持有设备对象 | 旧句柄可保留停止后的对象，不拖延硬件解除 |
| VirtIoRngDev::request | SpinNoIrq 保护请求状态、VirtIORng 和 buffer | 驱动在同步退出时完成硬件停止及资源回收 |
| VirtIoRngDev::irq_guard | Option<Irq>，析构会同步注销 IRQ | remove 保活设备，取走 guard 后在锁外同步注销 |
| IRQ handler 的 Weak 升级 | 回调临时持有设备 Arc | 管理者在 IRQ 同步注销完成前保有强引用 |
| VirtIoRngDev::completion | 注入的读者等待能力 | 停止时通知；等待谓词必须观察持久停止状态 |
| HostRngCompletion::waiters | 等待节点由等待栈持有 | 读者退出后不残留等待节点；不拥有 DMA 或请求 |
| RngRequest::Drop | Pending 时记录错误并保留资源 | 正常退出必须解决 pending，不把该紧急保留当成退出方案 |

当前 IRQ 回调升级 Weak 后可能成为临时强引用持有者；仅仅改最外层 Arc
不会改变内部设备 Arc 的析构上下文。管理者保活并显式同步注销是正常路径
的必要条件，不能靠 finalizer 掩盖所有权缺口。

## 3. hwrng 正常退出候选次序

1. model 保留绑定数据，驱动进入 remove。
2. 驱动在与请求提交相同的同步保护下设置 stopping，之后不能提交新请求。
3. 释放锁后唤醒读者；read 的谓词检查 stopping 并返回离线错误，不能再次等待。
4. hwrng unregister 撤下发布、关闭新操作准入，等待已有操作结束并完成
   子系统清理；具体诊断查询若访问运行状态，也纳入相应使用保护。
5. 驱动按硬件协议停止 DMA、同步注销 IRQ、回收队列和请求资源。
   IRQ 同步等待不持有 handler 需要的锁，资源释放必须晚于最后硬件访问。
6. remove 返回，model 结束绑定。新 probe 使用新的 runtime 和发布。

步骤 5 内 reset、IRQ 摘除、token 回收及队列销毁的精确次序，必须检查固定版本
kvirtiodrivers 0.13.5 的能力和 transport 契约后确定；尚未以泛化顺序冒充证明。
业务读者退出不等于设备已经停止 DMA，不能将步骤 4 当作释放 DMA 的依据。

## 4. 剩余析构与未决点

退出后允许保留停止标志、身份引用、空的资源状态和等待对象。
但 HwrngDevice、PublicationSource、等待 provider 都可能包含自定义析构，
必须检查具体实现及允许最后释放的上下文，不能仅凭字段是 Arc 宣称 IRQ-safe。
现有 host 等待 provider 只存 WaitQueue，不包含硬件资源；其完整析构上下文
及未来 provider 的契约仍需在接口定案时明确。

未退出就销毁成功绑定的管理凭证属于 bug，不提供后台补做 remove。
probe 失败的构造回滚是合法路径：特别要覆盖 IRQ 已注册、发布尚未成功时的
锁外同步注销；不能把错误返回等同于可以任意顺序自动析构。

## 5. 验收要求

- stopping 与提交并发时，停止线性化之后没有新请求。
- 阻塞读取被停止通知唤醒，且不依赖未来硬件中断才能返回。
- unregister 等待已有操作，不等待闲置消费者句柄销毁。
- handler 在飞时同步注销确实等待；退出管理者始终保活对象。
- pending 请求通过硬件协议收尾，而不是落入现有泄漏式紧急保护。
- 保留旧句柄完成解绑重绑，旧调用失败，新调用正常；旧对象析构不影响新绑定。
- probe 各阶段失败都执行合法回滚；未退出 Drop 单独作为生命周期 bug 验收。

下一步先核对 VirtIO 库的 reset/token/队列退出能力，再确定具体资源布局和接口。
不先新增通用退出框架，不默认引入 DeferredArc，不修改生产代码。

## 6. 固定版本库核对结果

已读取 Cargo.lock 固定的 kvirtiodrivers 0.13.5 本地源码，而非其他版本：

- VirtIORng 私有持有 transport 与 queue；公开请求回收接口仅有
  complete_request_entropy，需要观察到对应 used entry；NotReady 保留 outstanding。
- VirtIORng 没有显式 shutdown 或停机后取消 outstanding token 的公共接口。
- VirtIORng::drop 调用 queue_unset；MMIO modern 实现清 queue_ready 并读回等待，
  legacy 实现清队列寄存器。MmioTransport::drop 写空设备状态。
- VirtQueue 的 descriptor 回收函数是私有的，公开 pop_used 不是无完成取消接口。

因此不能直接断言“库完全没有停机动作”，也不能把现有 Drop 当成已经提供了
上层所需的显式停机及 token/DMA 映射回收契约。具体 transport 的保证还需验证。
RngRequest 当前在 Pending 时保留资源，正说明不能靠提前丢弃等待者来结束请求。

该缺口归已有 Ticket 10（VirtIO 停机与 outstanding 请求回收），不是 base/class
应增加的能力。库需要形成确认停机后回收未完成请求且恰好解除映射一次的接口，
不能伪造成功完成或把取消数据当作熵。具体接口及 transport 保证在该票设计。
01B/hwrng 生命周期设计可先推进，真实解绑/重绑验收必须依赖这项能力完成。

用户已确认将该能力作为 VirtIO 前置需求补充；范围、依赖与验收条件已同步到
[Ticket 10](ticket-plan.md)。这是需求确认，接口和实现仍须在正式库源码中设计，
不授权修改 Cargo registry 缓存或向外部库推送。

## 7. hwrng 同步注销接口候选

先保留三个面向调用者的角色，不把驱动停机扩展为通用 class 回调：

- HwrngClass::register：发布前准备排空等待对象，返回唯一 HwrngRegistration。
  构造失败返回错误，不把半初始化发布暴露给消费者。
- HwrngPublication::read/is_failed：通过 hwrng 的私有准入计数访问 provider；
  返回前归还许可。metadata 查询不调用 provider，不因旧句柄存在维持使用计数。
- HwrngRegistration::unregister：可睡眠的管理操作；关闭准入、撤下注册表项、
  完成锁外撤销通知，等待已准入操作归零，然后返回注销完成。

名称仅为候选，具体签名与上下文错误表达在 01B 接口定案时确定。注销不接收
任意清理闭包、不增加新的后台任务、不默认向 HwrngDevice 添加 stop 回调。
驱动 remove 可直接调用其具体设备对象的停止读者方法，再调用 unregister。
停止读者方法负责持久停止状态及通知，不负责代替 VirtIO 硬件停机。

stopping 与 hwrng 准入不是重复事实：前者约束具体设备的硬件请求提交和等待
谓词，后者管理发布入口的调用权限和在途操作。两者不能各自推断对方已经完成。
设置 stopping 到关闭发布之间仍可能进入 read，但它必须观察停止状态并退出，
不能新提交或永久等待。因此不需要为这个短暂窗口增加跨模块原子关闭事务。

unregister 的返回保证是：此发布不能再进入 provider、已准入 provider 调用
全部结束、子系统注销动作完成；不保证设备对象析构，也不保证硬件 DMA 已停止。
等待必须在驱动锁和注册表锁之外；不能从本发布自己的在途调用中同步注销它。
重复注销不重复通知或执行清理；关闭的发布永不恢复，重新发布创建新对象。

注销完成标记由唯一管理者维护；共享状态只需维护准入与在途计数及排空通知，
不为消费者增加等待整个退出事务的接口。计数降为零只有在入口已关闭后才发布
永久排空通知，避免正常运行中的一次空闲使未来注销错误地提前返回。

本候选的验证增加：停止后、关闭前进入的 read 必须返回；先排空后等待不丢
通知；开放期间短暂计数为零不能使注销早返回；is_failed 与撤销竞争同样受保护。

## 8. 等待接入：先复用现有契约

已核对 subsys-hwrng::completion::RngCompletion：wait_until 接受不逃逸谓词，
notify 只触发重查，契约已有先通知、虚假唤醒及无分配登记要求；host 的
HostRngCompletion 已用 kcompletion::WaitQueue 和 ktask::CompletionWait 实现。
因此本轮 hwrng 排空不需要先新增另一套一次性完成 trait 或迁移内核原语。

候选接线：发布前注入一个独立的排空等待对象，复用同一 RngCompletion 契约及
host 实现；它不与驱动等待熵数据的对象共用通知源，也不持有设备资源。
对象在发布前构造，关闭后不分配 waiter。测试宿主实现相同谓词/通知契约。

共享私有状态只保存准入关闭标记和在途计数；准入与归还在同一短锁下更新。
注销等待的谓词是“入口已关闭且计数为零”，不需要另存永久完成标志。
最后操作归还时，锁内更新计数并决定是否通知，锁外 notify；正常开放期间
出现计数为零不代表注销完成。谓词在锁内读取，等待本身不持这把锁。
持有调用/注销引用覆盖通知动作，禁止在释放状态对象后再访问等待对象。

现有 wait_until 在不可睡眠上下文返回错误，不能忽略错误后宣称注销完成。
正常 remove 的可睡眠上下文是硬性前提；注销具体签名及违约诊断仍须按仓库
规则定案，不用新错误重试框架或未论证的 expect 掩盖这一点。

01B 应改为现有等待契约的接入与行为验收，而不是把新增 os-api trait 当作交付
目标。当前仅 hwrng 使用，不为未来消费者提前搬迁；若其他子系统需要同一契约，
再评审中立命名及归属。RngCompletion 命名和文档的准确性随接口改造一起检查。
验收复用真实 host：关闭前后归零、先通知后等待、虚假唤醒、数据可见性、
非法上下文及最后一次归还；不以简单立即返回的 mock 冒充等待测试。

## 9. 上下文错误与注销凭证（待确认的接口细节）

现有 RngCompletion 的 wait_until 返回 KResult，且初始化等待资源必须早于
第一次调用谓词。不能把这个错误直接忽略，也不能把它改称硬件停止失败。
候选 HwrngRegistration::unregister(&mut self) -> KResult<()>：成功才标记注销
完成；不消费凭证，错误不授权释放资源或返回正常 remove 成功。
这是低层接口的调用上下文错误通道，不向 model 添加失败绑定或硬件重试协议。

若要求在改变发布状态前拒绝非法等待，可先复用同一个等待对象执行恒真谓词
以验证当前上下文，但这会重复准备等待资源，也不能保证回调之后上下文仍正确。
因此不把此技巧作为默认实现，也不为解决它新增可转移 wait token 或准备事务。
当前候选允许错误返回时发布已关闭：凭证仍持有对象，不假装回滚或注销完成。

model 的管理入口应在调用 remove 之前验证可睡眠上下文；驱动不得跨自旋锁、
IRQ-off 或 RCU 读侧区调用注销。具体 remove 调用链如何满足这个前提，以及
遇到违反前提时的诊断方式，必须在实现评审中给出，不能仅凭 Result 签名声称
无错误返回的 remove 已闭合。未经这一步不得在调用方随意 expect 或吞掉错误。

私有状态只需分别记录：共享的准入/在途计数、唯一凭证的注销完成标记。
未完成注销就 Drop 是独立 bug；重复成功注销无副作用；重绑新建状态。
不扩展出通用 Failed/Retrying 状态，不恢复后台补做退出。

## 10. 现有泛型 class 的调用盘点与迁移范围

按工作树 Rust 源码及 Cargo 依赖搜索：driver_class 的直接依赖只有 driver-model
和 subsys-hwrng。不能把另一个旧 contracts/kclass 的调用混算进此 crate。

| 位置 | 实际用途 | 迁移方向 |
|---|---|---|
| subsystems/hwrng/src/class.rs | 唯一生产业务封装，注册表/句柄/读取入口 | 由 hwrng 自己拥有注册与同步注销，不搬入完整泛型框架 |
| integration/kdriver 的 virtio 注册分支 | attach_legacy 发布 RNG，forget 注册凭证 | 临时桥随接口迁移；Ticket 11 改为真实绑定持有并显式注销 |
| util/entropy/src/virtio.rs | 订阅后枚举、按 driver_name 识别、缓存句柄并读取 | 保留发现及操作能力，适配新 hwrng 接口；不依赖 base/class |
| model/src/device/access.rs | 公开泛型 DeviceAccess::publish 门面 | 不作为通用设备模型职责保留；驱动调用具体子系统注册 |
| model/src/device/mod.rs | DeviceCore 实现 PublicationSource | 随泛型发布解耦；设备身份查询保留，不把上线状态作为服务使用许可 |
| model/ut-tests/domain.rs | Class<u32> 集成与来源门禁用例 | 改为目标生命周期测试，不将旧提前关闭 source 的行为冻结成需求 |

现有非空 ClassHooks 的需求不能由测试样例证明；hwrng 当前使用空 hook。
当前泛型层承担的设备位置日志、来源查询和测试也不是保留通用发布框架的依据。
但日志/身份查询能力不能因迁移静默消失：真实注册使用权威设备/绑定关联，
临时 legacy 来源仅在过渡接线中保留，不新增生产 driver_name/path 快照容器。

entropy 实际依赖的行为包括 subscribe-before-enumerate、相同发布去重、失效
读取终止，以及替换已失败来源；不是仅替换一个类型名。保留旧句柄重绑时的
发现/选择顺序须列入集成测试，不能只验证新注册表能够返回新条目。

迁移顺序：先在 hwrng 完成具体注册/消费者/同步注销和测试，再适配 kdriver 与
entropy，随后移除 model 的泛型发布门面及其依赖。全树确认无调用后才删除
base/class 及 workspace 依赖；这不意味着删除或迁移旧 contracts/kclass。
真正 Linux class 的设备关联另按具体节点需求设计，不以本票空壳替代实现。
名称调整在实现前单独列出理由，不擅自替换用户尚未确认的公开类型名字。

## 11. 已确认的模块与公开名称

用户已确认以下命名及布局；本节统一替换前文候选中的旧公开名称。

```text
drivers/subsystems/hwrng/src/
  lib.rs           HwrngDevice 驱动契约及公开导出
  registration.rs  注册表、唯一凭证、消费者句柄、私有准入和注销
  completion.rs    现有宿主等待契约
```

| 原名称 | 确认名称 | 职责 |
|---|---|---|
| HwrngClass | HwrngRegistry | RNG provider 注册表，不是 Linux class |
| HwrngRegistration | 保留 | 唯一注册凭证，负责显式注销 |
| HwrngPublication | HwrngHandle | 可克隆消费者句柄，只提供元信息及受控操作 |
| HwrngDevice | 保留 | 具体驱动实现的 RNG 操作契约 |

注册、读取准入、操作归还和注销维护同一组生命周期不变量，先聚合在一个 mod；
不按类型拆文件，不把旧泛型框架整体搬进来。操作许可是私有实现细节，不向
消费者暴露 acquire/release；read 和运行状态查询自行取得并归还许可。
不迁入通用 ClassHooks。注销所需的具体通知遵循子系统契约，业务取消和硬件
停止仍由驱动负责；未来确有清理扩展需求再定义，不为旧接口制造使用理由。

调用迁移包括生产 kdriver/entropy、外置 UT、文档与公开导出路径。命名调整
和生命周期行为修改分开组织；本次确认不等于生产代码已经完成，也不授权推送。

## 12. 内部状态与首批验证

只保留三组真实状态，不新增公共 ID、泛用名称或退出事务：

| 所有者 | 保存的事实 |
|---|---|
| HwrngRegistry | 已注册实例列表及现有发现订阅 |
| 单次注册的共享对象 | provider 引用、权威来源关联、短锁保护的关闭标记与在途数、排空等待对象 |
| HwrngRegistration | 此次注册对象及所属表的引用、是否已完成注销 |

HwrngHandle 只引用共享对象，不复制 provider 名称/path，不持撤销权。
来源关联的具体接口随 model 解耦确定，不将旧 PublicationSource::is_online
继续当作准入条件，也不为赶进度复制一套可变设备元数据。
同一实例用引用身份比较；重绑创建新实例，无需新增全局 publication ID。

读操作在共享对象的短锁下检查入口、递增计数，然后解锁调用 provider。
私有操作守卫保证所有返回路径都归还计数。is_failed 同样受保护；metadata
查询不进入 provider。关闭后的空 buffer read 也不绕过已失效句柄的准入检查。

注销先在同一状态锁下永久关闭入口，再移出注册表并快照通知者；两把锁不嵌套。
表中暂时仍存在但已关闭的对象不再作为可用实例返回，持有旧句柄也无法准入。
通知与排空等待都在锁外；只有成功排空后才标记凭证注销完成。
重复完成注销无副作用；失败不恢复入口、不假装成功，也不重复发送已完成通知。
回调不得等待本次注销、持有同一实例操作许可后同步注销，或破坏调用者的
可睡眠上下文；这些约束须进入公开订阅文档和重入测试，而非持表锁执行回调。

首批测试安排：

1. 子系统外置 UT：注册/枚举、句柄身份、读取与状态查询、关闭后拒绝操作。
2. 受控并发：读者已进入时注销不能返回；最后读者退出后返回；旧句柄仍可持有。
3. 通知交错：先归零后等待、虚假唤醒、开放期间归零后又进入操作，均不早返回。
4. 同步边界：准入与关闭竞争、锁外通知、重复注销、等待错误不报告完成。
5. 集成接线：现有 host 等待实现、entropy 订阅后枚举与去重、旧句柄重绑隔离。
6. 迁移保护：probe 回滚、未注销 Drop 诊断及 model 移除泛型发布门面的测试替代。

当前 ScriptedCompletion 测试适合顺序契约，不作为真实阻塞/唤醒的证据。
并发测试采用实际 host adapter 与受控握手，不用固定 sleep/yield 次数判断。
命名/路径调整与生命周期行为改造分开验证；生产源码尚未修改。

## 13. 第一实施步：命名与布局迁移

已将 src/class.rs 移到私有 registration mod，采用 HwrngRegistry/HwrngHandle，
保留 crate 根导出 HWRNG 等类型；同步 entropy 生产调用、外置 UT 和模块文档。
此步仍委托 driver_class，Registration Drop 仍按旧行为撤销，没有新 unregister，
没有迁移操作计数，没有删除 base/class。前文“未修改生产代码”为设计阶段记录。

验证：aarch64 qemu_defconfig，独立磁盘，关闭网络/vsock；subsys_hwrng、entropy、
kdriver 共 63 项测试通过，0 失败/忽略，覆盖率导出成功；nightly-2026-03-08
fmt 检查通过。日志 /tmp/hwrng-naming-ut.log、/tmp/hwrng-naming-fmt-nightly.log。
make clippy 通过，日志 /tmp/hwrng-naming-clippy.log。
这些证据只证明命名调整的现有行为回归，不代表目标同步注销已经验收。
