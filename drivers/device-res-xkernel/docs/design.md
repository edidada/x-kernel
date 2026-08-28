# device-res-xkernel — 设计文档

## 定位

`device-res-xkernel` 是 X-Kernel 对 OS-neutral `device-res` provider contract 的
实现层。它不负责驱动匹配、probe/remove 编排、设备发布或 driver-facing
`devm_*` API；这些分别属于 `kdriver` 和 `device_res`。

本 crate 的职责是把驱动子系统需要的资源能力接到 X-Kernel 内核子系统：

- `device_res::MmioOp` -> `memspace::iomap_device` / `memspace::iounmap`
- `device_res::DmaOp` -> `kdma` coherent / streaming DMA API
- `device_res::IrqOp` -> `kirq` shared hardirq、MSI-X API

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
├── lib.rs   # XKernelResourceProvider 类型和模块边界
├── mmio.rs  # MmioOp -> memspace
├── dma.rs   # DmaOp -> kdma
└── irq.rs   # IrqOp -> kirq
```

`XKernelResourceProvider` 是本 crate 唯一对外类型。它实现 `device_res` 要求的
provider traits，但不拥有全局 provider registry 的安装时机。X-Kernel driver init
早期由 `kdriver::resource::install_resource_provider()` 持有静态
`XKernelResourceProvider` 实例，并安装到 `device_res` 的 MMIO、DMA 和 IRQ provider
registry。驱动资源申请仍通过 `device_res` 或 `kdriver::resource` 的兼容 wrapper
进入 provider contract。

## IRQ 适配

`XKernelResourceProvider` 不维护本地 IRQ line state：

- `request_irq()` 调用 `kirq::try_register_shared()`，返回
  `device_res::IrqHandlerToken::SharedAction(id)`。
- `release_irq()` 对 shared token 调用 `kirq::try_free_irq_action()`；如果后续 provider
  返回 non-shared regular token，则通过 `kirq::try_free_irq()` 释放整条 action。
- `request_threaded_irq()` / `request_threaded_irq_default()` 由 `device_res::IrqOp`
  默认实现返回 `ResError::Unsupported`。该抽象已预留给后续 kirq threadirq 接入，但
  本拆分分支不提前引入 IRQ core threaded 语义。

在当前 provider 中，`device_res::IrqEvent::WAKE_THREAD` 和
`wake_thread_from_sources()` 只会在 hardirq shared handler 路径上按 handled/source
bitmap 转换；真正的 wake-thread 语义等 kirq threadirq provider override 接入后生效。

## Driver-Facing API

本 crate 不提供 `devm_*` helper。驱动侧资源申请 API 由 OS-neutral `device_res`
持有；`kdriver::resource` 只为了保留 driver orchestration crate 的
`DriverResult` 语义提供薄 wrapper，并直接调用 `device_res::devm_*`。这样
`device-res-xkernel` 只承担 host provider 实现，不形成绕过 provider contract 的
平行 API。

## 所有权边界

- `device-res` 定义驱动需要的资源能力，不依赖 X-Kernel 内核子系统。
- `device-res-xkernel` 实现这些能力并依赖 `kirq`、`memspace`、`kdma`，但不提供
  driver-facing helper，也不决定 provider 安装时机。
- `kdriver` 负责设备发现、匹配、probe/remove、provider 安装和 `DriverResult`
  wrapper，不实现 provider trait。
