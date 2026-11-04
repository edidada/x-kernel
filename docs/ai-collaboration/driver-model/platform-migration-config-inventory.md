# Platform / VirtIO-MMIO migration: configuration inventory

## Scope and evidence

Read-only source inventory at `3cee34dcbc9f9ba4231edb43287439785149bacf`
(`3cee34dcb`). This note is design input, not authorization to migrate drivers,
change defaults, or claim runtime acceptance. This investigation produced only
documentation; no configuration, build, QEMU run or production change was performed.
Sources below are repository-relative, with line numbers at that baseline.
Enumeration ownership and each driver's shutdown contract are separate parent
inventories. No external QEMU behavior was inferred from a feature name.

## Facts: configuration versus attachment

`drivers/Kconfig:125` defines a mutually exclusive VirtIO bus choice, default
PCI. `xtask/xconfig/src/build_config.rs:300` converts the two selections into
MMIO/PCI/None and rejects both enabled. `xtask/xkmake/src/qemu.rs:613` maps MMIO
to device suffix `device`, PCI to `pci`; requested devices with no selected bus
cause an error. The same suffix applies to all devices automatically appended
by `add_devices`, not only RNG.

The checked-in AArch64 QEMU defconfig explicitly leaves MMIO unset
(`platforms/kplat-aarch64/qemu_defconfig:32`). Its GPU/input and RNG driver
flags are set at lines 18–19 and 26; entropy's VirtIO consumer is separately
enabled at line 27, networking at line 22 and vsock at line 34. Missing settings
must be resolved through Kconfig, not interpreted as disabled: filesystem and
ext4 default on (`fs/Kconfig:3`, `:9`), block defaults on with filesystem support
(`drivers/Kconfig:173`), net defaults on with networking (`:203`), and socket
defaults on with vsock (`:211`).

The other inspected QEMU defconfigs also explicitly leave MMIO unset:
`platforms/kplat-riscv64/qemu_defconfig:28`,
`platforms/kplat-loongarch64/qemu_defconfig:23`,
`platforms/kplat-x86_64/qemu_defconfig:30`,
`platforms/kplat-aarch64/qemu_crosvm_defconfig:40`,
`platforms/kplat-aarch64/qemu_virtcca_defconfig:28`, and
`platforms/kplat-x86_64/qemu_csv_defconfig:30`.
This is a configuration observation, not proof these platforms support an
equivalent MMIO migration. The CrosVM-oriented config additionally requests a
secondary root block device (`qemu_crosvm_defconfig:30`); it is not the minimal
single-disk candidate.

| Driver configuration | Automatic QEMU attachment condition | Generated device / dependency |
|---|---|---|
| `KFEAT_DRIVER_VIRTIO_BLK` | Enabled and no `--no-block` | `virtio-blk-{suffix}`, raw `disk0`; disk must exist outside dry-run (`qemu.rs:482`, `:494`) |
| `KFEAT_DRIVER_VIRTIO_NET` | Enabled and no `--no-net` | `virtio-net-{suffix}`, user netdev, TCP/UDP host forwarding; host port resolution (`qemu.rs:483`, `:512`) |
| `KFEAT_DRIVER_VIRTIO_GPU` | Enabled **and** `--graphic` | `virtio-gpu-{suffix}`, disables VGA and adds serial monitor (`qemu.rs:484`, `:536`) |
| `KFEAT_DRIVER_VIRTIO_SOCKET` | Enabled, no `--no-vsock`, and QEMU device-support probe succeeds | `vhost-vsock-{suffix}`; host CID/backend constraints remain (`qemu.rs:485`, `:543`, `:576`) |
| `KFEAT_DRIVER_VIRTIO_RNG` | Enabled | `rng-random,id=host_rng0` plus `virtio-rng-{suffix}`; no corresponding no-RNG switch in this function (`qemu.rs:486`, `:566`) |
| `KFEAT_DRIVER_VIRTIO_INPUT` | No automatic attachment branch in `add_devices` | Driver availability does not establish a VirtIO keyboard/mouse/tablet; enabling graphics does not append one (`qemu.rs:475`) |
| `KFEAT_DRIVER_VIRTIO_9P` | No automatic attachment branch in `add_devices` | No automatic fsdev/9P device here; additional arguments/backend preparation require separate inventory (`qemu.rs:475`) |

`xtask/xkmake/src/cli.rs:248` defaults the image to `disk.img`; lines 252–270
define the block/net/vsock suppressors and opt-in graphics. Extra QEMU arguments
are appended after platform and automatic device arguments (`qemu.rs:49–54`).
Consequently the final launch command, not just resolved features, is the
attachment evidence. User arguments can add devices beyond this table. These
sources describe command generation, not successful QEMU realization or guest
binding. In particular vsock can be skipped by the support probe.

