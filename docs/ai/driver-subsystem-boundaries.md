# Driver Subsystem Boundary Rules

This document defines the architectural boundary for driver work. It is a
hard constraint: read it before adding a driver, a subsystem service, or a
capability contract.

## What this document is NOT about

Cross-host portability is **not** a goal. There is exactly one host
(X-Kernel), and no rule here exists to make a driver compile against another
kernel. Every rule below earns its place through one of four properties that
are still wanted:

| Property | Why it matters | Which rule carries it |
|---|---|---|
| Testability | the model must run on mock services, or its state machine is unverifiable | L0-b |
| Core stability (fan-in) | adding a device family must not edit the crate every driver shares | L0-a |
| Rollback invariant | "guards live in `Data`, probe failure = drop `Data`" only holds if resources have ONE acquisition path | L1 |
| Transcription fidelity | a Linux driver transcribes mechanically only if the framework's call surface looks like Linux's | L1, L0-a |

If a proposed change weakens one of those four, it needs an argument. If it
only weakens "portability", it does not.

## Model

One device model (`driver-core`) plus layered interfaces:

- **Device model** — `core/driver-core`: bus/device/driver contracts, const
  id tables, deferred probe, the device domain. Family-agnostic by
  construction: no device family is named in its API.
- **Kernel capability contracts** — `driver-core::provider` (MMIO/IRQ/DMA
  ops) and the per-device service traits (`DeviceResourceServices`,
  `ClockConsumerServices`, `DeviceServices`). Drivers reach them only through
  `DeviceCtx`; `core/driver-host` binds them to `memspace`/`kirq`/`kdma`/`khal`.
- **Device class subsystems** — `subsystems/`: per-class APIs and services.
  Drivers publish into them through `device_class` detached parents.
- **Extension services** — family-specific capabilities (the virtio transport
  probe) are extension traits recovered from the runtime. The core never names
  the family.

```text
drv/<family> (one module per driver)
        │
        ├── dev.iomap()/request_irq()/clk() …     (DeviceCtx — generic resources)
        ├── subsystems/<class> traits + publish    (class publication)
        └── extension::<FamilyService>()           (family-specific probing)

core/driver-host (the host bridge)
        ├── implements DeviceServices over its resource provider
        └── provides family extension services (typed, compile-checked)
```

## Scope: which trees the rules apply to

| Tree | Host-side by definition? | Rules |
|---|---|---|
| `core/driver-core` | no — must stay host-agnostic AND family-agnostic | L0-a, L0-b |
| `common/` (`devnode`) | no | L0-b, L1 |
| `subsystems/` **contracts** (traits, published types) | no | L0-b |
| `subsystems/` **services** (e.g. `block::dispatch`) | yes — see L0-b | L1 |
| `drv/` (concrete drivers + family libraries) | no | L1, L2 |
| `core/driver-host`, `core/driver-compose` | **yes** | none of L0/L1 |
| `bus/`, `firmware/`, `platform/`, `integration/` | **yes** | none of L0/L1 |

`bus/pci` maps the ECAM window through `khal`/`memspace`; `platform/*` are
hardware controller drivers and boot integration; `integration/uevent-netlink`
is the one consumer of `knet`. These are host-side by role, not by accident —
the old text listed them as violations, which trained reviewers to ignore the
whole document.

## Hard Rules

### L0-a — `driver-core` names no device family

`driver-core` must not name a device family, depend on a device stack, or gain
a family-specific method. Adding a driver never touches the core.

This is a **fan-in** rule, not a portability rule: `driver-core` is ~4.5k lines
with the model's whole test suite behind it. Every family-specific method added
there is a change that risks every already-transcribed driver.

Check: `grep -rniE 'virtio|nvme|ahci|sdhci|mmc|i2c|usb|pl011|ns16550'
drivers/base/src` must return comment lines only. `spi` is not in
the mechanical pattern because it collides with `spin`/`kspin`/`SpinNoPreempt`;
an SPI bus added to the core is caught by the review checklist below instead.

