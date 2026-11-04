# 平台迁移设计（一）：对象关系、入口与生命周期

状态：设计评审稿，尚未批准；不是可直接开发的完整接口规范。
基线：`ba134896a523faf61a6fd395c8c739c511bb236a`。
用户在范围盘点后确认进入设计：保留旧 PCI 子树例外，首个可启动验收目标为
AArch64 QEMU 的平台串口接管 + modern-MMIO block/RNG；不改变默认配置。
范围依据：[平台枚举迁移盘点](platform-enumeration-migration-scope.md)。
本稿不改生产代码、默认配置、vendor 或已完成的 R1 设计，不新增开发 tickets。

## 1. 先分清三种对象

| 对象 | 表达什么 | 不是什么 |
|---|---|---|
| 平台设备描述 / 平台 DeviceRef | 固件或板级声明的资源位置、匹配身份和拓扑 | 不是 RNG、block 的运行实例 |
| VirtIO 设备描述 / VirtIO DeviceRef | transport 识别的功能设备，含 device/vendor 匹配事实 | 不是第二个独立拥有同一 MMIO 的平台设备 |
| 驱动绑定 Data | 本次绑定的运行能力、资源和显式退出责任 | 不是可无限保留的只读设备元数据 |

一个 MMIO transport 对应一个平台设备，至多产生一个非空 VirtIO 功能设备。
两个 DeviceRef 表达不同层次，不是一个设备在新旧模型中重复注册。
功能设备的 parent 指向平台设备；绑定资源不能藏进任一描述或 DeviceRef。

```text
新 domain
  platform bus instance（启动装配建立）
    PL011 平台设备 → 串口接入驱动 → console/TTY
    virtio,mmio 平台设备 → MMIO transport 驱动
       │                        │ 拥有 MMIO 与下游注册责任
       └─ parent ← VirtIO 功能设备（属于 virtio bus）
                        → RNG driver → hwrng → entropy
                        或 block driver → block → filesystem
```

来源核对：Linux `drivers/virtio/virtio_mmio.c:572` 设置 virtio device 的 parent
和 config ops，再由 `register_virtio_device` 注册；`:653` 的 remove 注销下游。
`drivers/virtio/virtio.c:526` 使用共享 virtio_bus；`:368` 在功能驱动 remove 前
关闭 config 回调，要求驱动完成 reset。这里只借鉴职责，不照搬 C 布局或 WARN 策略。

## 2. 模块划分与命名候选

| 模块 | 对外职责 | 内部实现及不应承接的事 |
|---|---|---|
| 固件/板级发现 | 输出符合平台人口规则的事实 | 不按已注册功能驱动过滤，不分配队列、不启动设备 |
| kdriver 启动装配 | 固定新旧入口，安装 host services、注册 bus/driver | 不实现 VirtIO 协议；不在失败后切回旧模型 |
| 新平台注册模块 | 描述构造、设备唯一归属、静态/固件来源与 domain 对接 | 不检查 VirtIO device type 以选择框架 |
| VirtIO bus 模块 | device/vendor 匹配词汇，功能驱动接入，transport 能力合同 | 不扫描 DT，不调用宿主内核；不另造通用 class |
| VirtIO-MMIO transport 模块 | 平台资源到 VirtIO 设备的转换、下游注册/退出 | 不根据 RNG/block/net 类型自己调用对应构造函数 |
| RNG / block 接入模块 | 本次功能绑定、子系统发布与显式关闭 | 不再次映射平台寄存器或创建旧 DeviceObject |

候选名 `VirtioBus`、`VirtioDeviceData`、`VirtioId` 分别指总线匹配规则、只读
设备事实与驱动匹配项；其中 device/vendor id 是协议身份，不是新公共索引。
不要把队列或 transport 放进 `VirtioDeviceData`。版本属于 transport，协商特性
属于运行绑定，均不作为静态功能匹配身份的第二份可变权威。

暂不定新 crate 名或创建 trait-only crate。VirtIO 特有规则必须位于 model 之外，
具体目录落点待 transport 能力合同收口；入口内部的转换/预留逻辑保留为 mod。
通用 model 继续只提供 Bus、DeviceDescriptor、DeviceRef、DriverOps 和管理编排。

## 3. 新旧过渡：选择入口，不选择设备功能

