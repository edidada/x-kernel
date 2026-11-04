# 主线交付原则与 PR-01：统一 OS 能力层，退役 device-res

## device-res 退役批次（已验证，待抽取 PR）

ResourceDesc/Set 归回 kdevice::device::resource；devm 助手归回
kdevice::device::managed，直接使用 DeviceObject::add_cleanup。
删除仅由这层实现的 DeviceResource trait 及无消费者的 ResourceProvider 聚合。
os-api 不接管设备描述、身份或清理登记。旧 crate 和依赖删除。
原 UT 迁到 kdevice 并用真实设备对象执行清理；删除仅验证旧 re-export 的过渡用例。
以下“仅暂时保留”的记录为前一批历史状态，当前不再保留该 crate。
本批不改变 MSI 失败处理、网络关闭或 native DMA 约束。

验证：AArch64 SMP4 282 通过、0 失败、5 忽略，覆盖导出完成；选择
driver_os_api/kdevice/driver_os_api_xkernel/driver_model/kdriver/virtio。
kdevice 的 6 个资源用例使用真实设备对象，覆盖 LIFO、重复 drain、失败不登记和
threaded guard 成功释放。Native IRQ/RNG callback 尾部、shared peer 与负控仍通过。
AArch64、x86_64 UNITTEST clippy 通过，已恢复 AArch64 配置。
独立只读审查未发现阻塞；源码/manifest/lockfile 已无 device-res 依赖。
日志：`.scratch/pr01-device-res-retirement/`。下一步审计 PR1 抽取范围与最新主线
能力差异，单独抽取后重跑验收；当前不代表远端 PR 已创建或主线迁移已完成。

## IRQ source 位图取舍修正

此前“网络消费者需要 source 位图”的结论撤回：生产 RX 调度在 handler 内直接
完成，位图没有行为消费者，仅被转存/合并及测试读取。os-api 删除 source 字段、
类型和访问/构造 API；virtio-net 保留原有 IRQ 过滤、ack、RX 调度及 handled 结果。
这与本地 Linux irqreturn.h / virtio_ring.c 的职责划分一致：设备/队列事件在
驱动或 transport 内处理，不通过通用 IRQ 返回值上交。以下旧验证记录中的位图
保留只描述当时迁移状态，不再是目标要求。KIRQ 自身的冗余位图本批不修改。

## 当前 IRQ 迁移收口

IrqResource/MsiResource/IrqOp 和 DevIrq 均已统一到 os-api；旧 irq.rs 删除。
device-res 仅暂时重导出类型并保留 ResourceDesc/Set、DeviceResource、ResourceProvider
聚合及 devm 清理辅助函数。VirtIO 和 xkernel provider 已移除 device-res 依赖。
下一步处理这些框架集成归属，随后再抽取 PR1；本批仍不宣称整个 PR1 完成。

DevIrq 持原 provider/resource/token（静态借用或 Arc 保活），或原有外部 hook。
模型宿主不再另造释放闭包，RNG 的 Legacy/Model guard 分支删除。未使用的旧
guard set_enabled 便利方法删除；整线开关仍由 IrqOp 提供，不作为 action 所有权。
threaded 注册契约统一且默认 Unsupported，未新增宿主实现。MSI 释放失败 owner
问题和网络 close 仍是既有独立工作，不在本批静默改变。

验证：位图修正独立提交 fb366650f，通过 266/0/5（通过/失败/忽略）的驱动 UT。
资源/Op/guard 收敛通过 AArch64 SMP4 269/0/5，覆盖导出完成；包括 provider 保活、
原始 domain/hwirq/virq/token 精确释放、失败无 guard、threaded 默认 Unsupported
及无回调残留、RNG notify 尾部 drain 和 shared peer/负控用例。
AArch64/x86_64 UNITTEST clippy 通过，恢复 AArch64 配置。独立审查发现并修复
x86 INTx 路径残留字段访问后复审无阻塞。日志分别位于本地
`.scratch/pr01-irq-source-removal/` 和 `.scratch/pr01-irq-ownership/`。

