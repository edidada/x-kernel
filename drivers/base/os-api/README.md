# driver-os-api

驱动框架要求 OS 提供的能力契约，不是某个内核的实现或通用 OS 函数集合。

- MMIO、IRQ、DMA、clock 按能力聚合描述、Op、返回值与所有权句柄。
- 宿主实现 Op；驱动通过 model 的 Device 服务获取句柄并安排释放顺序。
- 不拥有 DeviceId、probe/remove、deferred probe、provider 注册表或 class。
- 只通过 irq-types 共享 IRQ 基础词汇，不依赖 kirq 或旧 device_res。
- 与旧 device_res 暂时并存；本次没有迁移旧模型调用方。

参见 [设计](docs/design.md) 和 [安全分析](docs/security.md)。