`scripts/ci/check_driver_boundaries.sh` runs this (and L0-b, L1 and the
register's integrity) mechanically; it is wired into `make clippy` via the
`check_boundaries` target, so it also gates the pre-commit hook.

### L0-b — contracts are host-free; services may use the host

The distinction that makes the model testable:

| | must be host-free | may use `kirq`/`ktask`/`kpoll`/… |
|---|---|---|
| `driver-core::DeviceServices` and friends | ✅ (this is why `MockServices` exists) | — |
| `subsystems/block` `BlockCompletionOperations`, `BlockWaiter`, `BlockSignals` | ✅ | — |
| `subsystems/block::dispatch::{wait,reclaimer}` | — | ✅ |
| `common/{devnode}` | ✅ | — |

A **contract** is a trait a driver implements or a type it publishes. A
**service** is an implementation the subsystem owns and runs. Contracts stay
host-free so the model and the class layer can be exercised on mocks;
services are allowed to be host code because that is their job.

`subsystems/block::dispatch` using `kirq::softirq` and
`ktask::future::PreparedTaskWait` is therefore **not** an exception — it is an
instance of this rule.

### L1 — resources have exactly one acquisition path: `DeviceCtx`

Concrete driver code (`drv/**`) acquires MMIO, IRQ, DMA and clocks through
`DeviceCtx` (or through a documented subsystem service), never by reaching
around to a provider.

Why this is load-bearing rather than stylistic: the framework's rollback
guarantee is *"store every guard in `DriverOps::Data`; probe failure = drop
`Data`, which releases guards in field order"*. That guarantee is only true if
there is one door. A second acquisition path creates a resource whose lifetime
the `Data` drop order does not describe — i.e. a hole exactly where RAII was
supposed to be the invariant.

Mechanically checkable half of L1: **no `drv/**`, `subsystems/**` or
`common/**` crate may name the host runtime or a provider singleton**
(`driver_host::resource_provider`, `XKernelResourceProvider`, `XKernelRuntime`,
`GLOBAL_RUNTIME`). A provider may be *injected* into a family library; it may
never be *fetched*. See exception E-01 for the one injected-provider site.

### L2 — exceptions are registered, not prose

An exception is allowed, but it must appear in the register below with a
reason and an exit condition. An unregistered host dependency in `drv/**` is a
defect; a registered one is a decision.

`scripts/ci/check_driver_boundaries.sh` enforces L0-a, L0-b, L1 and this
register: the allow-lists live in the script as data, each entry carries the
E-0x id it is registered under, and the script fails if an id is missing from
this document **or** if an allow-listed path no longer needs its exception. A
register that cannot go stale is the only kind worth having. The script is
wired into `make clippy` (`check_boundaries`), so it gates the pre-commit hook
too.

### Direction rules (unchanged, still hard)

- Kernel subsystems (`kirq`, `memspace`, `kdma`, `ktask`) must not depend on
  driver-subsystem crates.
- Host-specific code lives in `core/driver-host` (capabilities → framework:
  the only place the model touches `memspace`/`kirq`/`kdma`) and
  `integration/` (framework → kernel facilities). The direction is fixed.
- New driver needs become capability contracts or subsystem services first.
  Never make a concrete driver call a host API as a shortcut.

### `kirq` is common vocabulary, not a host implementation

`AGENTS.md` treats `kirq` as X-Kernel's common interrupt core, and
`driver-core` deliberately re-exports its IRQ vocabulary
(`driver-core/src/resource.rs`: `pub use kirq::{IrqController, IrqDomainId,
IrqTrigger}`) so the trigger/controller/domain shapes exist exactly once in
the tree. Therefore:

- **Allowed anywhere**: `kirq`'s *type vocabulary* — `IrqTrigger`,
  `IrqController`, `IrqDomainId`, and `driver_core::IrqAction`.
- **Not allowed in `drv/**`**: `kirq`'s *registration API* —
  `try_register_shared`, `try_free_irq`, `enable`, `try_map`, `alloc_msix`.
  Registration goes through `DeviceCtx::request_irq` /
  `DeviceCtx::resolve_irq`, which return an owned `DevIrq` guard that
  participates in the `Data` drop order.

The previous version of this document listed `kirq` flatly in the forbidden
set while `driver-core` depended on it. That contradiction is the reason the
rule drifted; it is resolved here in favour of the dedup, not in favour of
duplicating the vocabulary.

## Exception Register

| ID | Site | Host surface used | Why the contract is not enough | Exit condition |
|---|---|---|---|---|
| E-01 | `drv/virtio/devices/src/pci{,/x86_64}.rs` | `driver_core::provider::{IrqOp, MsiResource}` (injected by `driver-host`) | The MSI-X **config vector belongs to the transport**, not to a device driver's `Data` — Linux keeps it in `virtio_pci_modern.c` for the same reason. It is driven from inside the vendored `Transport` impl (`max_queue_size` programs the queue vector, `set_status` programs the config vector and activates), so no driver-side `DeviceCtx` call can own it. Lifetime is transport-scoped: allocated in `setup_msix`, released on transport drop. | Move together with per-vector MSI-X exposure (tracked in `docs/driver-framework-review.md` §8.3 class A). Not before: there is no multi-queue consumer yet. |
| E-02 | `drv/virtio/**`, `drv/clk` | `driver_host::{virtio_hal::Hal, virtio_transport::PciTransportService, register_clk_provider}` | Two independent causes. (a) `ExtensionService::from_services` is an `as_any` downcast, so the caller **must** name the concrete host type — that is what keeps `driver-core` family-agnostic (L0-a). (b) The vendored `virtio-drivers` requires `VirtIoHal: 'static` as a *type parameter*, so a driver constructing `VirtIoBlkDev<H, T>` must name a concrete `H`. (c) A clock provider driver's job *is* registering into the host clock subsystem (`of_clk_add_hw_provider` does the same in Linux). | (a) would need a TypeId-keyed extension registry with the contract trait living host-free — a `driver-core` public-API redesign whose only remaining payoff was portability. **Deliberately not pursued.** (b) needs an upstream change to the vendored crate. (c) none. |
| E-03 | `drv/serial/{pl011,ns16550}.rs` | `console_driver::{runtime, serial}` | Early-console adoption and the stdout identity handoff are single-machine boot integration, not a device resource. There is no second host for a shim to be portable to. | None planned. The real risk here is ownership, not layering — see the stdout handoff protocol in `drivers/drv/serial/docs/design.md`. |
| E-04 | `drv/virtio/src/rng.rs` | `ktask::future::PreparedTaskWait`, `kpoll::PollSet` | The hwrng completion wait needs a task-context park/wake. `subsystems/block::dispatch::wait` already wraps the identical primitive as a service, but hwrng has exactly **one** consumer today. | Fold into a `subsystems/hwrng` dispatch service when a second hwrng driver appears. Until then the execution-context invariant is documented at `HostRngCompletion`. |

Not exceptions (they follow from scope or L0-b): `subsystems/block::dispatch`
(service, host by design); `bus/pci`, `firmware/*`, `platform/*`,
`core/driver-host`, `core/driver-compose`, `integration/*` (host-side trees).

## Crate Layers

See `drivers/README.md` for the current inventory.

- **`core/`** — `driver-core` (model), `driver-host` (services + extensions +
  the resource provider), `driver-compose` (composition root, called once from
  `kruntime`).
- **`common/`** — cross-subsystem foundations only: `devnode`
  (userland node projection + node classes).
- **`subsystems/`** — per-class APIs and class services: `block`, `char`,
  `display`, `hwrng`, `input`, `net`, `vsock`.
- **`drv/`** — concrete drivers, one subsystem crate per family, one module
  per driver; shared family construction libraries live beside them
  (`drv/virtio/devices`, package `virtio-devices`).
- **`bus/`, `firmware/`** — PCI access and DT/ACPI parsing.
- **`integration/`** — framework → kernel facilities (`uevent-netlink`).

## Review Checklist

- Does `driver-core` gain any family knowledge? (L0-a grep must stay
  comment-only.)
- Does a `common/` crate, or a subsystem **contract**, gain a host-kernel
  dependency? (L0-b.)
- Does a new device family require editing `driver-core` instead of adding an
  extension trait? (L0-a.)
- Does driver code acquire a resource anywhere other than `DeviceCtx` or a
  documented subsystem service? (L1.)
- Does a `drv/**`, `subsystems/**` or `common/**` crate name the host runtime
  or a provider singleton — i.e. *fetch* rather than *receive* a provider?
  (L1.) If yes, is it registered? (L2.)
- Does a `drv/**` crate call a `kirq` **registration** API rather than
  `DeviceCtx::request_irq` / `resolve_irq`?
- Is a new host dependency in `drv/**` added to the exception register with a
  reason and an exit condition?
- Is the teardown order preserved: quiesce → withdraw publication → release
  IRQ → drop transport → unmap?