状态：用户已确认按职责逐步替换旧模型；本页是首个 PR 的范围与缺口清单，
不是已完成的实现、验收或已创建的远端 PR。transport 开发暂停，先整理 PR-01。

后续需求核对与取舍见 [能力契约整理](pr01-capability-contract-decisions.md)。
该稿纠正了初期把 threaded API 声明误当已实现资源能力的问题；以下清单按此修正。
旧 API 的存在不自动成为新接口需求，新旧设计均须按实际功能与生命周期评估。

**最新批准范围：先统一归属，再在唯一 crate 内演进。** PR-01 不再要求先完成
per-device DMA、VirtIO HAL 或 MSI owner 的最终设计。已有重复能力收敛到 os-api；
尚无等价新实现的必要能力先保留当前主线语义，一并迁入，随后单独改进。
删除旧 crate 与全部接口最终定型是两件事，不再相互绑定。

审计基线：开发底稿 `5bbdc8bc2`；2026-11-04 fetch 后的 `origin/main` 为
`f429a5953`。两者不同：实现必须从届时最新 main 单独提取，不能直接 cherry-pick
整条开发分支，更不能覆盖 main 已前进的 IRQ 错误处理等改动。

## 1. 已确认的交付原则

- 主线按职责替换，而不是先合入平行的新框架，再逐个迁移消费者。
- 每个 PR 接管一项完整职责，迁移该职责全部树内消费者，并删除被替代的实现、
  接口、workspace 依赖和配置入口；不能靠改目录或兼容 re-export 宣称完成替换。
- 允许“新能力接口 + 尚未替换的设备注册模型”组合。禁止同一职责因新旧消费者
  而维护两套实现；设备模型本身的替换是后续 PR 的独立范围。
- 用户以 os-api 替换 device-res 为例：若本 PR 宣称完整替换 device-res，合入后
  就不得仍有 device-res crate 或旧 provider 与新 provider 并行工作。
- 当前大分支只是实现与测试的开发底稿，不代表面向 main 的提交顺序。
- 可保留现有能力接口形式作为 os-api 的初始实现，但不能在另一 crate 或
  `legacy` 子模块再保留同职责的完整副本；后续演进同步迁移消费者、删除旧接口。
- 每批必须保持可构建、可启动、已有功能不退化。若范围过大，应重新商定更小的
  职责单位并删除其旧实现，不得默默退回“先引入新层，旧层以后再删”。

## 2. PR-01 的交付边界

建议标题：`refactor(drivers): unify OS capability contracts and retire device-res`。

目标是把主线现有 MMIO/IoPort 资源描述及 MMIO、IRQ、DMA、CPU 内存等能力统一到
`driver-os-api`，由一个 host adapter 实现；旧设备模型使用这套能力接口继续工作。
不要求引入新的 DeviceDomain、class、RNG 平台路由或 transport 驱动。

提取以 main 的现有工作路径为基线，复用底稿中能够等价替换的部分。不能为了
复用底稿而一并引入其 DmaOp/DeviceDma、普通 IRQ 等平行接线。首个主线 PR
不搬入整套新模型；底稿上的新模型消费者在后续提取时再适配统一契约。

```text
现有 kdevice / kdriver / block / virtio
                  │
                  ▼
             driver-os-api
                  ▲
                  │ 实现
         单一 X-Kernel 能力 adapter ──► kirq / kdma / memspace / 时间与分配器
```

“设备描述中的资源集合”和“绑定失败/解绑时的清理栈”不是 OS 实现。
资源描述集合归设备模型；本批保留主线 kdevice 的生命周期及 LIFO cleanup，
通过能力 guard 接入，不为了删除旧 crate 把设备注册/绑定逻辑塞进 os-api。
旧 devm helper 的便利入口可由 kdriver 组合这两层，但不再拥有第二套资源实现。

## 3. 职责对照与已核实的缺口

