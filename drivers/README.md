# Driver Crate Boundaries

The driver subsystem is split by role. Keep new code in the layer that owns
the contract it is implementing or consuming.

Cross-host portability is **not** a design goal — there is one host. The
layering below exists for four reasons that are still wanted: the model must
run on mock services, adding a device family must not edit the shared core,
resources must have exactly one acquisition path so RAII rollback holds, and
the call surface must look like Linux's so a driver transcribes mechanically.
The normative rules are in
[`docs/ai/driver-subsystem-boundaries.md`](../docs/ai/driver-subsystem-boundaries.md).

## Layers

```text
                base/  the device model: device, driver, bus, class, domain, devres
               ┌──────────────────────────────────────┐
               │ driver-base / driver-host            │
               │ driver-init (initcall levels)        │
               └──────────────────┬───────────────────┘
                                  │
      ┌───────────────┬───────────┴───────────┬─────────────────┐
      ▼               ▼                       ▼                 ▼
   common/        subsystems/              drv/             bus/ firmware/
   (devnode:      (per-class stack:        (one module      (PCI access,
    node + uevent  traits + class API       per driver,      DT/ACPI/FDT,
    projection)    + dispatch, e.g. block)  family crates)   DICE handover)
         │                │                    │
         │                └────► driver-host provider ◄────────┘
         │                       (capability backend bridge:
         │                        memspace / kirq / kdma / khal)
         │                                │
         └────► driver-init (composition root, initcall levels)
```

One device object, one class story per subsystem:
`driver-base` owns the model and the class machinery (registry, drain,
`class_api!`); each `subsystems/*` crate owns its whole class stack —
trait, `publish_*`/`subscribe_*` API, hooks (`ClassHooks`), node
projection. There is no separate class crate. Consumers import family
items from their subsystem and model types from `driver-base`.

Serial: `drv/serial/serial-core` is the UART family library (hardware,
early-port handoff); `console/` is the printk-side consumer.
`clocksource/`, `irqchip/`, `rtc/`, `perf/` are kernel-integrated
drivers outside the model; `x86-apic` lives in `arch/`.
Host-side trees — `core/driver-host`, `core/driver-compose`, `bus/`,
`firmware/`, `platform/`, `integration/` — may use host-kernel crates freely;
that is their role. The L0/L1 rules apply to `core/driver-core`, `common/`,
`subsystems/` and `drv/`.

## Device Model and Host Bridge (`core/`)

| Crate | Package | Role |
|------|------|------|
| `base` | `driver-base` | The device model: bus/device/driver contracts, const id tables, deferred probe, the device domain, extension services, the devres front/back axis, and the class machinery (registry, drain, `class_api!`). Family-agnostic by construction (rule L0-a). |
| `host` | `driver-host` | Per-device services (`DeviceServices`), the resource provider binding them to `memspace`/`kirq`/`kdma`/`khal` (formerly the `device-res` stack), the virtio transport extension and HAL, and the clock registry. |
| `init` | `driver-init` | Composition root with explicit initcall levels: L1 host services → L2 subsystem cores → L3 drivers → L4 enumeration → L5 probe storm. Called once from `kruntime`. |

## Cross-Subsystem Foundations (`common/`)

Only crates that several subsystems depend on by design — not a dumping
ground. Anything class-specific belongs in `subsystems/`. All three are
host-kernel-free (rule L0-b), which is what lets the model be tested against
`MockServices`.

| Crate | Package | Role |
|------|------|------|
| `common/devnode` | `devnode` | Model-agnostic userland node projection (devtmpfs/sysfs/uevent): data-driven `DevNodeType`, `DeviceNumber`, and the udev-visible uevent payload format. |
| `subsystems/*` | `subsys-*` | Per-class stack: trait + class API (publish/find/subscribe) + hooks + node projection; `drv/serial/serial-core` is the UART family library. |

## Per-Class Subsystems (`subsystems/`)

Contracts (APIs and published types) and services for one device class.

