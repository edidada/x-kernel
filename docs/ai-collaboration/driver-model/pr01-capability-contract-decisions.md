# PR-01 能力需求与接口取舍

状态：需求/设计整理稿，待评审；不授权按旧 API 清单机械补齐，也不是已完成迁移。
基线：开发底稿 `2daaa4bc0`，已包含 main `f429a5953`。
范围沿用 [PR-01 交付计划](mainline-replacement-pr01.md)，transport 开发继续暂停。

**范围修正（用户已确认）**：本稿中 per-device DMA、guard/ReleaseHook、MSI
owner 等最终设计要求转为后续能力层内部演进事项，不再阻塞 PR-01 删除旧 crate。
PR-01 先统一到 os-api；已有重复能力只留一套，尚无等价新实现的必要能力先保留
主线语义迁入。本文的“未决门槛”约束的是对应重设计，不是单纯归属迁移。
TimeOp/threaded 等接口的去留可后续独立清理，首批不以删无消费者接口为必需条件。

## 1. 判断标准

先问消费者需要完成什么、谁拥有状态、何时结束生命周期，再确定接口。
旧接口不是标准答案；新接口也不因为“新”就天然正确。保留的是正确功能和
生命周期保证，不是所有旧名称、参数和便利方法。

证据分四类：生产消费者；已实现但当前未使用的能力；只有 Unsupported 默认
实现的预留接口；仅测试/未启用占位代码。后两类不自动形成必须新增功能的需求。
没有驱动使用某个接口，不代表底层没有相关机制，也不等于所有时间/时钟需求不存在。

Linux 仅用作职责与流程参照，不逐项复制其 C 接口。本文对照本地
`/root/linux-stable` revision `8d3ae59288f1`。

## 2. 必须满足的能力与职责

| 需求 | 最终职责归属（建议） | 不属于它的责任 |
|---|---|---|
| 描述设备提供的 MMIO/IoPort/IRQ 等资源 | 设备模型；基础能力词汇可来自 os-api | OS provider 不维护设备拓扑、枚举结果集合 |
| 获取并持有一个可释放资源 | os-api 的能力契约和独占 guard；host adapter 实现 | guard 不负责停止设备、不等于操作准入许可 |
| probe 失败/解绑时统一释放资源 | 绑定持有 guard；已有 kdevice cleanup 或后续 Data 所有权 | 不在 os-api 再建生命周期 registry |
| 在设备 DMA 约束下分配/映射 | 持有明确设备视图的 DMA 能力 | 不从当前任务/全局变量猜设备；不承担 CPU 普通存储 |
| 注册本 action，退出时同步 drain | IRQ 能力与 KIRQ；驱动先停止自己的中断产生 | 不关闭共享 peer，不把 drain 当硬件 DMA/reset 证明 |
| 普通 CPU staging buffer 可失败分配 | CPU memory 能力、原 provider 配对释放 | 不冒用 coherent DMA，不做地址转换/缓存策略 |
| OS 核心机制实现 | 单一 host adapter 调用 KIRQ/kdma/memspace 等 | 可移植驱动不得直连宿主 crate |

Linux 依据：`drivers/base/devres.c::release_nodes/devres_release_all` 在设备侧
维护并逆序释放资源；`drivers/base/dd.c::__device_release_driver` 调用设备 remove
及后续 cleanup。`lib/devres.c::__devm_ioremap_resource` 组合资源检查、认领和映射，
不能把 X-Kernel 当前仅映射地址的 API 宣称为完整 Linux 资源认领机制。

## 3. 新旧接口对照与取舍

### 3.1 MMIO / I/O port

- 需求：probe 获得有效映射，受约束地访问寄存器，最后释放本次 mapping reference。
- 旧 `MmioOp + Io` 通过 provider 归还 token；新 `IoMap` 持 release hook。
  二者都表达有价值的所有权，不能只因为闭包较新就认定它更好。