| 旧职责 / 证据 | 开发底稿状态 | PR-01 要求 |
|---|---|---|
| 原 device-res/mmio.rs：描述、MmioOp、Io | 已删除重复模块；统一使用 os-api MmioOp/IoMap | 旧 devm 与新模型均保留同一 guard，宿主映射委托单一 provider |
| ResourceDesc::IoPort / IoPortRange | os-api 有范围类型，但不是 MMIO 映射 | 保留 x86 I/O port 资源描述及现有访问路径，不把端口当内存映射或在迁移时丢弃 |
| 原 device-res/dma.rs：DmaOp、DmaCoherent、typed helpers | 重复模块已删除；基础契约与 coherent guard 统一到 os-api | 保留 static HAL 原获取/释放协议；DeviceDma 的既有窗口/缓存策略不降级，不宣称两种 native backend 已合并 |
| 原 device-res/memory.rs：CpuMemoryOp/CpuBuffer | 首批已迁入 os-api；block AHCI/SDHCI/word_io 与 host 实现直接依赖新归属 | 保留 fallible allocation、初始化和原 provider 配对释放；不得借 DMA 代替 CPU 存储 |
| device-res/time.rs：TimeOp | os-api 的 ClkOps 是设备时钟控制，不是单调时间；当前扫描只有聚合 trait 与 host 实现，未见具体驱动调用 | 首批允许原样迁入以减少语义变化，不新增 clock provider；无消费者接口的删除可作为后续独立清理 |
| device-res/irq.rs：普通 IRQ、IrqEvent、token、MSI；threaded 仅接口预留 | 新 IrqAction 只有 Handled/NotHandled；旧生产 adapter 也没有覆盖 threaded 默认 Unsupported | 首批保留普通 IRQ/事件来源/MSI 的现有语义；threaded 预留可随接口迁入但仍为 Unsupported，不新增线程化实现或借迁移改变释放协议 |
| device-res/lib.rs：ResourceDesc/ResourceSet | 新 model 另有自己的描述模型 | 本批让旧 kdevice 自持其描述集合，元素能力类型来自 os-api；不为此引入新 domain |
| DeviceResource/register_cleanup + provider.rs 的 devm helper | kdevice::DeviceObject 实现 cleanup sink | 保留已用生命周期，不把 cleanup sink 变成 OS provider；helper 归框架集成，guard 归能力层 |
| device-res-xkernel：MMIO/DMA/IRQ/CPU/time 实现 | 新 NativeIrq/host_dma/host_mmio 在 kdriver 另有实现 | 主线 PR 只保留单一 adapter；可提取已验证机制，不能合入两套宿主接线 |

主线 adapter 已新增 `IrqDescError::NoMemory`、`InvalidHwirq` 的映射；初次审计
底稿尚未包含，现已随 rebase 保留并补齐 NativeIrq 接线，见第 8 节。提取时必须
保留这两类错误语义。KIRQ/MSI 的并行开发由 IRQ 负责人负责，本 PR 不
改其 worktree、不另造 IRQ core 释放机制，也不能回退已合入的核心合同。

## 4. 必须同批迁移的消费者

按当前 Cargo 依赖核对，旧 contract 的直接消费者是：

- `drivers/contracts/kdevice`：设备资源集合、DeviceResource 实现与 cleanup。
- `drivers/contracts/block`：AHCI、BCM2835 SDHCI、word_io 及 CPU buffer 测试。
- `drivers/devices/virtio`：block requests、input、net、PCI glue 与相关资源调用。
- `drivers/integration/kdriver`：resource provider、block IRQ、VirtIO block/input/glue、
  ixgbe HAL 及对应 UT。
- `drivers/adapters/xkernel/device-res`：全部 provider 实现及测试。

这不是只迁移 RNG。网络在本 PR 内只做能力接口接入与等价性验证，不顺手开展
已单独记录的网络 close/reset/MSI owner 生命周期项目；发现接口迁移必须依赖
该项目时应显式阻塞/重新拆分，不把已知问题宣称为已解决。

### IRQ 基础类型前置

