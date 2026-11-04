# 驱动设备模型基础设施

## 职责与边界

`drivers/base` 是与设备家族和宿主实现无关的模型基础设施，不是所有驱动公共代码的集合。
设备的注册、绑定、解绑、注销由 model 编排；能力的注册、操作准入和同步排空由
具体子系统拥有。保留身份引用不意味着保留硬件操作权限。

| 目录 / package | 职责 |
|---|---|
| `model/` / `driver-model` | 设备事实、总线契约、绑定与退出编排、只读发布来源 |
| `os-api/` / `driver-os-api` | OS 能力描述、Op、资源句柄及释放协议 |

依赖方向为 `hwrng → model → os-api`、`entropy → hwrng`。
hwrng 只使用模型的只读 `PublicationSource` 契约，不调用域管理或检查绑定状态。
model 不依赖设备家族或宿主内核。宿主适配层实现 OS 能力；可复用驱动不直接
调用 kirq、ktask、kdma 等宿主 API。

## 模型对象

- `DeviceDescriptor` 是枚举事实的唯一权威，注册后由私有 `DeviceCore` 拥有。
- `DeviceRef` 保留身份和静态事实，不保留驱动绑定资源。
- `DeviceAccess` 是 probe/remove 回调期间的借用视图，提供 info/res/fw。
- `DeviceDomain` 拥有注册表、绑定状态和管理编排；内部管理键不作为公开身份。
- `DriverOps::Data` 拥有本次绑定的运行对象；remove 先完成收尾，再释放 Data。
- `PublicationSource` 只读查询发布绑定的驱动身份、物理路径和设备注册在线状态。
  私有 BindingSource 保留 DeviceRef 与原驱动的只读身份，不保留 ops 或运行资源。
  unbind/rebind 不改变旧来源报告的驱动；设备在线位不充当子系统操作许可。
- `Environment` 提供 host provider 与扩展查询，不拥有子系统的注册生命周期。

设备服务携带设备上下文取得 OS 资源，RAII 释放不能代替显式停机、IRQ 同步排空
及在途操作退出。新 os-api 与旧 device-res 暂时并存，不能将其误认为重复可删接口。

## 子系统与旧兼容路径

hwrng 拥有 HwrngRegistry、唯一 HwrngRegistration 和操作句柄 HwrngHandle。
注册凭证 Drop 不自动注销；驱动先停止准入、唤醒读者，再显式注销并排空。
元数据最终析构不得要求睡眠。硬件释放必须在有保证的 teardown 上下文完成。

旧 `base/class` 的泛型注册表、许可、finalizer 及 `DeviceAccess::publish`
已在消费者迁出后退役。其只读来源契约归回 model，不另建只装一个 trait 的 crate。
仍使用旧设备模型的 PCI RNG 通过 hwrng 的 `attach_legacy` 接入，不能删除该桥。
旧 `contracts/kclass` 与 DeviceNode 仍负责 Input/DRM 投影，与已退役泛型层不同。

## crate 与 mod

按内聚的状态、不变量、生命周期及上下文建立 crate；仅属单个所有者的实现细节
保留为 mod。domain 的 registry/probe/dependency/lifecycle 共同维护域状态，
不按文件机械拆 crate。source 是设备身份的内部实现，不是新通用发布框架。
base 不增加总入口或 common/types 兜底 crate；具体数量随真实责任演进。

## 相关文档

- [驱动边界硬规则](../../docs/ai/driver-subsystem-boundaries.md)
- [驱动目录分层](../README.md)
- [模型设计](model/docs/design.md) 与 [安全分析](model/docs/security.md)
- [OS 能力设计](os-api/docs/design.md) 与 [安全分析](os-api/docs/security.md)
- [hwrng 设计](../subsystems/hwrng/docs/design.md) 与 [安全分析](../subsystems/hwrng/docs/security.md)
