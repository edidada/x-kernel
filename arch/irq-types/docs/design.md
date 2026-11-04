# irq-types — 设计

## 职责

IRQ 领域的共享值词汇，不是驱动资源层或 IRQ 核心的缩小版。
仅定义 IrqTrigger、IrqController、IrqDomainId。无依赖、no_std、无分配。

```text
kirq ──────────→ irq-types
driver-model → driver-os-api → irq-types
device-res ──→ driver-os-api → irq-types
```

IrqController 表达类别，不是控制器实例。Unknown 触发方式保留原始标志。
IrqDomainId 是宿主分配的不透明 u32；本 crate 不分配编号、定义根域常量、
检查域是否存在或解释固件域。相同数值只在同一个宿主域命名空间中有意义。

## 不属于本 crate

kirq 的描述符合并、域注册映射、handler/event、分发、同步退出和根域常量；
driver-os-api 的强类型 Virq、资源状态枚举、provider Op、token 和资源 guard；
旧 device_res 的资源结构、回调和获取契约。其 trigger/controller/domain
现引用同一份词汇；尚未迁移的契约不因共享词汇而自动合并。

全部实现位于 src/lib.rs：三个紧密相关的值类型不再机械拆 mod。
没有初始化、锁、回调或生命周期操作，可用于任何执行上下文。
现有 kirq 的导出路径保持兼容；model 的公开资源词汇引用同一类型定义。

## 验证

kirq 原有测试覆盖这些类型参与描述符和映射的行为。
driver-os-api UT 验证共享类型赋值、不透明域与未知触发标志保留，
以及新模型独立的 handler 契约。类型提取不改变 IRQ 核心行为。
