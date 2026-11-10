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
   devnode/       subsystems/              drv/             bus/ firmware/
   (node model    (per-class stack:        (one module      (PCI access,
    + netlink     traits + class API       per driver,      DT/ACPI/FDT,
    sink)         + dispatch, e.g. block)  family crates)   DICE handover)
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
early-port handoff, and the firmware→port decode behind its `earlycon`
feature); `boot-console/` is the boot-stage printk sink and stdout
bring-up; `subsystems/char-console` owns the runtime console identity over
the char class once the model has published the port. `rtc/` was the
first kernel-integrated driver migrated into the model: the class lives in
`subsystems/rtc`, the backends in `drv/rtc`. `clocksource/`, `irqchip/` and
`perf/` remain outside the model by boot order — see
[Kernel-Integrated Drivers](#kernel-integrated-drivers-outside-the-model).
`x86-apic` lives in `arch/`.
Host-side trees — `drivers/host`, `drivers/init`, `bus/`, `firmware/`,
`uevent-netlink/`, `boot-console/`, `clocksource/`, `irqchip/`, `perf/` — may
use host-kernel crates freely; that is their role. The L0/L1 rules apply to
`drivers/base`, `devnode/`, `subsystems/` and `drv/`.

## Device Model and Host Bridge (`base/`, `host/`, `init/`)

| Crate | Package | Role |
|------|------|------|
| `base` | `driver-base` | The device model: bus/device/driver contracts, const id tables, deferred probe, the device domain, extension services, the devres front/back axis, and the class machinery (registry, drain, `class_api!`). Family-agnostic by construction (rule L0-a). |
| `host` | `driver-host` | Per-device services (`DeviceServices`), the resource provider binding them to `memspace`/`kirq`/`kdma`/`khal` (formerly the `device-res` stack), the virtio transport extension and HAL, and the clock registry. |
| `init` | `driver-init` | Composition root with explicit initcall levels: L1 host services → L2 subsystem cores → L3 drivers → L4 enumeration → L5 probe storm. Called once from `kruntime`. |

## Device-Node Projection (`devnode/`, `uevent-netlink/`)

The node half of the model, split in two because one half must stay free of
kernel facilities: `devnode` owns the registry, the node identity and the
udev-visible payload format; `uevent-netlink` is the only crate that carries
those payloads out to `knet`. Linux keeps the same split (`drivers/base`
node policy vs `lib/kobject_uevent.c`); the reasoning behind the placement of
these two crates is in
[the boundary document](../docs/ai/driver-subsystem-boundaries.md#crate-layers).

`devnode` names no device family and no bus: a class registers its node type
as **data** (`DevNodeType`), so adding a subsystem never edits this crate.
It is host-kernel-free (rule L0-b), which is what keeps `MockServices` able
to drive the projection, and that is the whole reason `uevent-netlink` is a
separate crate: `knet` is paid by the 71-line sink, not by the model.

| Crate | Package | Role |
|------|------|------|
| `devnode` | `devnode` | The userland node registry (devtmpfs/sysfs/uevent): data-driven `DevNodeType`, `DeviceNumber`, event dispatch order, and the udev-visible uevent payload format shared with sysfs attributes. |
| `uevent-netlink` | `uevent-netlink` | Netlink sink for kobject uevents: installs itself as the `devnode` lifecycle publisher at boot and broadcasts to `NETLINK_KOBJECT_UEVENT` group 1. The one drivers-tree consumer of `knet`. |

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
| `subsystems/char-console` | `subsys-char-console` (console service over the char class) |
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
| `boot-console/` | `boot-console` | The printk sink, firmware stdout bring-up and early console input. | Runs in `early_driver_init` before the domain exists; its model-side half (stdout publication identity) is `subsystems/char-console`. |
| `clocksource/` | `clocksource-driver` | Architected timers (ARM generic, SBI, LAPIC/TSC). | The scheduler and workqueues need timekeeping before `driver-init` runs. |
| `irqchip/` | `irqchip-driver` | Interrupt controllers (GIC v2/v3, RISC-V PLIC, x86 IO-APIC). | Registers into `kirq`, which `driver-base` sits **above**; IRQs are needed before the model's L1 host services. |
| `perf/` | `aarch64-pmuv3` | AArch64 PMUv3 counter driver. | Arch PMU consumed by `platforms/kplat-aarch64` peripherals; no perf class exists yet. |

`irqchip/` sits **below** `kirq`: irqchip drivers implement concrete
controller behavior (configure, mask/unmask, claim, complete, priority, IPI
delivery) through `kirq::IntrManagerIf`. Generic descriptor, mapping,
registration, dispatch, softirq and threaded-IRQ semantics belong in `kirq`.
`arch/x86-apic` (CPU interrupt infrastructure) is the one hardware driver
that lives in `arch/` rather than `drivers/`.

## Relationship Multiplicity

The model's unit of life is the **device**: drivers, classes and providers
multiply around it. This is the audit of which shapes are supported, where
the extension seam is, and which are deliberately closed. The named tests
are kernel unit tests in `driver-base` that pin the behavior.

| Relationship | Supported | Mechanism | Extension seam / limit |
|---|---|---|---|
| driver 1 : N devices | yes | one `DriverDesc`, `probe` once per device, per-binding `Data` (`one_driver_binds_many_devices`) | per-device state must live in `Data`, never in the driver object |
| driver N : 1 device | sequential attempts | `attempted` set + `MatchScore`: `Fatal` hands the device to the next candidate, first success binds (`fatal_failure_falls_through_to_next_candidate`) | no simultaneous two-driver binding (Linux parity). A driver that must split a device enumerates **child devices** (`DeviceDescriptor::with_parent`); there is no aux-bus helper |
| device : bus | 1 bus (+ optional `BusInstanceId`) | `DeviceDescriptor.bus` / `bus_instance` | a device cannot sit on two buses (Linux parity) |
| device 1 : N classes | yes | one `Device::publish` per class, publications share `Arc<DeviceCore>` | **liveness is per device, not per publication**: the first withdrawal flips the shared online flag, so sibling publications stop being available and only their own drop removes the registry entry (`one_class_serves_many_devices_and_publications_share_device_liveness`) |
| class 1 : N devices | yes | `ClassRegistry` vector + `devices()`/`find()` + availability/removal subscriptions | scans are O(N); callbacks are class-wide (consumers filter); no per-device subscription |
| driver 1 : N classes | yes | `Data` holds N `Publication`s; field order is release order | a driver must keep every publication in one `Data` so rollback stays field-order RAII |
| publication : devnode | 1 : 1 | devnode registry keyed by publication id; `publish_auto_indexed` allocates per-class indices | one publication cannot project two nodes; two userland projections mean two class publications |
| class : hook type | 1 : 1 (`H`) | `Class<T, H: ClassHooks<T>>` | per-family behavior must dispatch inside `H`/`T`; the class cannot vary hooks per device |
| provider key : outputs | 1 : N | `ProviderClass::Key` → lookup with `get_by_index` | the lookup object owns the output fan-out |
| provider key : consumers | 1 : N | deferred wake keyed by `DepSource` (`provider_publication_wakes_every_consumer_on_the_key`) | one re-probe per parked consumer per publication |
| device : provider keys | 1 : N | each `dev.env().provide::<C>(key, lookup)` publishes one key | duplicate keys are rejected (`AlreadyExists`); provider lifetime is boot-static — a device removal does not withdraw its providers |
| extension type : instance | 1 : 1 | `ExtensionRegistry` keyed by `TypeId` | one value per contract type; a service holds its own per-device maps internally |
| class : subscribers | 1 : N | `subscribe_available` / `subscribe_removed` | no unsubscribe; boot-static consumers assumed |
| removal | N : M cascade | `remove_device`: retryable exclusive claim, children-first (device children ∪ bus-instance members), quiesce → `Data` drop (publications withdraw) → `Removed` | `Removed` entries remain as tombstones; re-adding the same hardware gets a new identity (no stable hardware key) |

### Known closed shapes

These are decisions, not missing work; each has an exit condition:

1. **One device, two simultaneously bound drivers** — closed (Linux parity).
   The supported expansion is child devices owned by a parent driver.
2. **Per-publication liveness** — closed: the device is the liveness unit, so
   `Publication::is_available()` reads the shared `DeviceCore`.[^1]
3. **Provider withdrawal on device removal** — closed: clock providers are
   boot-static (`of_clk_add_hw_provider` semantics; see E-02c). Exit condition:
   a removable provider class ties its registration to a `Data` guard and adds
   the unregister path to `ProviderServices`.
4. **Multi-instance extensions** — closed: the extension *type* is the key.
   A family that needs N instances puts the map inside its one service value.

[^1]: The class tests in `drivers/base/src/class/tests.rs` were previously
orphaned (not linked into the unittest build); they are linked now, and the
multiplicity tests above live beside them.

## Porting Workflow

See [`docs/linux-porting-map.md`](../docs/linux-porting-map.md) — the normative
Linux → x-kernel mapping for AI-assisted driver transcription, with the golden
templates under `drv/`.
