# 主线交付原则与 PR-01：统一 OS 能力层，退役 device-res

状态：用户已确认按职责逐步替换旧模型；本页是首个 PR 的范围与缺口清单，
不是已完成的实现、验收或已创建的远端 PR。transport 开发暂停，先整理 PR-01。

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
- 每批必须保持可构建、可启动、已有功能不退化。若范围过大，应重新商定更小的
  职责单位并删除其旧实现，不得默默退回“先引入新层，旧层以后再删”。

## 2. PR-01 的交付边界

建议标题：`refactor(drivers): unify OS capability contracts and retire device-res`。

目标是把主线现有 MMIO/IoPort 资源描述及 MMIO、IRQ、DMA、CPU 内存等能力统一到
`driver-os-api`，由一个 host adapter 实现；旧设备模型使用这套能力接口继续工作。
不要求引入新的 DeviceDomain、class、RNG 平台路由或 transport 驱动。

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
| device-res/mmio.rs：描述、MmioOp、Io | os-api 有 MmioOp/IoMap | 统一映射与释放合同；迁移旧 devm 调用，不能只搬出新接口 |
| ResourceDesc::IoPort / IoPortRange | os-api 有范围类型，但不是 MMIO 映射 | 保留 x86 I/O port 资源描述及现有访问路径，不把端口当内存映射或在迁移时丢弃 |
| device-res/dma.rs：DmaOp、DmaCoherent、typed helpers | os-api 同时有 DmaOp 和 per-device DeviceDma/guard | 先明确唯一的获取/释放接口及 per-device DMA 来源；不能复制两套 allocator/provider。保留现有地址、缓存、失败和释放语义 |
| device-res/memory.rs：CpuMemoryOp/CpuBuffer | os-api 缺失；block AHCI/SDHCI/word_io 已使用 | 纳入统一 CPU 存储能力，保留 fallible allocation、初始化和原 provider 配对释放；不得借 DMA 代替 CPU 存储 |
| device-res/time.rs：TimeOp | os-api 的 ClkOps 是设备时钟控制，不是单调时间；当前扫描只有聚合 trait 与 host 实现，未见具体驱动调用 | 两种职责不能互相替代；明确保留或删除无消费者接口的依据，不把它当成已使用功能，也不额外引入 clock provider 框架 |
| device-res/irq.rs：普通和 threaded IRQ、IrqEvent、token、MSI | 新 IrqAction 只有 Handled/NotHandled；NativeIrq 只覆盖普通 handler | 保留 threaded/default-primary/oneshot 与事件来源语义，不能把 WAKE_THREAD 压成 Handled；精确摘除 action 并等待完整 callback |
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

底稿 os-api 依赖 `arch/irq-types`，但审计基线 main 尚无该 crate；不能遗漏这个
提取依赖。复用已实现的中立 IrqTrigger/IrqController/IrqDomainId 定义，让 kirq
改为导出同一基础类型，同时删除原定义，而不是让 kirq 依赖驱动能力层。
这是类型归属调整，不包含新的 IRQ 调度/MSI 释放机制；提取前与 KIRQ 负责人
核对最新 main 的类型和进行中的修改，保持 kirq 现有公开路径与行为。

实现前需对最新 main 重跑依赖和符号扫描；上述是审计快照，不是免检白名单。
新框架底稿里的 model/RNG 消费者在后续提取时也必须使用本 PR 的唯一契约，
不得再把底稿中的第二套 os-api/host 实现带入主线。

## 5. PR 内的实现顺序与删除清单

以下是同一个 PR 的工作顺序，不是三个允许新旧长期共存的独立 PR：

1. 明确 IRQ、DMA、guard 的最终合同，补齐现有能力与测试；不引入 model 依赖。
2. 把现有 host 实现与全部旧消费者切到唯一合同；保留资源失败回滚、清理顺序、
   threaded IRQ、CPU buffer 与现有 PCI/MMIO 行为。按能力拆可审阅提交。
3. 删除旧 contract/provider package、Cargo 别名与旧测试入口；同步边界规则、
   模块文档、Kconfig/构建清单和消费者测试路径，完成整体验收。