底稿 os-api 依赖 `arch/irq-types`，但审计基线 main 尚无该 crate。首批若复用
该部分，必须同时提取其依赖；若保留主线现有 IRQ 词汇，则不为接口外形统一
强制修改 KIRQ。基础类型统一可独立进行：复用中立定义，让 kirq 导出同一类型
并删除原定义，而不是让 kirq 依赖驱动能力层。
这是类型归属调整，不包含新的 IRQ 调度/MSI 释放机制；提取前与 KIRQ 负责人
核对最新 main 的类型和进行中的修改，保持 kirq 现有公开路径与行为。

实现前需对最新 main 重跑依赖和符号扫描；上述是审计快照，不是免检白名单。
新框架底稿里的 model/RNG 消费者在后续提取时也必须使用本 PR 的唯一契约，
不得再把底稿中的第二套 os-api/host 实现带入主线。

## 5. PR 内的实现顺序与删除清单

### Provider 随契约一起收口（最新确认）

用户同意：不仅迁移 os-api，还要把两边有效的宿主能力实现收敛到新框架使用的
provider。不能只搬类型而让旧 adapter 与 kdriver/host_* 长期各自实现同一能力。
先在当前分支完成接线与回归，再抽取主线 PR；不是先抽取再适配新模型。

目标位置确定为 `drivers/adapters/xkernel/provider`，包名 `driver-os-api-xkernel`，
Rust 名 `driver_os_api_xkernel`。它实现 os-api，不依赖 driver-model、kdevice、
kdriver 或具体设备驱动。统一的是宿主实现归属，不要求所有能力塞进一个总 trait
或一个总 provider 对象；可以保留职责明确的各家族实现。

按现有层次组织，不新增笼统的 host 大 crate：`adapters/xkernel` 已是宿主
适配层，provider 是其下独立 crate；框架服务组合放在
`drivers/integration/kdriver/src/host/`，保留为 kdriver 的内部模块组。
`host/mod.rs` 仅导出服务构造入口，`services.rs` 组合能力，`management.rs`
实现设备管理锁。设备身份/描述/清理仍归模型，不建立对等 services crate。
当前先落地服务模块组与调用路径；旧 adapter 和 host_irq/host_dma 的宿主
实现归并尚未完成。此次目录调整不代表 provider 已完成迁移。

服务模块组验证：AArch64 SMP4、无网络/vsock 的 model+kdriver UT
165 passed、0 failed、5 ignored，覆盖率导出成功；UNITTEST clippy 与只读
复核通过。UT 仍放在 ut-tests，挂载路径及调用入口同步更新；管理测试的
全限定名称随模块路径变化。日志：`.scratch/pr01-host-layout/`。

```text
设备模型 / kdriver 服务组合 ──► driver-os-api-xkernel ──► kirq/kdma/memspace/kalloc
          │                            │
          └──────────────► driver-os-api ◄┘
```

| 当前部分 | 目标职责与迁移要求 |
|---|---|
| 旧 adapter 的 CPU/MMIO/raw DMA/time | 纳入统一宿主 adapter，保留当前有效协议；TimeOp 尚需完成契约迁移 |
| kdriver/host_irq::NativeIrq 与旧 adapter/irq | 统一普通 IRQ 接入，保留精确 token/drain、旧事件来源与 MSI 能力；不是搬进两个模块就算完成 |
| kdriver/host_dma 的 native 分配、bounce、地址翻译、释放 | 归宿主 adapter；内部可以区分 coherent/streaming 生命周期，但底层 native 获取/回收只维护一份 |
| HostDeviceDma 保留 DeviceRef、读取 descriptor.dma | 留在框架接线，按操作借用权威 DMA 描述；不能为搬 crate 缓存第二份设备配置或让 adapter 依赖 model |
| host_services 的 DeviceServices/extension/management lock 组合 | 留在 kdriver；这是设备框架服务，不是 OS 能力 provider |
| host_mmio 薄接线 | 可内联到服务组合，或仅做 RuntimeError 转换；不得恢复 memspace 映射实现 |
| devm helpers、DeviceResource、资源集合与 cleanup | 随设备框架职责迁移，不放进宿主 provider |

