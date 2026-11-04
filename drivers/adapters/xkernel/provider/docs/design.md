# driver-os-api-xkernel — Design

Kernel unit tests live in `ut-tests/` and are mounted from their original
source modules with `unittest::path_mod_test!` under `cfg(unittest)`.

## Purpose

The public types are `XKernelResourceProvider` and `NativeIrq`. The latter
implements os-api IRQ operations; `irq.rs` temporarily translates the legacy
IRQ contract into the same registration/release backend in `native_irq.rs`.
`msi.rs` owns the shared MSI allocation/release implementation. Neither entry
owns device identity, cleanup registration or service composition. This crate
has no dependency on driver-model or kdriver.

Shared registration now uses the native typed error classification, including
BadState for invalid context and Busy for teardown. Exact-token release must
return a handler after drain: no match is NotFound, not successful release.
The legacy infallible release fails closed on rejection rather than permitting
resource reclamation. Legacy resource shapes and callback contracts have not
yet been unified. Unit-returning MSI release still warns on refusal; this
slice does not resolve its ownership/retry limitations.

CPU-only storage, MMIO, raw DMA, IRQ and time implement driver-os-api contracts.
The legacy IRQ adapter remains during PR-01's staged migration; there is only
one CPU allocator provider implementation.

IRQ reverse-map owner preparation errors map from `IrqDescError::NoMemory` to
`ResError::NoMemory`; invalid hardware-number bounds map to InvalidResource.
The consuming unit-returning MSI release interface remains unchanged because
KIRQ now removes production MSI reverse lookup without allocating an owner.

`driver-os-api-xkernel` is X-Kernel's implementation layer for OS-neutral
driver capabilities. It is not responsible for driver matching,
probe/remove orchestration, device publication, or the driver-facing
`devm_*` API; those belong to `kdriver` and `device_res` respectively.

The crate's job is to wire the resource capabilities the driver subsystem
needs onto the X-Kernel kernel subsystems:

- `driver_os_api::MmioOp` -> `memspace::iomap_device` / `memspace::iounmap`
- `driver_os_api::DmaOp` -> the `kdma` coherent / streaming DMA API
- `driver_os_api::CpuMemoryOp` -> ordinary fallible `kalloc` allocation
- `device_res::IrqOp` -> the `kirq` shared hardirq and MSI-X API
- `driver_os_api::TimeOp` -> `khal::time::monotonic_time`

## Architecture

```text
driver crates / kdriver
        │
        │ OS-neutral resource API
        ▼
 driver-os-api / legacy device-res IRQ adapter
        │
        │ provider traits
        ▼
  driver-os-api-xkernel
        │
        ├── memspace
        ├── kirq
        └── kdma
```

Sources are split along the provider-trait boundaries:

```text
drivers/adapters/xkernel/provider/src/
├── lib.rs   # the XKernelResourceProvider type and module boundaries
├── mmio.rs  # MmioOp -> memspace
├── dma.rs   # DmaOp -> kdma
├── memory.rs # CpuMemoryOp -> ordinary kalloc storage
├── irq.rs   # legacy IRQ vocabulary adapter
├── native_irq.rs # os-api IRQ, shared registration/drain backend
├── msi.rs   # shared MSI reservations
└── time.rs  # TimeOp -> khal::time
```

`XKernelResourceProvider` implements
the provider traits required by `device_res`, but it does not own the
driver framework's provider-selection policy. `kdriver::resource` holds a
static `XKernelResourceProvider` instance and passes it explicitly to
`device_res::devm_*_with_provider()` inside the driver-facing
`DeviceResourceExt` methods.

## IRQ Adaptation

IRQ trigger/controller/domain values, route descriptions, action tokens and
MSI messages use os-api's shared vocabulary; no per-variant conversion remains.
The legacy resource structure and callback protocol are still distinct.

Legacy mapping now honors an explicit domain unchanged. Only absent domains
use the historical GIC/PLIC/IOAPIC root fallback; unknown/LoongArch/MSI families
without a domain still leave validation to kirq. The returned resource retains
the selected domain and actual mapped virq, so request, enable and release
reconstruct the same descriptor. A rejected explicit domain is never retried
against a root. A resource with no route metadata still remains PlainVirq.
Mapping does not allocate or register domains on the caller's behalf.

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
keeps `driver-os-api-xkernel` a pure host provider implementation, with no
parallel API that bypasses the provider contract.

## Ownership Boundaries

- `device-res` defines the resource capabilities drivers need and does not
  depend on X-Kernel kernel subsystems.
- `driver-os-api-xkernel` implements those capabilities and depends on
  `kirq`, `memspace`, `kdma`, and `kalloc`, but provides no driver-facing helpers and
  does not decide when the provider is installed.
- `kdriver` owns device discovery, matching, probe/remove, provider
  custody, and the `DriverResult` wrapper; it does not implement provider
  traits.

## Fallible ordinary CPU memory

The `CpuMemoryOp` adapter calls the inherent
`kalloc::global_allocator().alloc(Layout)` method and propagates exhaustion as
`ResError::NoMemory`. It deliberately bypasses the `GlobalAlloc` implementation,
whose OOM policy is fatal. Matching release uses the same allocator, pointer
and layout. This host adapter is the layer allowed to depend on `kalloc`;
portable device contracts retain only `Layout` and `CpuMemoryOp`.

No `kdma` call, page-attribute conversion or device-sharing hook occurs for
CPU storage. The existing coherent/streaming DMA adapters are unchanged.
