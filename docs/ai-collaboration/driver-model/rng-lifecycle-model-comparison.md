# RNG 全生命周期：X-Kernel / Linux 模型横向评审

状态：源码对照与改进建议，尚未批准本页建议的代码修改。
X-Kernel 基线：`7f9798126aca8af461222d89a2cd72fc0c06d63a`。
Linux 基线：`8d3ae59288f1e7d58d76558a6ee96d533bc5019f`，本地 `/root/linux-stable`。
范围是当前 QEMU Arm modern-MMIO RNG 接入，不把未来 transport 迁移设计当成实现。
平台迁移暂停；本次不改生产代码、不执行构建或运行时测试。

## 1. 先明确“卸载”的三个动作

1. **解绑设备驱动**：结束该设备的本次绑定；设备对象仍注册，可以再次 probe。
2. **注销设备**：结束绑定并从设备注册表移除；旧引用不再有重新绑定的资格。
3. **注销驱动定义 / 卸载模块**：撤下驱动候选，并解绑它绑定的设备；代码卸载还
   涉及模块引用。本项目当前没有提供这第三项，不能把 `remove_device` 当成它。

前两项当前分别是 `DeviceDomain::unbind_device` 和 `remove_device`。
`DriverOps::remove` 是框架调用的解绑回调，不是“注销整个驱动”的管理入口。
来源：model `src/domain/lifecycle.rs:36`、`:49`、`src/driver.rs:44`；Linux
详细调用链见 [Linux 源码笔记](rng-linux-lifecycle-reference.md)。

## 2. 对象图：职责相似，但当前层数不同

```text
当前 X-Kernel
DT view → RNG 过渡选择/资源预留 → PlatformBus 上的 DeviceRef
                                      │ match compatible
                                      ▼
                            RngDriver::probe → RngBinding
                                      ├─ IoMap
                                      ├─ VirtIoRngDev / vendor VirtIORng
                                      └─ HwrngRegistration → HwrngHandle → entropy

Linux
DT node → platform_device → virtio_mmio platform_driver
                               │ 注册 child
                               ▼
                       virtio_device → virtio_rng_driver
                                          └─ virtrng_info / hwrng → hwrng core
```

不能把当前 `RngDriver` 直接等同 Linux `virtio_rng_driver`：它同时承担平台资源
获取、MMIO transport 创建及 RNG 功能接入。Linux 把前两项放在 transport 层。
这是已知迁移债务，不是现在继续扩展新旧分流器的理由。

## 3. 按生命周期横向对照

| 阶段 | 当前 X-Kernel | Linux 的职责归属 | 结论 |
|---|---|---|---|
| 发现 | PlatformBackend 使用 firmware view；特选现代 MMIO RNG；PlatformModel 预留来源/MMIO | OF 平台人口创建 platform_device；MMIO transport 在自己的 probe 中识别设备 | 当前发现混有过渡路由；迁移暂停，不再加新功能分支 |
| 注册设备 | StagedRng 构造 DeviceDescriptor；domain.add_device 注册并可能立即 probe | device 注册与驱动绑定不同；virtio device 由 transport 注册 | 注册成功不代表绑定成功，这个区分应保留 |
| 注册驱动 | DriverDesc 保存名字、匹配表、DriverOps；domain 保存候选 | platform_driver / virtio_driver 注册到所属 bus | 驱动定义不是每设备运行实例 |
| match | PlatformBus 按 compatible 判断；domain 按注册顺序尝试 | platform bus 匹配 transport；virtio bus 按 device/vendor 匹配功能驱动 | 跨驱动评分已删除；剩余层次差异属于迁移 |
| probe | 临时 DeviceAccess + Environment；返回 RngBinding；管理锁覆盖 | probe 在设备锁约束下执行，私有状态附到设备 | typed Data 合理，不需为了像 C 改成裸 drvdata |
| 资源 | DeviceAccess.res → host services → os-api guard；驱动决定关闭顺序 | 平台 devm/MMIO、transport virtqueue/IRQ、功能驱动私有数据 | 可移植 host seam 是本项目额外需要，不是无意义 wrapper |
| 发布 | probe 内 HWRNG.register；获得可显式注销的凭据 | 本地 virtio-rng 在 probe 成功、DRIVER_OK 后的 scan 注册 hwrng | 时点不同；当前发布前 runtime 已可用且发布失败会回滚，不能仅因早于 domain commit 判为缺陷 |
| 使用 | HwrngHandle 的准入计数保护调用；runtime 管理请求与 IRQ | hwrng core 管理选用/读取引用；virtrng_info 管理请求完成 | 发布层与硬件层有不同并发责任 |
| 解绑 | close_reads → hwrng unregister/drain → reset 确认 → IRQ drain → 队列释放 → MMIO 释放 | RNG remove、hwrng unregister、virtio reset/del_vqs 各自负责 | 对照责任和先后约束，不机械照抄 Rust/C 析构布局 |
| 注销设备 | 先关闭注册存活状态，再解绑、删除条目；旧 DeviceRef 仅留元数据 | device_del/unregister 与最终引用释放分开 | 设备身份寿命与硬件资源寿命分开是必要设计 |
| 重绑 | 保留设备描述，新 probe 返回新 Data；旧消费者永久关闭 | 保留设备对象，重新建立驱动私有状态 | 旧 publication 不能重新变为新实例；当前 BindingSource 固定发布驱动身份合理 |
| 驱动注销 | 无相应生产管理入口 | driver_unregister/bus_remove_driver/driver_detach | 当前 boot-static RNG 不需要；不得宣称已支持模块卸载 |