DMA 收口必须区分两个地址：设备可见地址与 native 回收凭证。raw static HAL
保持既有协议，设备级路径仍验证整个区间/window/mask、初始化接收 bounce、
拒绝时 discard 而不破坏原 buffer。现有 raw DmaMapping 没有 bounce CPU 地址
及 discard 操作，不能用其 map/unmap 强行替换设备级实现。应把所需 native
owner/操作放在 adapter 内部，使两种入口复用底层机制，不为了内部复用扩大
os-api 的公共合同。若需要修改 kdma 或 vendor，先单列前置，不隐含批准。

IRQ 同样不能直接把 NativeIrq 当作旧实现的完整替代：它目前只实现普通 IRQ，
旧消费者仍用事件来源信息、MSI 分配/释放及 threaded 预留。先统一公共合同，
再迁移两个模型的消费者到同一宿主注册/释放路径。threaded Unsupported 继续
保持，不新增线程化项目；MSI owner/网络 close 重设计仍延期。

接下来按以下增量执行，每步都须构建和定向回归：

1. 收口 IRQ/time 契约与消费者，同时合并对应宿主实现，不再只搬类型。
2. 把设备级 DMA native 后端迁入统一 adapter，与 raw 入口复用内部所有者；
   保留原始设备描述读取位置，删除 kdriver 内的重复 native 操作。
3. 删除旧 adapter package，更新 Cargo、测试入口、边界规则和模块文档。
   重命名是上述职责归并的结果，不单独用重命名宣称迁移完成。
4. 验证 adapter 对设备模型无反向依赖，旧 package/重复能力实现无残留；
   重跑 CPU/MMIO/DMA、IRQ shared/drain、RNG 完整生命周期与现有 HAL 回归。

本节为实施边界，尚未完成 provider 迁移。先前 CPU/MMIO/DMA 批次的验收只覆盖
当时已实现的改动，不替代本次宿主收口验收。

以下是同一个 PR 的工作顺序，不是三个允许新旧长期共存的独立 PR：

1. 以主线已用合同为起点确定迁移映射：能力类型/Op/guard 归 os-api，描述集合
   与 cleanup 归设备框架，host 行为归单一 adapter；不引入 model 依赖。
2. 把现有 host 实现与全部旧消费者切到唯一合同；保留资源失败回滚、清理顺序、
   普通 IRQ、CPU buffer 与现有 PCI/MMIO 行为。首批不强制删除无消费者的预留
   接口，也不为接口数量对齐新增功能。按职责拆可审阅提交。
3. 删除旧 contract/provider package、Cargo 别名与旧测试入口；同步边界规则、
   模块文档、Kconfig/构建清单和消费者测试路径，完成整体验收。

必须退役：`drivers/contracts/device-res`、`device_res` workspace 依赖，以及
旧 `device-res-xkernel` 包/接线。adapter 可原位重组或移动，最终目录与包名应
在实现设计时确定；不能只改名继续包装旧 contract。

不默认保留 `device_res` 兼容 crate、`legacy` provider 模块或转换链。
不默认搬入没有本批消费者的 clock/provider 注册框架。
可保留现有 guard/helper，但必须共享唯一能力合同，而不是旧模型副本。
不把所有功能重设计后再删除旧 crate；先消除独立旧能力层，随后在 os-api 内
按需求逐项替换接口和实现。

## 6. 验收门槛

- workspace、Cargo.lock、配置与源文件不再依赖旧 package；历史文档提到旧名
  不等于活跃依赖，检查应区分二者。
- 原 device-res/provider 的有效语义 UT 迁入正确的新目录；无消费者接口退役的
  mock 用例记录理由，不机械搬迁，也不靠删回归用例消除真实失败。
- MMIO 与 DMA：成功/失败配对、清理一次性、probe 回滚、CPU/DMA 存储区分；
  本批不改变 DMA 视图与 HAL 协议。后续改 per-device DMA 时再验证新的
  mask/window/streaming 语义，不把未来验收作为本批必须实现的功能。