- 建议最终驱动持一种映射 guard。map/unmap 的原始 token 不作为可随意复制、
  伪造并重复释放的驱动接口。具体采用 provider+opaque token 还是独占 release
  owner，应由分配失败与生命周期条件确定，不同时保留两种公开获取协议。
- 保留边界/对齐与 volatile 访问约束；“Acquire/Release fence 等同所有架构
  readl/writel”不是本轮已证明的结论，需核对架构 MMIO ordering，不以更名代替。
- IoPort 是 x86 端口空间，不是 MMIO 地址；保留现有描述和访问路径，不强行套映射。

证据：`drivers/contracts/device-res/src/mmio.rs`、
`drivers/base/os-api/src/mmio.rs`、`drivers/integration/kdriver/src/host_mmio.rs`。

### 3.2 DMA：倾向采用新模型的设备视图，而非复刻旧 DmaOp

- Linux `kernel/dma/mapping.c::dma_alloc_attrs/dma_map_page_attrs` 显式接收 device，
  分配读取设备 DMA mask/ops。单有长度和对齐不能完整表达设备的 DMA 能力。
- 旧 `DmaOp::alloc_coherent(DmaSpec)` 方法未显式携带设备，当前全局 provider
  接线也未绑定设备视图；trait 本身并不禁止实现对象持有设备状态。
  `DmaAllocation/DmaMapping`
  暴露可复制记录，释放靠调用者重新提交原字段。新 `DeviceDma` 持稳定的 mask、
  window、coherency，返回非 Clone guard，更接近需要的所有权与设备约束。
- 建议保留 per-device capability 方向；统一 coherent 与 streaming 的获取/
  释放合同，不为旧调用点在旁边永久保留全局 DmaOp。宿主底层分配器不是第二套
  驱动契约，可以继续复用。
- 流式映射必须约束原 buffer 的存活、CPU/device 访问切换和方向同步。旧的安全
  函数接收裸 NonNull，却由 adapter 假定调用者保证内存有效，不应原样继承。
  新 unsafe mapping 契约也须在具体消费者验证，不能靠 trait 名字证明安全。
- bus address 为 0 可以有效；不能照搬旧 adapter 的 `bus_addr == 0` 跳过释放。
- 不默认搬入 `CoherentBox/CoherentArray`：当前扫描只见定义，不见树内使用；
  以后有 typed DMA 需求再按初始化、对齐、Drop 和设备访问约束设计。

**未决门槛**：旧设备模型每类消费者怎样获得真实 DMA 视图，缺失时如何处理；
vendor/static HAL 如何持有原 allocation owner。不能为方便接线默认为全地址
identity，也不能引入偷偷记账的全局 allocation 表。ixgbe HAL 当前明确为未启用
占位代码，不得声称已完成其真实硬件验证；同时仍须处理它的编译依赖。

这不是只有占位代码的问题：现有生产 `VirtIoHalImpl` 使用静态方法获取 DMA，
把地址交给 vendor，再在 dealloc/unshare 重建记录；没有传入设备实例，也没有
原 guard 返回通道。必须单独核对 vendor 现有承载机制，决定是否需要前置支持。
不能为凑 per-device 接口自造线程局部“当前设备”、泄漏 guard 或隐藏全局 owner 表；
若需要改 vendor 或迁移 transport，须先提出依赖及范围调整，不能在本 PR 偷跑。

证据：`drivers/base/os-api/src/dma.rs`、`drivers/integration/kdriver/src/host_dma.rs`、
`drivers/adapters/xkernel/device-res/src/dma.rs`、
`drivers/integration/kdriver/src/driver_registry/net/ixgbe_hal.rs`、
`drivers/integration/kdriver/src/driver_registry/virtio/glue.rs`。

### 3.3 CPU memory：保留已有正确设计，不强行改造成新 DMA guard

