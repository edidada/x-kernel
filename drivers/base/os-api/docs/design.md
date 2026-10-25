# driver-os-api — 设计

## Held per-device DMA

`DeviceDma` is an unsafe, Send + Sync provider contract for one stable device
DMA view. Unlike the legacy `DmaOp` records, it returns owned `CoherentMem` and
non-Clone `StreamingMapping` guards. Streaming construction requires a release
hook; Drop invokes it once with cleanup dependencies retained by the hook.
The bus address may be zero. The original streaming buffer remains caller-owned.

Providers enforce window/mask/cache policy and native allocation pairing.
Streaming failure leaves the buffer unchanged and no live mapping; successful
mapping ends with directional synchronization at Drop. This API adds neither
a host allocator nor a global provider registry. Tests cover guard movement,
release-owner retention and exactly-once cleanup without hardware access.

## 职责与依赖

本 crate 表达驱动框架需要 OS 提供什么能力，以及使用者如何持有和释放能力。
它不实现内核行为，也不管理设备绑定。没有静态 provider 或服务注册表。

```text
driver-model → driver-os-api → irq-types ← kirq
host adapter → driver-os-api + kernel subsystems
```

宿主适配方实现 Op，并在实现 DeviceResourceServices 时返回能力句柄。
新模型的生产宿主服务尚待接入；不能把旧 device-res-xkernel 的实现当作新 Op 实现。
旧 device_res 保持独立，未改动其类型、调用方或生命周期。

## 内部模块

模块私有，crate 根导出能力对象；不保留一个与能力模块分离的 host_api 大文件。

| 模块 | 共同拥有的职责 |
|---|---|
| mmio | MmioRegion、IoPortRange、映射 token、MmioOp、IoMap 的访问及释放 |
| irq | Virq、IrqSpec、已映射资源、MSI 描述、IrqOp、handler、注册 token、DevIrq |
| dma | DmaInfo 地址视图、请求/映射描述、DmaOp、CoherentMem |
| clock | ClkSpec、ClkOps、ClkHandle、EnabledClock 的 prepare/enable 配对 |
| error | 统一的 OS 能力失败分类 ResError / ResResult |

ResourceMap、ResourceEntry、ResourceKind 是 model 的设备枚举集合，仍留在 model。
DeviceServices 携带 DeviceId 并参与依赖等待，也留在 model。
RuntimeError、ServiceError、ProbeError 不下沉；model 负责从 ResError 转换。
clock 原来使用模型错误，现改用 ResError；保留 BadState、AlreadyExists、
NotFound 和一般 Io 分类，MappingFailed 仍供映射失败使用。

## 生命周期

### DMA 地址约束

`DmaInfo` 将设备 mask、总线窗口与 coherency 分开。`DmaWindow` 保存已解析的
CPU 起点、设备起点和窗口最后偏移，构造时验证两端不溢出且长度非零。
不再用一个有符号 offset 代替窗口；地址差大于 i64 也能无损表达。
参考 Linux `include/linux/dma-direct.h` 的 `bus_dma_region` 职责，但当前只支持
一个连续窗口。固件多段窗口不能包成一个大窗口，转换方应拒绝尚不支持的配置。
OF 的局部 parent 地址必须先完成祖先转换，不能直接当作 CPU 地址。

宿主对完整分配/映射调用 `translate_range(paddr, len)`，同时检查窗口下界、
上界、长度溢出以及连续低位设备 mask；单地址 `translate` 仅用于单字节查询。
`DmaWindow::IDENTITY` 是显式全地址恒等策略，不表示缺失固件属性。
设备描述仍为唯一配置来源，宿主通过设备上下文读取；不增加第二份可变配置。

IoMap、DevIrq、CoherentMem、ClkHandle 保存宿主提供的 ReleaseHook，
Drop 通过 Option::take 至多调用一次。没有 hook 不代表拥有实际硬件资源；
宿主负责建立正确的获取/释放配对，驱动必须先停止硬件和依赖它的操作。

EnabledClock 拥有一份 ClkOps Arc，独立于 ClkHandle 生命周期。
prepare 成功而 enable 失败时只 unprepare；成功 token Drop 执行 disable、
unprepare；keep_running 显式放弃自动平衡。此行为沿用新模型原实现。

DevIrq 是设备服务返回的拥有型句柄；IrqResource 和 IrqHandlerToken 是 provider
交换的描述与注册凭证，不能混为消费者句柄。IRQ 资源枚举继续区分 PlainVirq 和
DomainMapped，保留来源域/hwirq 与强类型 Virq。默认 map_irq 只适合恒等映射宿主；
非恒等域必须覆盖该方法，不能把 hwirq 当成已解析 virq。

IrqHandlerFn 为 Fn()->IrqAction，闭包携带设备上下文。普通 IRQ 调用不得睡眠；
不用于 pseudo-NMI，没有 threaded IRQ 注册入口。宿主负责内核回调转换。

## 上下文与验证

资源获取与释放一般在任务上下文，由具体宿主实现约束；Drop 不保证可在 IRQ/NMI
上下文执行。MMIO 访问也不替驱动建立设备存在性或并发访问协议。
本 crate 没有锁或可变全局状态。

ut-tests 覆盖 IRQ 词汇/映射、handler 契约、DMA 地址变换、guard 释放、
时钟 enable 失败回滚和 token 保活；model 测试覆盖错误转换和设备生命周期整合。