- IRQ：普通、共享、事件来源、token 精确释放、完整回调 drain、
  非法上下文与现有 MSI 失败语义；保留现有同步回归，不能只跑 RNG。
  MSI owner 重设计和网络 close 不纳入本批，也不得宣称旧问题已经修复。
- 框架：现有 kdevice probe 失败 LIFO 清理、解绑清理、资源仍被使用时的约束不变。
- 平台：按仓库流程准备配置，AArch64/x86 构建和 clippy；现有 PCI 与 MMIO
  启动/块设备/网络及 x86 I/O port 等受影响路径回归。测试无硬件覆盖的控制器
  需明确限制。
- 独立规范/需求评审通过；PR 不夹带设备匹配/拓扑/class/transport 模型切换。

## 7. 当前落点与恢复条件

已完成原则记录、基线核对、消费者盘点，并开始当前分支上的能力迁移，尚未抽取 PR。
按用户最新确认，先在当前新框架分支实施“归属统一、现有语义不变”的迁移，
把缺失但需要保留的能力一并迁入 os-api，移走设备模型职责，再迁移全部消费者并
删除旧 crate。验证新框架与现有消费者后，再从最新 main 抽取独立 PR 并重新验收。
无需等 DMA/HAL/MSI 最终模型完成。未经验证不称为可合入，
不自动推送或创建远端 PR。

先把 PR-01 整理至可评审状态，再恢复 transport 开发。后续设备注册/绑定、
class、transport 的 PR 顺序仍需按依赖核对，不能把本页误读为全部顺序已定。

### 首批实施：CPU 内存能力

- CpuMemoryOp/CpuBuffer 及两项契约 UT 从 device-res 移至 os-api，无重复定义、
  无旧路径转导出；块驱动生产与测试消费者全部直接引用 os-api。
- 保留现有唯一宿主实现和分配/初始化/释放协议；宿主 package 整体迁移尚未进行。
- AArch64 UNITTEST 构建和 clippy 通过；SMP4、无网络/vsock 的定向 QEMU UT
  （driver_os_api/device_res_xkernel/block）61 passed、0 failed、0 ignored，
  覆盖率导出成功。只读复核无阻断，差异检查通过。
- 日志：`.scratch/pr01-cpu-memory/`。本批不代表 IRQ/DMA/MMIO 已统一，
  不代表新框架整体验收或 PR-01 完成；x86 验证仍待后续执行。

### 第二批实施：MMIO 契约与宿主映射

- 删除 device-res 的 MMIO 定义和 Io guard；描述集合、devm helper、VirtIO glue
  直接使用 os-api 的唯一描述/Op/IoMap。ResError 统一定义在 os-api，旧 crate
  暂转导出供剩余 DMA/IRQ 迁移使用，不保留第二套错误类型。
- IoMap::map_with 直接保留原始 provider/token，释放不分配闭包；新模型
  host_mmio 委托同一宿主实现，不再单独调用 memspace 实现映射和释放。
- MmioOp 明确 unsafe 实现合同，unmap 要求原始未释放 token，避免复制元数据
  被安全调用当作额外 owner。保留新 guard 的边界/对齐检查，未宣称 Linux
  MMIO ordering 等价，也未新增物理区间 claim。
- AArch64 SMP4、无网络/vsock 的最终定向 UT：189 passed、0 failed、5 ignored；
  覆盖 os-api/device-res/host adapter/model/kdriver。clippy 与差异检查通过，
  只读复核无阻断。日志位于 `.scratch/pr01-mmio/`。
- CPU/MMIO 完成不等于 PR-01 完成；后续继续 DMA、IRQ、设备资源/cleanup
  职责迁移、旧 package 删除及双架构整体验收，再从 main 抽取 PR。

### 第三批实施：DMA 基础契约与 coherent guard

