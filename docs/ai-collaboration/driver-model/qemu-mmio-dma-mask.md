# Scoped QEMU modern-MMIO RNG DMA mask basis

## Decision and scope

`u64::MAX` is justified as the **address-width mask** for the existing R1-08
QEMU Arm virt direct modern-MMIO RNG policy. This conclusion combines the
transport's 64-bit queue addresses, 64-bit buffer descriptors, QEMU's actual
address-space path and the bounded host allocation policy below. It does not
follow from missing `dma-ranges`, nor authorize every 64-bit physical address.

Scope: available direct `virtio,mmio` children of `linux,dummy-virt`, modern
Version=2 RNG, coherent identity DMA, and the existing native providers. This
does not establish a default for arbitrary VirtIO, PCI, legacy MMIO, translated
windows, IOMMUs, non-coherent platforms or different machine configurations.

## Versions actually inspected

- X-Kernel baseline: `1fd56f1c76f5582d586a95ac8ca8f29196492644`.
- Installed `/usr/bin/qemu-system-aarch64 --version`: **10.0.11**, Debian
  package **1:10.0.11+ds-0+deb13u1**. Source inspection uses upstream tag
  **v10.0.11**, resolving to commit
  **e3ad0582fdcec5c194835b0b4e8342cdc197b5cb** (`git ls-remote` verified).
  The Debian source/patch set was not compared byte-for-byte with this tag.
- Local `/root/linux-stable` HEAD:
  **8d3ae59288f1e7d58d76558a6ee96d533bc5019f**; the inspected
  `drivers/virtio/virtio_mmio.c` has no local modifications.
- The official OASIS 1.3 URL currently identifies its text as **Committee
  Specification Draft 01, 6 October 2023**. References below pin that edition,
  not an assumed final standard.

## Primary-source chain

1. **Protocol width.** OASIS 1.3 CSD01 section 2.7.5 defines the split-ring
   descriptor address as `le64`. Section 4.2.2 supplies low/high 32-bit register
   pairs for the 64-bit descriptor, driver and device area addresses. These
   define representability, not RAM availability or a platform DMA permission.
   [Official specification, sections 2.7.5 and 4.2.2](https://docs.oasis-open.org/virtio/virtio/v1.3/csd01/virtio-v1.3-csd01.html).

2. **Modern transport does not truncate the high half.** QEMU's
   `virtio_mmio_write` stores the high registers and, on QueueReady, constructs
   each ring address by shifting a `uint64_t` high half by 32 and combining the
   low half before `virtio_queue_set_rings`. This is the non-legacy branch.
   [QEMU v10.0.11 virtio-mmio.c, lines 387–505](https://github.com/qemu/qemu/blob/v10.0.11/hw/virtio/virtio-mmio.c#L387-L505).

3. **Buffer addresses follow the same device address space.** `VRingDesc.addr`
   is `uint64_t`; split-queue descriptor processing passes its address and
   length into `virtqueue_map_desc`, which calls `dma_memory_map` using
   `vdev->dma_as`. Mapping can fail; field width is not a guarantee that every
   possible value maps memory.
   [Descriptor definition](https://github.com/qemu/qemu/blob/v10.0.11/hw/virtio/virtio.c#L62-L68),
   [mapping](https://github.com/qemu/qemu/blob/v10.0.11/hw/virtio/virtio.c#L1565-L1608),
   [split-queue consumer](https://github.com/qemu/qemu/blob/v10.0.11/hw/virtio/virtio.c#L1739-L1777).

4. **Direct-MMIO address-space selection.** `virtio_bus_device_plugged`
   initializes `dma_as` to `address_space_memory`. Its IOMMU-platform branch
   can substitute a transport `get_dma_as` result; the inspected MMIO bus class
   installs no such override. Thus this transport retains system-memory DMA,
   including when that feature branch is taken. This is evidence for the
   scoped identity binding, not a rule that ACCESS_PLATFORM can always be
   ignored.
   [Bus selection](https://github.com/qemu/qemu/blob/v10.0.11/hw/virtio/virtio-bus.c#L74-L92),
   [MMIO bus class](https://github.com/qemu/qemu/blob/v10.0.11/hw/virtio/virtio-mmio.c#L858-L875).

5. **Actual board and device.** Arm virt creates sysbus `virtio-mmio`
   transports and direct-root DT nodes with `dma-coherent`. RNG `chr_read`
   obtains buffers through `virtqueue_pop`, fills their input scatter/gather
   buffers, and returns them through `virtqueue_push`; it does not introduce
   an independent 32-bit RNG buffer-address path.
   [Arm virt construction](https://github.com/qemu/qemu/blob/v10.0.11/hw/arm/virt.c#L1163-L1194),
   [RNG buffer delivery](https://github.com/qemu/qemu/blob/v10.0.11/hw/virtio/virtio-rng.c#L43-L79).

6. **Linux corroboration, not a copied fallback policy.** At the local Linux
   HEAD above, `virtio_mmio_probe` lines 624–636 requests a 64-bit streaming
   and coherent mask for modern MMIO, then tries 32 bits if the DMA API rejects
   it. Legacy coherent-ring handling differs. This supports separating
   transport width from platform DMA feasibility; it does not require this
   kernel to imitate Linux's platform negotiation blindly.
   [Pinned Linux source](https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/drivers/virtio/virtio_mmio.c?id=8d3ae59288f1e7d58d76558a6ee96d533bc5019f#n624).

## Why the existing host policy remains bounded

At the X-Kernel baseline, `platform_model::stage` supplies the mask separately
from firmware conversion and rejects non-coherent/non-identity results.
`firmware_dma::from_firmware_with_qemu_virt_mmio_policy` substitutes a window
only for MissingWindow, using one complete nonempty RAM interval from the
provenance-bearing DT view. Explicit facts and errors are not overwritten.
The saved local `evidence/r1-08-firmware/qemu-virt.dts` under the R1 evidence
root records coherent MMIO nodes and RAM `[0x40000000, 0x80000000)`; the code
uses the current tree's interval, not these hard-coded endpoints.

`host_dma` reads the retained DeviceRef descriptor, checks the entire coherent
allocation span (including page rounding) or streaming bounce interval against
window and mask, and retains the original native release token. Native
allocation ownership and reserved-memory restrictions still apply. Therefore
the permitted set is the intersection of descriptor window, address mask and
valid native allocation/mapping, not the mask alone. See
`drivers/integration/kdriver/src/{platform_model,firmware_dma,host_dma}.rs`
at the baseline above.

## Acceptance boundary

This closes the missing **source rationale** for the narrow production mask.
It is not a new test result, an above-4-GiB exercise, exhaustive 64-bit hardware
proof, cache-maintenance validation, or IRQ/late-response teardown acceptance.
The package-source equivalence caveat remains explicit. A changed transport,
machine/address-space arrangement or DMA policy requires renewed evidence;
existing full-range rejection must not be weakened. Focused policy tests and
real-run results are recorded separately by the implementation owner.