Kfeat driver features forward into kdriver (`api/kfeat/Cargo.toml:144–150`);
they enable code, not a hardware-presence assertion. Entropy consumption is
separate from the RNG driver (`drivers/Kconfig:219–234`). The runner contains
no automatic `virtio-mmio.force-legacy=false` setting: MMIO suffix alone is not
proof of MMIO Version 2. The previous RNG acceptance explicitly supplied that
argument and checked the selected device; preserve this distinction.

## Facts: boot and dependency constraints

- AArch64 runner selects QEMU `virt,gic-version=3` without VMM, or
  `virt,virtualization=on` with VMM; it loads the direct kernel and chooses host
  CPU under acceleration or cortex-a76 otherwise (`qemu.rs:143–159`). This is
  not a UEFI boot candidate. Other architectures have separate platform branches
  (`qemu.rs:161` onward), so do not extrapolate AArch64 findings to them.
- Driver discovery/initialization precedes entropy initialization, filesystem
  mounting and networking (`core/kruntime/src/lib.rs:390–423`). Migrating block
  enumeration can therefore break root mounting before a later test runs.
- Root selection honors a configured name, otherwise first/secondary block;
  missing requested media is fatal (`fs/boot/src/lib.rs:550–580`). The related
  policy is `Kconfig:278–292`. A `--no-block` experiment is **not** a safe
  default for the existing disk-backed test guest.
- The test coverage extractor reads the configured disk with debugfs
  (`xtask/xkmake/src/coverage.rs:229–244`). Retain a worktree-private writable
  image for the established guest/coverage path; this inventory does not propose
  a new diskless test infrastructure.
- Network and vsock init are feature-gated independently of QEMU attachment
  (`core/kruntime/src/lib.rs:420–423`). Their no-device paths report warnings
  (`net/knet/src/lib.rs:171`, `:225`); this does not prove network-dependent
  consumers/tests can run without devices.
- Production 9P host-share mounting is enabled by fs9p outside unittest
  (`core/kruntime/src/lib.rs:417–418`), and mounting failure is fatal
  (`fs/boot/src/lib.rs:583–602`). A future 9P candidate must prepare an actual
  export/device; compiling the driver is insufficient.

## Proposed minimal first candidate — not an executed result

Use the canonical AArch64 QEMU defconfig as a reproducible starting point, then
resolve a config selecting MMIO instead of PCI with the project's defconfig /
olddefconfig flow. Preserve the normal filesystem/ext4/block path and private
disk; retain RNG as the already-integrated control. Leave graphics off and
suppress automatic net/vsock with the existing runner flags. Supply
`-global virtio-mmio.force-legacy=false` and independently verify actual
discovered transport versions and unique owners. Keep the existing SMP setting
for lifecycle tests; there is no need to introduce another machine or rootfs.

Under those assumptions the runner explicitly requests **block + RNG** through
MMIO, not “RNG only”. This is the smallest useful disk-backed migration candidate
from this inventory: it preserves boot/coverage and makes block enumeration the
next critical dependency. It is not permission to unbind the mounted root disk.
A destructive block-lifecycle test would need a separately owned non-root disk
and explicit flush/mount/consumer design before implementation.

Keep an unchanged canonical PCI run as a compatibility control. Expand net,
vsock, GPU/input and 9P individually only after their actual attachment and
consumer prerequisites are designed. Suppressing a device is isolation for this
first candidate, not acceptance of its migration or complete platform coverage.
Choose entropy-worker enablement per test purpose, recording it: worker-enabled
normal-consumption regression and worker-disabled controlled lifecycle fixtures
prove different things.

## Unknowns / decisions needed before migration design

1. Define “complete platform enumeration”: whether it includes built-in UART,
   RTC, interrupt-controller/timer discovery and empty VirtIO slots as well as
   explicitly attached VirtIO devices. The automatic-device table is not a
   platform inventory and assigns no ownership to those devices.
2. Parent audit must establish current/target owner, probe ordering and shutdown
   contract for each driver, especially root block. A global MMIO bus selection
   changes more than RNG; successful boot is not independent teardown proof.
3. Input/9P need explicit fixture/backend choices; vsock additionally needs host
   support. This audit did not invoke QEMU, probe host capabilities, or resolve
   a new `.config`, so actual devices, DT slots and interrupts remain unmeasured.
4. Modern-only first acceptance versus preserving legacy-MMIO compatibility
   needs an explicit design matrix. Do not silently remove existing legacy or
   PCI behavior because the first candidate uses modern MMIO.
5. Other platform defconfigs are source inventory only. No all-architecture
   migration, firmware handoff change, network close implementation, physical
   hotplug, IOMMU or MSI acceptance is authorized by this note.