提出一个启动装配语义：本次启动的普通平台人口由 **旧入口或新入口之一**负责。
将来用一个明确的配置选择表达，具体 Kconfig/API 名尚不定稿。现有
KFEAT_VIRTIO_BUS_MMIO/PCI 只决定 transport 配置，不应兼任新旧框架选择。

- 默认模式维持现有运行行为（包括当前 R1 RNG 特选接入），作为兼容对照。
- 新验收模式完全绕开旧普通平台发现/注册，不能先在旧表登记再转交新表。
- 新模式中所有选定平台设备走新模型，未支持功能保持未绑定，不按 RNG 类型
  回落旧路径。新模式启用时也不能同时启动旧的 RNG 特选入口。
- PCI backend 保持旧 host/bridge/endpoint 子树；其 platform host 是明确例外，
  新平台人口不能再次注册同一个 host。
- 旧 PCI 仍需其旧 platform bus 锚点；保留锚点不等于继续运行旧 PlatformBackend
  枚举。不能依赖 PCI 后端“找不到 platform bus 就丢掉父关系”的降级路径。
- “平台人口”不是所有 DT 节点。根/总线递归、available、early console 接管、
  irqchip/timer/PCI 等已有所有者、资源不可表示情况均要有明确规则。

共享的是发现事实及稳定来源，不是两份注册管理权。不增加运行时切换、框架
优先级、probe 失败自动回退或通用设备路由注册中心。新入口的去重/预留状态应
由一个清楚的装配所有者维护，不能继续借 RNG feature 决定是否启用全平台保护。

## 4. 复用现有 API 与需要改变的接入

以下为现有 API，不是新接口提案：

| API | 本设计中的用途 / 限制 |
|---|---|
| `DeviceDomain::set_services` | 启动装配安装 host bundle；不能围绕 child 注册临时切换全域服务 |
| `add_bus_instance` | 创建匹配词汇的注册锚点，不自动提供 transport 或递归退出 |
| `DeviceDescriptor::builder` | 注册前描述事实；不传 runtime/队列/MMIO 映射 |
| `add_device` | 注册并可能同步触发 probe；返回成功不代表绑定成功 |
| `register_driver(DriverDesc)` | 按总线词汇匹配，可能探测已有设备 |
| `DriverOps::probe/remove` | 创建/显式关闭本次 Data；probe Err 的回滚由驱动完成 |
| `unbind_device/remove_device` | 前者结束绑定，后者注销；不自动替控制器清理所有下游 |
| `DeviceAccess::publication_source` | 功能子系统的只读来源，不授予 transport 访问权 |

建议每个 domain 由启动装配建立一个逻辑 VirtIO bus instance，共享总线类型；
各功能设备的 parent 表达实际 transport 归属。它不是一条伪造的共享物理总线。
不在每次 transport rebind 时无条件 add_bus_instance：当前模型没有对应完整
实例退休接口，反复创建会引入额外生命周期问题。实例粒度仍须随下一节评审。

## 5. 必须先收口的接口缺口：transport 如何交给功能驱动

不能只写“功能驱动获取 transport”就当设计完成。现有代码有四个约束：

1. `BusDeviceData` 只允许描述性事实，DeviceRef 最终释放上下文不保证可睡眠。
   不允许通过描述中的 Arc 偷偷保活硬件。见 model `src/bus/mod.rs:68`。
2. `Environment::extension` 返回 boot-static 服务；provider provide 同样不是
   绑定级可撤销通道，不能临时注册 parent transport 并冒充静态 provider。
   见 model `src/environment.rs:26`、`src/extension.rs:34`。
3. parent probe 调用 add_device 时，child 可以立即 probe；parent Data 尚未提交。
   `with_data` 仅接受 Active 绑定并取得 parent 管理锁，所以 child 不能用
   “查找 parent Data”取得 transport；后续 child remove 也不能反向等待 parent
   正持有的管理锁。见 model `src/domain/registry.rs:248`、`lifecycle.rs:204`。
4. vendor 的功能对象按值持有 Transport；MmioTransport 的 Drop 会 reset。
   不能为了给两个模型对象共享而随意 Clone、制造第二个 raw transport 或增加
   不受约束的 Arc<Mutex<_>>。见 vendor `src/transport/mod.rs:26`、`mmio.rs:589`。

