# Driver Crate Boundaries

The driver subsystem is split by role. Keep new code in the layer that owns the
contract it is implementing or consuming.

## Layers

```text
              device model and service runtime
             ┌──────────────────────────────────┐
             │ driver-core / driver-runtime     │
             │ driver-compose (composition root)│
             └───────────────┬──────────────────┘
                             │
        ┌────────────────────┴────────────────────┐
        ▼                                         ▼
  driver contracts                       subsystem drivers
  (class traits, capability ops)         (drv/*: one module per driver)
        │                                         │
        └────────────► device-res-xkernel ◄───────┘
                       (host backend bridge)
```

## Device Model and Runtime

| Crate | Role |
|------|------|
| `core/driver-core` | The device model: bus/device/driver contracts, id tables, deferred probe, the device domain. |
| `core/driver-runtime` | Per-device services (`DeviceServices` implementation over the resource providers), the virtio HAL, and the clock registry. |
| `core/driver-compose` | Composition root: runtime install, driver registration, DT/PCI enumeration into the domain. Called once from `kruntime`. |

## Driver Contracts

OS-neutral contracts; portable and host layers meet here.

| Crate | Role |
|------|------|
| `driver_base` | Common driver error/result and base device trait. |
| `device-res` | Kernel capability contracts requested by drivers: MMIO, IRQ, DMA, time, and devres ownership helpers. |
| `kdevice` | Shared bus/device/driver model, topology, matching, lifecycle, and device-managed cleanup storage. |
| `kclass` | Runtime publication layer for typed device classes. |
| `block`, `char_driver`, `display`, `input`, `net`, `vsock`, `hwrng` | Device class contracts that concrete drivers implement and kernel subsystems consume. |

Contracts stay host-free where possible (`driver_base`, `device-res`,
`devnode`, `kclass`, and the per-class trait crates). Two pragmatic
exceptions carry kernel-runtime dependencies by design: `block` (its
dispatch services use `kirq`/`ktask`/`kcpu_id_map` — they are kernel
class services, not portable contracts) and `kclass`'s `node_compat`
(vendored input formatting). Treat those as the kernel-owned half of
their subsystem.

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
| `integration/kdevice-uevent` | Ordered devnode lifecycle events to netlink kobject uevents; the uevent payload format is a udev-visible compatibility surface. |

## Host Adapters and Platform Support

| Crate | Role |
|------|------|
| `adapters/xkernel/device-res` | Implements the `device-res` capability traits against `memspace`, `kirq`, `kdma`, `khal`. |
| `platform/*` (`pci`, `of`, `acpi`, `console`, `irq`, `timer`, `rtc`, `x86-apic`, `aarch64-pmuv3`, `rs-fdtree`, `dice`) | Bus, firmware, and platform infrastructure; `pci::support` owns CAM/BAR configuration shared by enumeration and drivers. |

## Porting Workflow

See `docs/linux-porting-map.md` — the normative Linux→x-kernel mapping for
AI-assisted driver transcription, with the golden templates under `drv/`.