必须退役：`drivers/contracts/device-res`、`device_res` workspace 依赖，以及
旧 `device-res-xkernel` 包/接线。adapter 可原位重组或移动，最终目录与包名应
在实现设计时确定；不能只改名继续包装旧 contract。

不默认保留 `device_res` 兼容 crate、`legacy` provider 模块或转换链。
不默认搬入没有本批消费者的 clock/provider 注册框架。
可保留有实际使用价值的 guard/helper，但必须共享唯一能力合同，而不是旧模型副本。

## 6. 验收门槛

- workspace、Cargo.lock、配置与源文件不再依赖旧 package；历史文档提到旧名
  不等于活跃依赖，检查应区分二者。
- 原 device-res/provider UT 迁入正确的新目录，保留覆盖，不靠删用例消除失败。
- MMIO 与 DMA：成功/失败配对、清理一次性、probe 回滚、CPU/DMA 存储区分；
  实际改到 per-device DMA 时验证 mask/window/streaming 语义，不借机放宽约束。
- IRQ：普通、共享、threaded/default-primary、token 精确释放、完整回调 drain、
  非法上下文与 MSI 失败语义；涉及同步机制时保留针对性负控，不能只跑 RNG。
- 框架：现有 kdevice probe 失败 LIFO 清理、解绑清理、资源仍被使用时的约束不变。
- 平台：按仓库流程准备配置，AArch64/x86 构建和 clippy；现有 PCI 与 MMIO
  启动/块设备/网络及 x86 I/O port 等受影响路径回归。测试无硬件覆盖的控制器
  需明确限制。
- 独立规范/需求评审通过；PR 不夹带设备匹配/拓扑/class/transport 模型切换。

## 7. 当前落点与恢复条件

已完成的是原则记录、基线核对、消费者盘点和 PR-01 范围，不是代码提取。
当前 os-api 无法直接作为全量替代品：threaded IRQ、CPU/time 和 DMA 获取合同
必须先收口。下一动作是对此范围形成可实施的接口差异设计，然后从最新 main
提取独立 PR 分支；未经验证不称为可合入，不自动推送或创建远端 PR。

先把 PR-01 整理至可评审状态，再恢复 transport 开发。后续设备注册/绑定、
class、transport 的 PR 顺序仍需按依赖核对，不能把本页误读为全部顺序已定。

## 8. 最新 main 对齐审计（2026-11-04）

开发底稿已从 `eec7d7722` rebase 到 `origin/main=f429a5953`，rebase 后为
`a5aabed3d`。main 是分支祖先；64 个提交的 range-diff 只有两处冲突解决变化：
Cargo.lock 同时保留 kirq 的新 kalloc-types 依赖和共享 irq-types 依赖，kirq
设计文档同时保留主线 fallible reverse-map owner 与分支的软件派发测试说明。
未修改其他 worktree，也没有推送。

逐项保留检查：

- 旧 device-res、device-res-xkernel、block、kdevice 与此 main 基线内容一致，
  包括 CPU buffer、threaded IRQ 以及主线 adapter 新增的错误分类。
- 主线 KIRQ 的 fallible reverse-map owner 准备、MSI 无分配删除与失败恢复机制
  保留。基础类型抽取仅替换类型定义来源，不删除新增 NoMemory/InvalidHwirq。
- 新 NativeIrq 原有 exhaustive match 缺少这两类错误；已补 NoMemory → NoMemory、
  InvalidHwirq → InvalidResource，并增加 map/request 两条入口的错误传播用例。
- 主线的 kbox owner 发布前准备、kmutex FIFO 等更新通过 rebase 继承，不以
  开发分支旧代码覆盖。现有 MSI 后端拒绝与消费者 owner 丢失问题仍按原交接处理，
  不把核心无分配释放误称为已解决全部驱动 teardown 问题。

这些检查证明最新 main 的能力没有被 rebase 覆盖，不代表 os-api 全量迁移已完成。
第 3 节的 threaded IRQ、CPU 内存、TimeOp 去留和 DMA 合同仍是 PR-01 的明确工作，
不得在缺失这些决策/实现时删除旧 crate。验证日志放在
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