发布时点比较中的 Linux “probe 成功”指功能驱动 `drv->probe` 返回；scan 仍在
VirtIO bus probe 内，不能据此说它一定晚于整个 driver-core 绑定提交。

X-Kernel 证据入口（相对仓库）：

- `drivers/integration/kdriver/src/bus/platform_backend.rs:156`：过渡路由。
- `drivers/integration/kdriver/src/platform_model.rs:89`、`:195`：装配与注册。
- `drivers/base/model/src/domain/registry.rs:65`、`:248`：驱动/设备注册。
- `drivers/base/model/src/domain/probe.rs:113`：候选匹配、probe 和提交。
- `drivers/integration/kdriver/src/driver_registry/virtio/rng_model.rs:57`、`:155`：probe 与退出。
- `drivers/devices/virtio/src/rng.rs:402`、`:435`：关闭读取、停止硬件。
- `drivers/subsystems/hwrng/src/registration.rs:69`、`:177`、`:263`：发布、注销、准入。
- `drivers/base/model/src/device/source.rs:29`：发布来源固定在原绑定。

## 4. 建议本轮先处理的具体问题

### A. 过度设计：设备管理锁与生命周期 generation 重复保护 probe 提交

model `src/domain/probe.rs:169` 检查 probe 开始时取得的生命周期 generation；
不一致就执行一套未提交 Data 的 remove/drop 回滚。生产中该 generation 只在
`src/domain/lifecycle.rs:120` 的解绑中递增，而解绑和 probe 持同一个设备管理锁。
合法管理接口不能在 probe 中并发或重入修改它。

对应测试 `ut-tests/lifecycle.rs:14` 明确写明不是可达管理竞争，而是直接修改
内部 generation 人工制造失败。它证明回滚代码可执行，不能证明需要这个机制。

建议删除生命周期版本计数及其专属提交失效协议，用管理锁和内部状态不变量解释
唯一绑定；保留身份检查。**不要删除依赖唤醒 generation 或驱动注册表变化检查**：
前者防止真实丢唤醒，后者处理其他设备/线程注册候选的真实交错。
这是简化建议，不是已发现当前 RNG UAF，也不授权跳过合法错误处理。

### B. 功能可观测性缺口：probe 失败的具体原因被框架丢弃

model `src/domain/probe.rs:196` 匹配 `Fatal(_)`，仅记录候选失败并递增计数。
`add_device` 内部调用 `probe_pending` 不返回该统计，`DeviceSnapshot` 也没有错误
原因。RNG 在 DMA 校验、资源取得等位置可以返回 Unsupported/Invalid/Io；即使
底层没有日志，最终也只剩“未绑定”，无法从框架判断哪个驱动为何失败。

Linux `drivers/base/dd.c:620` 的 `call_driver_probe` 对 defer、拒绝匹配、其他
probe 错误分别输出诊断。这不是要求把 add_device 改成“probe 失败就注册失败”。
建议先在统一 probe 失败处理处记录设备、驱动、具体错误，必要时再增加最近失败
查询；不要为此建立另一套日志注册表。发现统计也要明确旧路径口径，避免漏掉
新模型却仍声称是总数（kdriver `src/enumeration.rs:150`、`src/manager.rs:59`）。

### C. 命名先修正语义，不为统一词汇扩大重构