`block` 的 AHCI、BCM2835 SDHCI、word_io 使用 `CpuMemoryOp/CpuBuffer`。
该接口区分 CPU 与 DMA、可失败分配、初始化后暴露字节、原 provider 配对释放，
有实际消费者和失败测试。建议将此能力归入统一 os-api，保留必要语义与借用关系；
无需为了与 CoherentMem 外形一致而添加 Arc/release hook。

证据：`drivers/contracts/device-res/src/memory.rs`、
`drivers/contracts/block/src/word_io.rs`、`drivers/contracts/block/ut-tests/word_io_provider.rs`。

### 3.4 IRQ：区分实际功能与预留接口

普通 IRQ、共享 action、精确 token 释放、完整 callback drain、固件路由/virq
区分，以及 x86 PCI MSI-X 分配是真实迁移需求。新 IrqSpec/IrqResource 的显式
枚举方向应保留，不回退到 number/hwirq/domain 字段组合猜测身份。

**纠正前期清单**：旧 contract 有 threaded/default-primary 方法，但默认返回
Unsupported；`device-res-xkernel::XKernelResourceProvider` 没有实现覆盖。
当前扫描未见生产驱动调用，只有 helper/extension 与 mock UT。旧 adapter 还将
WAKE_THREAD 退化成普通 handled。不能据此宣称主线资源层已有可工作的 threaded
IRQ，也不应为了“能力不丢失”在 PR-01 新实现一个 threaded IRQ 接入项目。
KIRQ 自身是否支持其他线程化路径是另一层的问题，不能混同。

本 PR 保留现有普通 IRQ 路径；threaded 预留接口可迁入并保持 Unsupported，
实际支持与无用 helper 清理后续另行处理，不把 mock 支持当生产合同验收。普通 handler 的事件来源信息则
不能机械丢弃：VirtIO net 生成 RX/TX source 位，旧 adapter 向 KIRQ 传递它们。
新 IrqAction 两值接口不足以无损携带此信息。最终需要保留信息或证明消费者迁移
后不再需要它，不能仅以 Linux irqreturn_t 无 source 位为理由删除。
目前 net 注释将 source 位定位为未来多队列路由的 advisory 信息；RX 唤醒直接由
`schedule_rx()` 完成，不读取这些位。因此不能把“已有 producer/透传”再误报成
“当前 RX 调度依赖它”。功能需求与可观测信息是否保留，应分别作决定。

Linux `kernel/irq/manage.c::free_irq/request_threaded_irq` 将共享 action 身份、
设备侧停止中断、等待 handler 完成、线程化 primary/thread 分工分别规定；
不表示所有资源 provider 都必须立即实现线程化。

建议正常释放成功只意味着“本 action 已摘除并 drain”。None/不匹配 token 不可
当成功；不把释放整条 IRQ 线作为本 action 释放的兜底。非法上下文必须在有副作用
前拒绝；Drop 如何处理契约违例需与旧设备清理路径共同设计，不能泛化成静默继续。

### 3.5 MSI reservation 与 action 是不同生命周期

旧和新接口都有 `free_msix(resource) -> ()`；新接口同样不能因新就原样采纳。
现有 PCI transport 有分配/释放消费者，不能像没有使用的接口那样直接去掉。
reservation 不是 action token；必须明确设备停止、消息排序、action drain 与
reservation 释放顺序。主线的无分配释放不等于后端永不拒绝或设备已经停止。

**后续重设计门槛**：失败是否能返回仍有效 owner、PCI 调用方如何保留它；是否需要与
已暂缓的网络生命周期工作交叉。不得只将返回类型改成 Result 后让调用方仍提前
丢 owner，不得默默建全局隔离表。PR-01 只迁移现有语义，不以最终契约扩大范围，
也不在这个整理稿里默认批准网络 close 重构。

证据：`drivers/devices/virtio/src/pci/x86_64.rs`、
`drivers/adapters/xkernel/device-res/src/irq.rs`；另见既有 network-lifecycle-handoff。