需要设计的最小能力：**与具体 VirtIO 设备关联、在 probe 前可取得、独占本次
功能绑定、可明确关闭且不能被旧句柄重新打开的 transport 接入。** 退出时归还/
销毁 raw transport、reset、IRQ、DMA 的顺序必须由具体合同说明。

下一设计片比较两种接入位置，而不是立即增加代码：

- model 的注册入口是否缺少一个与纯描述分离的、设备级运行服务注入点；仅当
  它确实属于通用设备注册职责才修改 model，不增加 `get_virtio_transport`。
- 或者由 VirtIO bus 自身拥有正式、显式注销的设备运行对象，通过窄接口接入
  model；必须说明同步 probe 前关联、失败撤销、旧引用与 parent teardown。

两者都必须避免为查找服务再造一套公开 ID、隐藏全局保活表或第二套生命周期。
在该 seam 定案前，本稿不提供虚假的 `take_transport()` 函数签名、不承诺靠
现有 API 已能实现，也不进入 transport 功能开发。如果需要前置机制，应先列为
独立设计与验收项，而不是在驱动里积累绕行代码。当前未授权修改 vendor。

## 6. 生命周期顺序（责任已分清，机制仍待第 5 节定案）

### 发现与绑定

1. 启动装配选择入口，准备 host 能力和 bus/driver；所有可能 probe 的入口在
   可睡眠上下文执行，发现/注册表短锁不跨驱动回调。
2. 枚举构造平台描述并注册。`virtio,mmio` 匹配 transport 驱动，不匹配 RNG。
3. transport probe 独占取得映射与服务、检查 header/version/设备类型。
   device id=0 不创建功能 child；平台 slot 是否保留为未绑定设备须在人口规则明确。
4. transport 建立可供 child 使用的能力后注册 child，准备承担同步 child probe
   和所有后置失败回滚。child 的 resource acquisition 必须落到正确设备约束。
5. VirtIO bus 按 device/vendor 匹配功能驱动；RNG 或 block 初始化自身运行对象
   并发布子系统能力。无匹配不等于 transport probe 必须失败。

### 功能解绑与 transport 退出

- **仅解绑功能驱动**：功能驱动关闭消费者准入、唤醒/排空自身工作，完成规定的
  reset、IRQ 同步、队列/DMA 回收及发布撤销；保留 VirtIO 设备注册，可重新绑定。
  不从来源设备在线位推导这些工作已经完成。具体先后必须保留各家族验证过的合同。
- **解绑平台 transport**：先关闭产生 child 的入口，再同步注销自己拥有的 child，
  等 child 的完整功能退出结束，再释放 transport 服务及 MMIO。保留平台设备
  注册；下次 probe 产生新的 child 身份，旧 child 引用不能指向新的运行能力。
- **注销平台设备**：在 transport 退出后结束平台注册；不等待所有只读引用耗尽。
- **probe 失败**：谁取得资源/创建 child 谁回滚。child add 成功不保证 child 已绑定；
  parent probe 在 child 注册后失败时仍需注销 child，不允许遗留可运行子设备。

reset 不成功时不得继续释放可被访问的 DMA/MMIO。不为本阶段承诺硬件故障的
可恢复重试或固定退出时限。现有 remove 无 Result；网络 fallible close 的所有权
问题继续随网络迁移解决，不能在这里静默新增失败隔离表或宣称已经支持。

## 7. 后续评审顺序与验收草案

先评审本稿的对象和入口，再依次收口：

1. 第 5 节 transport 运行能力交接与 vendor 所有权适配（前置设计）。
2. 平台人口及描述转换合同：重复发现、禁用节点、已拥有基础设施、空 slot、
   多资源和 DMA 未配置/不可表示；不再用旧 driver match list 表达可发现设备。
3. 串口接管和 block 迁移合同；根盘只做正常使用，退出测试用独立非根设备。
4. PR 依赖图和可独立演示的切片；详细设计验收后才生成开发 tickets。

未来测试至少覆盖：新旧入口互斥、失败无旧回退、PCI host 不重复且拓扑保留、
无驱动设备可注册、空/未知/不支持 transport 的区别、同步 child probe 及后置
失败、parent remove/child operation 交错、child 与 parent 各自 rebind、两实例
隔离、旧句柄失效、console/rootfs 不回退、现有 RNG/PCI 接口兼容。
这只是验收输入，不是已执行结果，也不声称本阶段完整设计已完成。