| 当前名称 | 实际职责 / 容易误解处 | 建议 |
|---|---|---|
| `DeviceDomain::remove_device` | 解绑并注销设备；与 DriverOps::remove 容易混同 | 优先考虑 `unregister_device`；`unbind_device` 保留 |
| `DeviceState::Active` | 已绑定初始化，不代表正在工作、PM active 或子系统可用 | 候选 `Bound`，与“绑定状态”对齐 |
| `DeviceRef::is_online` | 注销尚未关闭；未绑定设备也为 true | 明确为注册存活状态；若改 `is_registered`，需注明注销开始即 false，而非条目已物理删除 |
| `DeviceState::Removed` | 当前注册条目直接删除，这是保留但不实际提交的状态 | 审查外部消费者后考虑删除，不人为加入墓碑来配合名字 |
| `PlatformModel` | 当前只是 QEMU boot-MMIO RNG 的装配/所有权管理者 | 建议名称包含 `Rng` 与 `Registration`，不要暗示通用平台模型 |
| `PlatformModel::root()` | 安装 host services、注册总线/驱动并缓存初始化结果，不只是 getter | 候选 `ensure_root_registered()`，显式表示初始化副作用 |
| `RngDriver` | 当前是 platform/MMIO RNG 接入，不是通用 VirtIO 功能 bus driver | 候选 `VirtioMmioRngDriver`，后续真正迁移再分层 |
| `RngBinding` | 本次绑定的资源及显式退出责任 | 保留，含义清楚 |
| `DeviceDomain` | 跨 bus 的注册与生命周期协调容器，不是 IRQ/IOMMU 硬件域 | 先补类型职责说明；不为换词大范围改成含糊的 Manager |
| `DeviceCore` / `DeviceRef` / `DeviceAccess` | 私有身份存储 / 持有引用 / 回调临时访问 | 保留，三者不是三个设备实例 |
| `model` / `os-api` / `kdriver` | 可移植对象编排 / 宿主能力合同 / X-Kernel 适配装配 | 保留；Linux 不需要同构的 OS 移植 seam，不能据此合并 |

另外 `attempted` 注释称“永久失败”不准确：显式 `bind_device` 会清空它
（`src/domain/probe.rs:92`）。应说明它是显式绑定尝试轮次内的失败候选历史。
`physical_path` 当前来自 `path_component`，不是自动拼接父拓扑的 Linux sysfs
DEVPATH（`src/device/mod.rs:87`、`src/bus/platform_bus.rs:108`）；文档不应声称
已具备完整 DEVPATH 命名空间。当前 RNG 不依靠该字符串授予访问或做发布唯一性。

## 5. 扩展缺口：要记录，但不冒充当前 RNG 故障

- **provider 的撤销合同不完整**：`Environment::provide` 接受 static lookup，
  不返回撤销凭据；rustdoc 要求它作为最后一个可失败步骤。Linux clock provider
  有 `of_clk_del_provider` 和 devm 注册释放配对。当前 native host 的
  `register_provider` 返回 Unsupported（kdriver `src/host_services.rs:146`），
  RNG 不用该路径；这是将来 provider 驱动迁移的前置，不应现在建立完整通用框架。
- **驱动定义注销未实现**：明确 boot-static 范围。未来实现时先撤候选再逐设备
  解绑；设备注销不等于驱动注销，也不等于可卸载代码模块。
- **平台/transport/功能总线未拆开**：沿用暂停的迁移文档，不在本轮重启。
- PM、冻结恢复、模块自动加载不是本次 RNG 生命周期验收目标；不能仅因为 Linux
  有就要求全部补齐，也不能在 PR 中暗示这些已经支持。

## 6. 应保留的机制及原因

- `DeviceRef` 与运行资源分离：陈旧引用需要可安全查询，不应导致 MMIO 永久保活。
- typed `Data` 和回调 `DeviceAccess`：Rust 所有权表达，不是多造一个设备层。
- hwrng admission 与 runtime close：前者阻止/排空消费者调用，后者唤醒阻塞读者
  并停止硬件请求；仅注销注册表不能唤醒正在等待设备完成的调用。
- reset 确认与 IRQ drain：前者停止设备 DMA，后者等待包括 notify 尾部的软件
  回调；两者互相不能替代。当前多个资源阶段不是仅靠重排 Drop 字段就能删除。
- `PublicationSource` 的绑定来源与 DeviceRef 当前驱动不同：旧发布不能在 rebind
  后显示成新驱动，不应重新合并为一个随设备变动的 driver_name。
- host services / os-api guards：让可复用驱动不直接依赖 kirq、ktask、memspace。

## 7. 建议收口顺序

先讨论 A（删除冗余生命周期提交协议），再处理 B（probe 诊断），再按 C 中确认的
名称做独立重命名提交。provider/驱动注销只补能力范围说明，迁移继续暂停。
每一项实现后独立测试和 review，再纳入分批 PR；本对照不替代这些实现验收。
