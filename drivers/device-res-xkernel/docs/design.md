# device-res-xkernel — 设计文档

## 定位

`device-res-xkernel` 是 X-Kernel 对 OS-neutral `device-res` provider contract 的
实现层。它不负责驱动匹配、probe/remove 编排或设备发布；这些仍属于 `kdriver`。

本 crate 的职责是把驱动子系统需要的资源能力接到 X-Kernel 内核子系统：

- `device_res::MmioOp` -> `memspace::iomap_device` / `memspace::iounmap`
- `device_res::DmaOp` -> `kdma` coherent / streaming DMA API
- `device_res::IrqOp` -> `kirq` shared hardirq、regular threaded IRQ、MSI-X API

## 架构

```text
driver crates / kdriver
        │
        │ OS-neutral resource API
        ▼
device-res
        │
        │ provider traits
        ▼
device-res-xkernel
        │
        ├── memspace
        ├── kirq
        └── kdma
```

源码按 provider trait 边界拆分：

```text
drivers/device-res-xkernel/src/
├── lib.rs   # provider 安装、DriverError 映射和 devm_* helper
├── mmio.rs  # MmioOp -> memspace
├── dma.rs   # DmaOp -> kdma
└── irq.rs   # IrqOp -> kirq
```

`install_resource_provider()` 在 X-Kernel driver init 早期安装同一个
`XKernelResourceProvider` 到 `device_res` 的 MMIO、DMA 和 IRQ provider registry。

## IRQ 适配

`XKernelResourceProvider` 不维护本地 IRQ line state：

- `request_irq()` 调用 `kirq::try_register_shared()`，返回
  `device_res::IrqHandlerToken::SharedAction(id)`。
- `request_threaded_irq()` 调用 `kirq::try_register_threaded()`，当前只支持空 line 上的
  non-shared regular threaded action，并返回 `IrqHandlerToken::RegularAction`。
- `request_threaded_irq_default()` 调用 `kirq::try_register_threaded_default()`，继承
  `kirq` generic ONESHOT 语义，并返回 `IrqHandlerToken::RegularAction`。
- `release_irq()` 根据 token 区分 shared hardirq action 和 regular threaded action：
  shared token 调用 `kirq::try_free_irq_action()`，regular token 调用
  `kirq::try_free_irq()`。

`device_res::IrqEvent::WAKE_THREAD` 和 `wake_thread_from_sources()` 会转换为
`kirq::IrqEvent` 的 wake-thread 语义；普通 `HANDLED` 不唤醒 IRQ thread。

## Devres Helpers

本 crate 提供 X-Kernel 风格 `DriverResult` helper：

- `devm_iomap()`
- `devm_request_irq()`
- `devm_request_threaded_irq()`
- `devm_request_threaded_irq_default()`
- `devm_alloc_coherent()`

这些 helper 只是 `device_res::devm_*` 的薄适配，负责把 `ResError` 转换成
`driver_base::DriverError`，并接受 X-Kernel 的 `kdevice::DeviceObject`。

## 所有权边界

- `device-res` 定义驱动需要的资源能力，不依赖 X-Kernel 内核子系统。
- `device-res-xkernel` 实现这些能力并依赖 `kirq`、`memspace`、`kdma`。
- `kdriver` 负责设备发现、匹配、probe/remove 和 re-export helper，不实现 provider。
