# Driver Subsystem Boundary Rules

This document defines the hard architectural boundary for driver work.

## Model

The driver subsystem has one device model (`driver-core`) and layered
interfaces:

- **Device model**: `driver-core` owns the bus/device/driver contracts, id
  tables, deferred probe, and the device domain. It is family-agnostic by
  construction — no device family is named in its API.
- **Kernel capability contracts**: interfaces that drivers require from a
  host kernel, such as MMIO mapping, IRQ registration, DMA, time, and clocks
  (the provider contracts in `driver-core::provider`), exposed to drivers
  through `DeviceCtx` and bound to the host by `driver-runtime`.
- **Device class subsystems**: per-class APIs and services (`subsystems/`)
  that drivers publish into via `kclass` detached parents.
- **Extension services**: family-specific service capabilities (the virtio
  transport probe) are extension traits recovered from the runtime — the
  core never names the family.

Concrete drivers must be written against these surfaces. A driver touches
`DeviceCtx` for generic resources, its class crate for publication, and its
family's extension service for family-specific probing.

```text
drv/<family> (one module per driver)
        │
        ├── dev.iomap()/request_irq()/clk() …     (DeviceCtx — generic resources)
        ├── subsystems/<class> traits + publish    (class publication)
        └── extension::<FamilyService>()           (family-specific probing)

core/driver-runtime (the host bridge)
        ├── implements DeviceServices over its resource provider
        └── provides family extension services (typed, compile-checked)
```

## Hard Rules

- `driver-core` must not name any device family, depend on device stacks,
  or gain family-specific methods. Adding a driver never touches the core.
- `common/` crates (`driver_base`, `devnode`, `kclass`, `hwrng`) stay
  host-kernel-free.
- Concrete driver code must not call host-kernel APIs (`khal`, `kirq`,
  `memspace`, `kdma`, `kruntime`, `ktask`, `kwork`) directly; the two
  documented exceptions are subsystem-owned services (`subsystems/block`
  dispatch) and constructors requiring `&'static` provider traits from the
  vendored virtio layer.
- Host-specific code lives in the `driver-runtime` provider (capabilities
  -> framework: the only place the model touches `memspace`/`kirq`/`kdma`)
  and `integration/` (framework -> kernel facilities); the direction rule
  is fixed.
- Kernel subsystems (`kirq`, `memspace`, `kdma`) must not depend on
  driver-subsystem crates.
- New driver needs become capability contracts or subsystem services first.
  Never make a concrete driver call a host API as a shortcut.

## Crate Layers

See `drivers/README.md` for the current inventory.

- **`core/`**: `driver-core` (model), `driver-runtime` (services +
  extensions), `driver-compose` (composition root, called from `kruntime`).
- **`common/`**: cross-subsystem foundations only.
- **`subsystems/`**: per-class APIs and class services (`block` owns the
  Gendisk model and dispatch services — kernel class services by design).
- **`drv/`**: concrete drivers, one subsystem crate per family, one module
  per driver; shared family construction libraries live beside them
  (`drv/virtio/device`).
- **`bus/`, `firmware/`**: PCI access, DT/ACPI parsing.
- **`adapters/`, `integration/`**: the two host-bridge directions.

## Review Checklist

- Does `driver-core` gain any family knowledge? (grep `virtio|nvme|…` in
  `core/driver-core` — must stay empty.)
- Does a `common/` crate gain a host-kernel dependency?
- Does a new device family require editing `driver-core` instead of adding
  an extension trait?
- Are resources acquired via `DeviceCtx` (or a documented subsystem
  service), never by reaching around to providers?
- Is the teardown order preserved: quiesce -> withdraw publication ->
  release IRQ -> drop transport -> unmap?