- 删除 device-res/dma.rs，所有基础类型与 DmaOp 只定义于 os-api；宿主 raw
  provider、static VirtIO/ixgbe HAL、devm 与资源描述消费者直接使用统一类型。
- 删除 DmaCoherent，统一为 CoherentMem。私有 owner 枚举选择 provider/完整
  allocation token 或原有 hook 路径，避免同时复制 cpu/bus/len。typed helpers
  迁入 dma/typed.rs，保持现有接口语义并共用同一 guard。
- 不改设备级 DeviceDma 策略：窗口/mask、接收 bounce 初始化、拒绝时 discard
  仍由既有实现负责。raw DmaMapping 不具备这些 native 元数据，不能机械替换。
  本批不宣称 native backend 统一，也不宣称解决 raw DMA 历史安全签名问题。
- AArch64 SMP4、无网络/vsock 定向 UT：260 passed、0 failed、5 ignored，
  覆盖 os-api/device-res/adapter/model/kdriver/virtio，含新增原始释放 spec、
  coherent 零 bus 地址、失败无释放及 typed guard 测试；覆盖率导出成功。
- AArch64 与 x86 UNITTEST clippy 均通过，AArch64 canonical 配置已恢复；
  x86 为编译/lint 检查，不冒称硬件或 QEMU 功能验收。只读复核与差异检查通过。
  日志位于 `.scratch/pr01-dma/`。下一批处理 IRQ 契约和消费者。

## 8. 最新 main 对齐审计（2026-11-04）

开发底稿已从 `eec7d7722` rebase 到 `origin/main=f429a5953`，rebase 后为
`a5aabed3d`。main 是分支祖先；64 个提交的 range-diff 只有两处冲突解决变化：
Cargo.lock 同时保留 kirq 的新 kalloc-types 依赖和共享 irq-types 依赖，kirq
设计文档同时保留主线 fallible reverse-map owner 与分支的软件派发测试说明。
未修改其他 worktree，也没有推送。

逐项保留检查：

- 旧 device-res、device-res-xkernel、block、kdevice 与此 main 基线内容一致，
  包括 CPU buffer、threaded IRQ 接口预留及主线 adapter 新增的错误分类；
  保留声明不代表生产 provider 已实现 threaded 功能。
- 主线 KIRQ 的 fallible reverse-map owner 准备、MSI 无分配删除与失败恢复机制
  保留。基础类型抽取仅替换类型定义来源，不删除新增 NoMemory/InvalidHwirq。
- 新 NativeIrq 原有 exhaustive match 缺少这两类错误；已补 NoMemory → NoMemory、
  InvalidHwirq → InvalidResource，并增加 map/request 两条入口的错误传播用例。
- 主线的 kbox owner 发布前准备、kmutex FIFO 等更新通过 rebase 继承，不以
  开发分支旧代码覆盖。现有 MSI 后端拒绝与消费者 owner 丢失问题仍按原交接处理，
  不把核心无分配释放误称为已解决全部驱动 teardown 问题。

这些检查证明最新 main 的能力没有被 rebase 覆盖，不代表 os-api 全量迁移已完成。
此处记录的是当时的 rebase 对齐，不是全量迁移完成。按最新批准范围，PR-01
可以通过迁入现有有效语义补足接口，不再等待 DMA/MSI 的最终重设计。验证日志放在
`.scratch/driver-pr01-rebase/`。

AArch64 canonical defconfig 经 `make defconfig` 后，独立 worktree/disk、SMP4、
无网络/vsock，非性能测量；两组 QEMU UT 均成功导出 coverage：

- `UNITTEST_CRATE=driver_os_api,device_res,device_res_xkernel,driver_model,kdriver`：
  188 passed / 0 failed / 5 ignored，含新的 map/request 错误传播用例。
  五项忽略为 opt-in MMIO 专项，不计为通过。
- `UNITTEST_CRATE=kirq,block`：266 passed / 0 failed / 0 ignored，包含主线
  MSI owner 准备失败/槽恢复、IRQ admission 及 block CPU buffer 失败路径。