### 3.6 Time、clock 与聚合 provider：迁移与清理分开

- TimeOp 当前只有旧聚合 trait 与 host 实现，未见树内直接驱动调用；本批可原样
  迁入，后续评估删除。host glue 自己调用单调时间不等于需要一个所有驱动必须依赖的接口。
- ClkOps 是设备时钟 prepare/enable/rate 控制，不是 TimeOp 替代品。当前底稿
  只有测试 provider，生产 host 返回 Unsupported；不作为 PR-01 必带能力。
- ResourceProvider 聚合 MMIO/DMA/IRQ/Time，当前未见消费其 trait bound 的代码；
  本批可保留，后续评估删除。一个 host 类型实现多个独立能力，不需要公共总接口。
- 可移植驱动以后确需时间/时钟时，应补相应最小能力契约，不能借本次省略允许它
  直接调用 khal/ktask。

## 4. 两种实现都必须接受的所有权检查

1. 能力 guard 自身不是硬件停机协议；调用者先 stop/drain，再释放资源。
2. 真正拥有资源的返回值不可 Clone；内部 token/释放参数不允许普通调用者伪造。
3. 资源 acquisition、release owner、devm cleanup 节点的失败必须一起考虑。
   新 `ReleaseHook = Arc<dyn Fn()>` 不天然满足“可失败、恰好释放一次”；
   必须说明谁独占调用权，是否需要一次性 owner，以及 owner 分配失败的回滚。
4. 当前 host_mmio 等 release-hook 元数据仍依赖 infallible Arc 策略。不得因
   底层映射返回 Result 就宣称全过程已经可失败；owner 应在发布前准备好，或能
   可靠回滚已获取资源。优先复用现有机制，不自行扩展通用分配框架。
5. CPU buffer 的 provider 借用、新 DMA 的持有能力和 MMIO 的 mapping reference
   可以各自采用适合生命周期的表示，不要求所有资源拥有同一种包装外形。

Linux `drivers/base/devres.c::__devm_add_action` 本身也可能返回 ENOMEM；可失败
清理登记不能在资源获得后被忽略。这里借鉴失败处理，不照搬 Linux 链表结构。

## 5. 修正后的执行顺序

1. PR-01 先迁移能力归属：Op/guard/基础词汇归 os-api，设备资源集合与清理
   归设备框架；已有 host 实现统一接线，维持主线现有行为。
2. 新接口暂不能等价接管的必要能力，先采用现有可用接口形式迁入；不并行复制
   一套新旧 provider，不强制转换 VirtIO static HAL，不引入新的默认 DMA 假设。
3. 全部消费者依赖 os-api，删除 device-res 与旧 package/兼容转发接线；在独立
   main 基线上验证。第一批不是只新增 os-api 等以后再迁消费者。
4. 后续在唯一能力层中逐项处理 DMA/HAL、IRQ/MSI owner、guard 失败语义以及
   无消费者接口清理。每项先收口设计，再同步迁移消费者并删除被替代接口。

验收不只要求 API 可编译：必须覆盖原有效功能、错误分类、申请失败回滚、释放
一次性、共享隔离、callback drain、CPU/DMA 区分和真实主线受影响路径。
无消费者接口退役的 mock 用例不机械照搬；覆盖实际语义的回归用例必须保留或迁移。

本稿尚未定稿最终 Rust 签名或分配失败实现细节，不授权直接实现这些重设计。
已批准的归属迁移按 PR-01 范围执行，不再等待第 3/4 节全部问题关闭。

文档核对：IRQ/MSI 与 DMA/CPU/MMIO 两路只读调研均无阻断性事实问题；已据此
收窄旧 DmaOp 的描述，区分 trait 表达能力与当前全局 provider 接线。此结论
不是具体接口设计验收；本轮没有修改生产代码或运行构建/UT。
