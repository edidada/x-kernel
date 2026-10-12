# device-res-xkernel — Design

## Purpose

`device-res-xkernel` is X-Kernel's implementation layer for the OS-neutral
`device-res` provider contract. It is not responsible for driver matching,
probe/remove orchestration, device publication, or the driver-facing
`devm_*` API; those belong to `kdriver` and `device_res` respectively.

The crate's job is to wire the resource capabilities the driver subsystem
needs onto the X-Kernel kernel subsystems:

- `device_res::MmioOp` -> `memspace::iomap_device` / `memspace::iounmap`
- `device_res::DmaOp` -> the `kdma` coherent / streaming DMA API
- `device_res::IrqOp` -> the `kirq` shared hardirq and MSI-X API
- `device_res::TimeOp` -> `khal::time::monotonic_time`

## Architecture

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

Sources are split along the provider-trait boundaries:

```text
drivers/adapters/xkernel/device-res/src/
├── lib.rs   # the XKernelResourceProvider type and module boundaries
├── mmio.rs  # MmioOp -> memspace
├── dma.rs   # DmaOp -> kdma
├── irq.rs   # IrqOp -> kirq
└── time.rs  # TimeOp -> khal::time
```

`XKernelResourceProvider` supplies platform MMIO, IRQ, time and compatibility
DMA operations. `XKernelDmaProvider` owns a distinct device DMA context selected
by host integration from streaming and coherent masks. `kdriver::resource` holds a
static `XKernelResourceProvider` instance and passes it explicitly to
`device_res::devm_*_with_provider()` inside the driver-facing
`DeviceResourceExt` methods.

## IRQ Adaptation

`XKernelResourceProvider` keeps no local IRQ line state:

- `request_irq()` calls `kirq::try_register_shared()` and returns
  `device_res::IrqHandlerToken::SharedAction(id)`.
- `release_irq()` calls `kirq::try_free_irq_action()` for shared tokens;
  if a later provider returns a non-shared regular token, the whole action
  is freed through `kirq::try_free_irq()`.
- `request_threaded_irq()` / `request_threaded_irq_default()` return
  `ResError::Unsupported` from the `device_res::IrqOp` default
  implementations.
- MSI-X is implemented on the x86_64 backend only; other architectures
  return `ResError::Unsupported` (see the Known Limitations section of
  `docs/security.md`). The abstraction is reserved for a future kirq
  threaded-IRQ integration; this branch does not introduce IRQ-core
  threaded semantics ahead of that.

In the current provider, `device_res::IrqEvent::WAKE_THREAD` and
`wake_thread_from_sources()` only translate the handled/source bitmap on
the hardirq shared-handler path; real wake-thread semantics take effect
once a kirq threadirq provider override is integrated.

## Driver-Facing API

This crate provides no `devm_*` helpers. Driver-side resource acquisition
is exposed by `kdriver::resource` as `DeviceResourceExt` — a driver calls
`device.devm_iomap(...)`, for example. `kdriver::resource` is responsible
for passing `XKernelResourceProvider` explicitly into the OS-neutral
`device_res` helpers and for mapping `ResError` onto `DriverError`. This
keeps `device-res-xkernel` a pure host provider implementation, with no
parallel API that bypasses the provider contract.

## Ownership Boundaries

- `device-res` defines the resource capabilities drivers need and does not
  depend on X-Kernel kernel subsystems.
- `device-res-xkernel` implements those capabilities and depends on
  `kirq`, `memspace`, and `kdma`, but provides no driver-facing helpers and
  does not decide when the provider is installed.
- `kdriver` owns device discovery, matching, probe/remove, provider
  custody, and the `DriverResult` wrapper; it does not implement provider
  traits.

## Device DMA providers

`XKernelDmaProvider::new` validates streaming and coherent address masks.
Each provider owns immutable device policy and uses kdma's shared platform
backend and bounce pool. The default preserves unrestricted addressing;
restricted devices declare masks explicitly. The device and resources retain
`Arc<dyn DmaOp>` so allocation, release and synchronization use the original
provider. Out-of-mask candidates return an error; this backend has no
low-address allocation guarantee.

The zero-sized `XKernelResourceProvider` delegates legacy DMA calls to a static
platform compatibility context. The bound VirtIO path uses the per-device
provider for queue pages, GPU pages, buffers and indirect descriptors. Address
zero is a valid successful provider result; fallibility is represented by Result.
Streaming sync performs bounce copies through kdma instead of inherited no-ops.
`InvalidInput` maps to `InvalidResource`; allocation/hook exhaustion maps to
`NoMemory`. Cache policy remains uncached/shared for all devices.

The `dma-trace` feature forwards coherent diagnostics to kdma. Integration
selects this adapter feature without a direct dependency on the DMA core.