adapter 新增测试是注入错误的传播验证，不冒充真实 allocator OOM；核心 MSI
用例也不替代真实设备停止或已发 MSI 的硬件排序证明。两路只读评审无阻断。

AArch64 与 x86_64 各自 canonical defconfig 下的 `make clippy UNITTEST=y`
均通过；这是双架构编译/lint 检查，不是 x86 QEMU 启动验收。已恢复 AArch64
canonical 配置。修改文件 rustfmt 和 diff 检查通过，旧资源层与 main 的内容
一致性及 range-diff 核对均通过。
## Provider consolidation progress (2026-11-04)

The host implementation now lives in `drivers/adapters/xkernel/provider`
(`driver-os-api-xkernel`). NativeIrq and its lifecycle/SMP tests moved from
kdriver into that crate. Legacy IRQ entry points reuse the shared registration,
exact-action release and MSI backend, preserving source bitmaps. TimeOp moved
unchanged into os-api. Framework identity, cleanup registration and service
composition remain outside the provider.

This is not PR-01 completion: legacy IRQ vocabulary/routing, the aggregate
provider and device-res consumers still need migration. Native per-device DMA
must preserve mask/window/bounce/discard semantics when its backend is moved.
MSI consuming-release failure ownership remains a separately tracked limit.
After validation in this branch, extract the replacement PR onto main and
revalidate; do not submit a permanently parallel capability layer.

Validation: AArch64 SMP4 selected driver suites passed 261 tests, with zero
failures and five ignored tests; coverage was exported. This includes moved
NativeIrq shared-peer/tail-drain and concrete RNG notify-tail fixtures.
UNITTEST-enabled clippy passed on AArch64 and x86_64; AArch64 configuration
was restored. Independent read-only review found no code blocker. Logs are
local under `.scratch/pr01-irq-provider/` (not committed).

### Shared IRQ vocabulary and domain preservation

Trigger/controller/domain values now share irq-types via os-api; route
descriptions, handler tokens and MSI messages are defined only in os-api.
Device-res temporarily re-exports these definitions while its consumers migrate.
Legacy resource shapes, callback protocol and guards remain outstanding.

A separately committed correction makes the legacy adapter preserve explicit
domains through mapping and later request/enable/release, instead of silently
substituting a controller root. Only absent domains use the historical fallback.
The explicit-domain regression test failed before the fix; no global KIRQ
mapping policy or domain ownership was changed.

Validation: AArch64 SMP4 selected driver suites passed 265 tests, zero failures,
five ignored; coverage exported. UNITTEST clippy passed on AArch64 and x86_64,
and canonical AArch64 configuration was restored. Independent review found no
blockers. Local red/green and lint logs: `.scratch/pr01-irq-vocabulary/`.

### Unified IRQ callbacks

IrqEvent, IrqHandler and IrqThreadHandler now have one definition in os-api's
`irq/handler.rs`. Native and legacy providers both accept `Arc<dyn IrqHandler>`
and share their host callback adapter. The separate IrqAction and no-argument
IrqHandlerFn contract were removed, not retained as compatibility alternatives.
Callbacks receive the dispatched virq and preserve the source bitmap required
by network consumers. Model resources and the RNG path use this same contract.

Threaded intent/traits are preserved from the existing contract, not implemented
as a new feature: ordinary registration has no paired thread and the xkernel
adapter only forwards handled/source results. Legacy threaded registration
still defaults to Unsupported. Guard ownership, stop/drain sequencing and MSI
reservation release are unchanged. Resource shapes, guard acquisition and the
remaining device-res consumers are the next replacement slice.

Validation: AArch64 SMP4 selected suites passed 266 tests, zero failures and
five ignored; coverage exported. Tests include model virq/source forwarding,
event merging, callback tail drain/shared peers and concrete RNG notify-tail
with the existing negative control. AArch64 and x86_64 UNITTEST clippy passed;
AArch64 configuration restored. Independent read-only review found no blocker.
Local evidence: `.scratch/pr01-irq-callback/`.