`subsystems/block` owns the Gendisk model plus the completion waiting, the
softirq reclaimer and the IRQ wiring under `dispatch/`. That split is the
L0-b rule in practice: `BlockCompletionOperations` / `BlockWaiter` /
`BlockSignals` are host-free contracts, while `dispatch::{wait,reclaimer}` are
host services and legitimately use `kirq`/`ktask`/`kpoll`. The
`contracts`-era pretense that the whole crate was a pure interface is gone with
the directory name.

| Crate | Package |
|------|------|
| `subsystems/block` | `block` |
| `subsystems/char` | `char_driver` |
| `subsystems/display` | `display` |
| `subsystems/hwrng` | `hwrng` |
| `subsystems/input` | `input` |
| `subsystems/net` | `net` |
| `subsystems/vsock` | `vsock` |

## Concrete Drivers (`drv/`)

One subsystem crate per family, one module per driver, one feature per driver —
mirroring Linux's file-per-driver model. Adding a driver touches: a module
file, one feature line in its subsystem crate, one `driver-compose`
registration, and a Kconfig block.

| Crate | Package | Modules |
|-------|---------|---------|
| `drv/virtio` | `drv-virtio` | `blk`, `net`, `rng`, `gpu`, `input`, `vsock`, `virtio_9p`, plus the `mmio` transport driver. |
| `drv/virtio/devices` | `virtio-devices` | Shared virtio device-family constructors (`VirtIoBlkDev`, …) over the vendored `virtio-drivers` transport, implementing the class traits. A separate package because the HAL and class layers share it. |
| `drv/serial` | `drv-serial` | `pl011`, `ns16550` (MMIO + I/O-port). |
| `drv/clk` | `drv-clk` | `fixed_clock` (DT `fixed-clock` provider). |
| `drv/block` | `drv-block` | `ramdisk`. |

Drivers acquire resources through `DeviceCtx` only (rule L1). The registered
host-dependency exceptions are E-01 … E-04 in
`docs/ai/driver-subsystem-boundaries.md`; do not add a new one without an
entry there.

## Bus and Firmware (`bus/`, `firmware/`)

| Crate | Package | Role |
|------|------|------|
| `bus/pci` | `pci` | CAM/BAR configuration and access, PCI discovery into the device domain. First-class `driver-core` bus (`PciBus`, `PciMatch`). |
| `firmware/of` | `of` | Device-tree node discovery: `reg`/`reg-names`, `interrupts`/`interrupt-names`, `clocks`/`clock-names`, provider attributes. |
| `firmware/acpi` | `acpi` | ACPI table parsing (MCFG, MADT, DSDT `_CRS`). |
| `firmware/rs-fdtree` | `rs_fdtree` | The vendored FDT reader underneath `of`. |

## Platform Drivers and Boot Integration (`platform/`)

Hardware controller drivers and boot integration. Their long-term home is
`drv/<family>` (console likewise landing beside the char subsystem), migrated
one-by-one when touched.

| Crate | Package |
|------|------|
| `platform/console` | `console-driver` |
| `platform/irqchip` | `irqchip-driver` |
| `platform/timer` | `timer-driver` |
| `platform/rtc` | `rtc-driver` |
| `platform/x86-apic` | `x86-apic` |
| `platform/aarch64-pmuv3` | `aarch64-pmuv3` |
| `platform/dice` | `dice-driver` |

`platform/irqchip` sits **below** `kirq`: irqchip drivers implement concrete
controller behavior (configure, mask/unmask, claim, complete, priority, IPI
delivery) through `kirq::IntrManagerIf`. Generic descriptor, mapping,
registration, dispatch, softirq and threaded-IRQ semantics belong in `kirq`.

## Integration (`integration/`)

| Crate | Package | Role |
|------|------|------|
| `integration/uevent-netlink` | `uevent-netlink` | Netlink transport for kobject uevents — the one drivers-tree consumer of `knet`. The payload format (a udev-visible compatibility surface) lives in `devnode::uevent`, shared with sysfs attributes. |

## Porting Workflow

See [`docs/linux-porting-map.md`](../docs/linux-porting-map.md) — the normative
Linux → x-kernel mapping for AI-assisted driver transcription, with the golden
templates under `drv/`.
