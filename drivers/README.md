# Driver Crate Boundaries

The driver subsystem is split by role. Keep new code in the layer that owns the
contract it is implementing or consuming.

## Layers

```text
                core/  device model + services + composition root
               ┌──────────────────────────────────────┐
               │ driver-core / driver-runtime         │
               │ driver-compose                       │
               └──────────────────┬───────────────────┘
                                  │
      ┌───────────────┬───────────┴───────────┬─────────────────┐
      ▼               ▼                       ▼                 ▼
 common/         subsystems/              drv/             bus/ firmware/
 (cross-          (per-class APIs          (one module      (PCI access,
  subsystem         + class services,      per driver,       DT/ACPI/FDT
  foundations:      e.g. block dispatch)   family crates)    parsing)
  driver_base,
  device-res,           │                    │
  kclass, devnode)      │                    │
                        └────► adapters/xkernel/device-res ◄──┘
                               (capability backend bridge)
                                        │
                                integration/
                                (console runtime, uevent-netlink)
```

## Device Model and Runtime (`core/`)

| Crate | Role |
|------|------|
| `core/driver-core` | The device model: bus/device/driver contracts, id tables, deferred probe, the device domain, extension services. Family-agnostic by construction. |
| `core/driver-runtime` | Per-device services (`DeviceServices` over the resource providers), the virtio transport extension and HAL, and the clock registry. |
| `core/driver-compose` | Composition root: runtime install, driver registration, DT/PCI enumeration into the domain. Called once from `kruntime`. |

## Cross-Subsystem Foundations (`common/`)

Only crates that several subsystems depend on by design — not a dumping
ground. Anything class-specific belongs in `subsystems/`.

| Crate | Role |
|------|------|
| `driver_base` | Common driver error/result and base device trait. |
<<<<<<< HEAD
| `device-res` | Kernel capability contracts requested by drivers: MMIO, IRQ, DMA, time, and devres ownership helpers. |
| `kdevice` | Shared bus/device/driver model, topology, matching, lifecycle, and device-managed cleanup storage. |
| `kclass` | Runtime publication layer for typed device classes. |
| `block`, `char_driver`, `display`, `input`, `net`, `vsock`, `hwrng` | Device class contracts that concrete drivers implement and kernel subsystems consume. |
=======
| `device-res` | Capability contracts (MMIO, IRQ, DMA, time) with RAII handles. |
| `devnode` | Model-agnostic userland node projection (devtmpfs/sysfs/uevent): data-driven `DevNodeType`, `DeviceNumber`, `DeviceLocation`. |
| `kclass` | Typed class publication: detached class parents, per-class registries, availability/removal subscriptions. |
>>>>>>> 06c873276 (refactor(drivers): reorganize top-level layout for unambiguous driver placement)

## Per-Class Subsystems (`subsystems/`)

APIs, class services, and shared objects for one device class.
`subsystems/block` owns the Gendisk model, completion waiting, the
softirq reclaimer, and IRQ wiring — that is kernel class service by
design (it uses `kirq`/`ktask`), and the `contracts`-era pretense that
it was a pure interface is gone with the directory name.

## Concrete Drivers (`drv/`)

One subsystem crate per family, one module per driver, one feature per
driver — mirroring Linux's file-per-driver model. A virtio driver's
hardware/protocol implementation lives beside its driver modules
(`drv/virtio/device/`, package `virtio` — shared with the HAL and
class layers, hence a separate package), so one family is one tree.

## Bus and Firmware (`bus/`, `firmware/`)

`bus/pci` — CAM/BAR configuration and access, shared by enumeration and
drivers. `firmware/{of,acpi,rs-fdtree}` — DT/ACPI parsing and node
discovery. The remaining `platform/` residents (irqchip, timer, rtc,
apic, pmu, console) are hardware controller drivers and boot
integration; their long-term home is `drv/<family>` and
`integration/console`, migrated one-by-one when touched.

## Subsystem Drivers (`drv/`)

One subsystem crate per family, one module per driver, one feature per driver —
mirroring Linux's file-per-driver model. Adding a driver touches: a module
file, one feature line in its subsystem crate, one compose registration, and a
Kconfig block.

| Crate | Modules |
|-------|---------|
| `drv/virtio` | `blk`, `net`, `rng`, `gpu`, `input`, `vsock`, `virtio_9p`, plus the `mmio` transport driver. |
| `drv/serial` | `pl011`, `ns16550` (MMIO + I/O-port). |
| `drv/clk` | `fixed_clock` (DT `fixed-clock` provider). |
| `drv/block` | `ramdisk`. |

## Shared Device Layers

| Crate | Role |
|------|------|
| `devices/virtio` | Virtio device-family constructors (`VirtIoBlkDev`, …) over the vendored `virtio-drivers` transport, implementing the class traits. Shared by the drv/virtio modules and the HAL. |
| `integration/uevent-netlink` | Netlink transport for kobject uevents — the one drivers-tree consumer of `knet`. The payload format (a udev-visible compatibility surface) lives in `common/devnode::uevent`, shared with sysfs attributes. |

## Host Adapters and Platform Support

| Crate | Role |
|------|------|
| `adapters/xkernel/device-res` | Implements the `device-res` capability traits against `memspace`, `kirq`, `kdma`, `khal`. |
| `platform/*` (`pci`, `of`, `acpi`, `console`, `irq`, `timer`, `rtc`, `x86-apic`, `aarch64-pmuv3`, `rs-fdtree`, `dice`) | Bus, firmware, and platform infrastructure; `pci::support` owns CAM/BAR configuration shared by enumeration and drivers. |

## Porting Workflow

See `docs/linux-porting-map.md` — the normative Linux→x-kernel mapping for
AI-assisted driver transcription, with the golden templates under `drv/`.
