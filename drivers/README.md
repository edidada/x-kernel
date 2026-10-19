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
early-port handoff); `console/` is the printk-side consumer that must come
up before the model and later publishes into the char class. `rtc/` was the
first kernel-integrated driver migrated into the model: the class lives in
`subsystems/rtc`, the backends in `drv/rtc`. `clocksource/`, `irqchip/` and
`perf/` remain outside the model by boot order — see
[Kernel-Integrated Drivers](#kernel-integrated-drivers-outside-the-model).
`x86-apic` lives in `arch/`.
Host-side trees — `drivers/host`, `drivers/init`, `bus/`, `firmware/`,
`integration/`, `console/`, `clocksource/`, `irqchip/`, `perf/` — may use
host-kernel crates freely; that is their role. The L0/L1 rules apply to
`drivers/base`, `common/`, `subsystems/` and `drv/`.

## Device Model and Host Bridge (`base/`, `host/`, `init/`)

| Crate | Package | Role |
|------|------|------|
| `base` | `driver-base` | The device model: bus/device/driver contracts, const id tables, deferred probe, the device domain, extension services, the devres front/back axis, and the class machinery (registry, drain, `class_api!`). Family-agnostic by construction (rule L0-a). |
| `host` | `driver-host` | Per-device services (`DeviceServices`), the resource provider binding them to `memspace`/`kirq`/`kdma`/`khal` (formerly the `device-res` stack), the virtio transport extension and HAL, and the clock registry. |
| `init` | `driver-init` | Composition root with explicit initcall levels: L1 host services → L2 subsystem cores → L3 drivers → L4 enumeration → L5 probe storm. Called once from `kruntime`. |

## Cross-Subsystem Foundations (`common/`)

Only crates that several subsystems depend on by design — not a dumping
ground. Anything class-specific belongs in `subsystems/`. `devnode` is
host-kernel-free (rule L0-b), which is what lets the model be tested against
`MockServices`.

| Crate | Package | Role |
|------|------|------|
| `common/devnode` | `devnode` | Model-agnostic userland node projection (devtmpfs/sysfs/uevent): data-driven `DevNodeType`, `DeviceNumber`, and the udev-visible uevent payload format. |

## Per-Class Subsystems (`subsystems/`)

Contracts (APIs and published types) and services for one device class.

`subsystems/block` owns the Gendisk model and the host-free contracts;
`subsystems/block-host` owns the completion waiting, the softirq reclaimer
and the IRQ wiring. That split is the L0-b rule in practice:
`BlockCompletionOperations` / `BlockWaiter` /
`BlockSignals` are host-free contracts, while `block-host`'s `wait` /
`reclaimer` are host services and legitimately use `kirq`/`ktask`/`kpoll`.

| Crate | Package |
|------|------|
| `subsystems/block` | `subsys-block` (Gendisk model + host-free contracts) |
| `subsystems/block-host` | `subsys-block-host` (wait / reclaimer services) |
| `subsystems/char` | `subsys-char` |
| `subsystems/display` | `subsys-display` |
| `subsystems/hwrng` | `subsys-hwrng` |
| `subsystems/input` | `subsys-input` |
| `subsystems/net` | `subsys-net` |
| `subsystems/rtc` | `subsys-rtc` |
| `subsystems/vsock` | `subsys-vsock` |

## Concrete Drivers (`drv/`)

One subsystem crate per family, one module per driver, one feature per driver —
mirroring Linux's file-per-driver model. Adding a driver touches: a module
file, one feature line in its subsystem crate, one `driver-init`
registration, and a Kconfig block.

| Crate | Package | Modules |
|-------|---------|---------|
| `drv/virtio` | `drv-virtio` | `blk`, `net`, `rng`, `gpu`, `input`, `vsock`, `virtio_9p`, plus the `mmio` transport driver. |
| `drv/virtio/devices` | `virtio-devices` | Shared virtio device-family constructors (`VirtIoBlkDev`, …) over the vendored `virtio-drivers` transport, implementing the class traits. A separate package because the HAL and class layers share it. |
| `drv/serial` | `drv-serial` | `pl011`, `ns16550` (MMIO + I/O-port). |
| `drv/clk` | `drv-clk` | `fixed_clock`, `fixed_factor` (DT clock providers). |
| `drv/block` | `drv-block` | `ramdisk`. |
| `drv/rtc` | `drv-rtc` | `pl031`, `goldfish`, `cmos`, `ls7a`; publishes into `subsys-rtc`'s class. |

Drivers acquire resources through `Device` only, and publish providers through
`Device::provide::<C>` (rule L1: two doors, no third). A driver registers itself
with one `driver_base::declare_driver!(...)` line. The registered
host-dependency exceptions are E-01 … E-04 in
`docs/ai/driver-subsystem-boundaries.md`; do not add a new one without an
entry there.

## Bus and Firmware (`bus/`, `firmware/`)

| Crate | Package | Role |
|------|------|------|
| `bus/pci` | `pci` | CAM/BAR configuration and access, PCI discovery into the device domain. First-class `driver-base` bus (`PciBus`, `PciMatch`). |
| `firmware/of` | `of` | Device-tree node discovery: `reg`/`reg-names`, `interrupts`/`interrupt-names`, `clocks`/`clock-names`, provider attributes. |
| `firmware/acpi` | `acpi` | ACPI table parsing (MCFG, MADT, DSDT `_CRS`). |
| `firmware/rs-fdtree` | `rs_fdtree` | The vendored FDT reader underneath `of`. |
| `firmware/dice` | `dice-driver` | DICE handover-data reader shared by the TEE and devfs layers (crate name kept for compatibility; it parses firmware-provided data, it does not drive a device). |

## Kernel-Integrated Drivers (outside the model)

This group is deliberately outside the L0/L1 rules: none of these crates
publishes through the device model, and each is boot-ordering-pinned or
arch/BSP glue. A move into `drv/<family>` + `subsystems/<class>` is possible
only when that pin is resolved — the RTC migration (see above) is the
template for a driver that could move.

| Directory | Package | Role | Why not in the model |
|------|------|------|------|
| `console/` | `console-driver` | Early-boot stdout adoption and the printk-side console path. | Runs in `early_driver_init` before the domain exists; later publishes into the char class. |
| `clocksource/` | `clocksource-driver` | Architected timers (ARM generic, SBI, LAPIC/TSC). | The scheduler and workqueues need timekeeping before `driver-init` runs. |
| `irqchip/` | `irqchip-driver` | Interrupt controllers (GIC v2/v3, RISC-V PLIC, x86 IO-APIC). | Registers into `kirq`, which `driver-base` sits **above**; IRQs are needed before the model's L1 host services. |
| `perf/` | `aarch64-pmuv3` | AArch64 PMUv3 counter driver. | Arch PMU consumed by `platforms/kplat-aarch64` peripherals; no perf class exists yet. |

`irqchip/` sits **below** `kirq`: irqchip drivers implement concrete
controller behavior (configure, mask/unmask, claim, complete, priority, IPI
delivery) through `kirq::IntrManagerIf`. Generic descriptor, mapping,
registration, dispatch, softirq and threaded-IRQ semantics belong in `kirq`.
`arch/x86-apic` (CPU interrupt infrastructure) is the one hardware driver
that lives in `arch/` rather than `drivers/`.

## Integration (`integration/`)

| Crate | Package | Role |
|------|------|------|
| `integration/uevent-netlink` | `uevent-netlink` | Netlink transport for kobject uevents — the one drivers-tree consumer of `knet`. The payload format (a udev-visible compatibility surface) lives in `devnode::uevent`, shared with sysfs attributes. |

## Porting Workflow

See [`docs/linux-porting-map.md`](../docs/linux-porting-map.md) — the normative
Linux → x-kernel mapping for AI-assisted driver transcription, with the golden
templates under `drv/`.
