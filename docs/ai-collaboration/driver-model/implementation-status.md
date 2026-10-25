# Driver model R1 implementation checkpoint — 2026-10-25

## Scope

Branch: `xiaohui/driver-base-hwrng`. Implementation started from the accumulated
driver-model branch; the pre-checkpoint HEAD is `75a68bfbc`, incorporating main
`240fcbfd1`. This checkpoint does not claim a new rebase or upstream merge.
The approved design remains unchanged:
`bdc322d03950d8395f23e73daa8624a3ad27ec69bd4d75219c758db6d2f86249`.

- R1-01/02: per-device management serialization, lifecycle closure and probe
  progress acceptance, including controlled controller/child teardown.
- R1-03/04: hwrng-owned registration, explicit synchronous withdrawal, reliable
  host drain and stable binding provenance.
- R1-05: kernel-lifetime entropy work and demand handoff across source changes.
- R1-06: vendored instance DMA ownership and failure rollback; legacy Hal
  compatibility remains. Real host DMA/cache adaptation is not certified.
- R1-07: transport reset contract, RNG admission closure, teardown outside
  driver spinlocks, synchronized IRQ guard release,
  pending cancellation, constructor rollback and actual RNG/model integration
  tests. Teardown runs in sleepable context.

The integration fixture uses actual model callbacks, registration, host
completion/drain and the concrete RNG driver. A real consumer task blocks before
normal removal or post-publication probe failure wakes/drains it. HAL, transport
and IRQ delivery are controlled test implementations, not physical hardware.
Production MMIO binding/remove/rebind wiring belongs to R1-08.

## Validation

- Final configured aarch64 QEMU guest suite:
  `virtio,driver_model,kdriver,subsys_hwrng,entropy`: **210 passed, 0 failed**.
  SMP4, worktree-local disk, `--no-net --no-vsock`.
- `make build` and `make clippy` pass with existing dependency
  future-incompatibility notices.
- Standalone vendor tests: all features **76 unit tests + 6 doctests**;
  no default features **40 unit tests + 4 doctests**. Library clippy passes
  with two existing upstream warnings; strict all-target warning-free status
  is not claimed.
- Incremental Standards/Spec reviews have no outstanding findings for the
  completed slices. This is not a new full-branch security audit.
- Local command logs and recoverable snapshots are under ignored
  `.scratch/driver-model-r1/evidence/`, including `r1-07-model-checkpoint/`.
  Build directories, disk images and local tickets are not source deliverables.

## Deferred IRQ acceptance

The user explicitly deferred IRQ confirmation on 2026-10-25 and authorized
moving to the next development step. Do not change the independently maintained
IRQ worktree or redo Completion. R1-07 remains partially accepted:

- Shared-action preservation and concurrent handler notification-tail drain
  still require confirmation.
- Mock release-order tests are not proof of the real IRQ core's synchronization.
- R1-08 development may proceed against the synchronous IRQ provider contract.
  Hardware unbind/rebind safety and final acceptance must not be declared
  complete until the actual provider contract and pending IRQ cases are checked.

## Next: R1-08

First trace aarch64 QEMU firmware discovery, MMIO ownership and DMA constraints
(mask, offset, coherency) before selecting the host adapter. Do not infer an
unlimited DMA mask solely from absent dma-ranges. Then implement the single
RNG discovery handoff, instance resources and production model binding.
Retain separate evidence for controlled inputs, real Version=2 MMIO, sampling,
pending unbind, stale handles and rebind. Legacy PCI RNG is a regression target,
not a substitute for modern MMIO evidence.

Only local commits are authorized at this checkpoint; no push or main PR.

### R1-08 first slice: local firmware DMA decoding

`of::dma` now preserves local DMA windows without converting them into a mask.
It distinguishes missing and empty properties, supports numeric one-/two-cell
fields and multiple windows, and rejects malformed tuples, zero sizes and
overflowing ends. This is not yet connected to production device discovery.

Investigation used QEMU 10.0.11 with `virt,gic-version=3`, cortex-a76, SMP4,
1 GiB RAM, `virtio-rng-device` and `virtio-mmio.force-legacy=false`, dumping
the DTB without booting a guest. Root/MMIO nodes declare `dma-coherent` but
provide no MMIO `dma-ranges`. That does not establish unlimited addressability
or prove a runtime Version=2 register read. The checked-in default remains PCI.
DTB, DTS and command logs are local evidence under `r1-08-firmware/`.

Before production wiring, resolve ancestor translation/coherency and explicit
platform policy. The initial `DmaInfo(mask, offset, coherent)` could not represent
window lower bounds or gaps, and `translate` did not reject positive results
above `u64::MAX` before casting. The second slice below addresses the single-window
contract; production integration must still validate whole allocations. The OF
decoder deliberately does not synthesize a permissive mask. R1-08 remains in progress.

Slice validation: configured aarch64 guest `of` suite **16 passed, 0 failed**,
including five new DMA tests; coverage extraction succeeded. `make build`, `make clippy`,
new-file header checks and patch whitespace checks pass. Standards and Spec
reviews found no outstanding issues in this local-decoding slice; they do not
certify the pending production DMA or MMIO integration.

### R1-08 second slice: bounded device DMA view

Replaced the unbounded signed offset with `DmaWindow`, a validated contiguous
CPU-physical/bus window. The descriptor remains the configuration authority;
no host-global or second mutable device configuration was added. Device mask
and coherency remain separate from the bus window, following Linux's separation
of `dma_mask` and `bus_dma_region` (local `include/linux/dma-direct.h`).

`translate_range` validates the entire nonempty interval, not only its base.
Private window fields preserve both address-end invariants; full-width offsets
no longer pass through a signed integer or truncating cast. Invalid masks,
window under/overruns and arithmetic overflow fail rather than widening access.
The model mock now checks requested lengths; a valid base with an invalid end
is covered through the resource interface. Legacy `device_res`/`kdma` are unchanged.

This contract currently represents one window. A future firmware converter must
reject unsupported multirange input, resolve all ancestor addresses before
constructing a window, and require explicit platform policy for absent facts.
That converter and the actual per-instance host allocator remain pending; this
slice does not claim real-hardware DMA or unbind acceptance.

Validation: aarch64 QEMU `driver_os_api,driver_model,of` suite **93 passed,
0 failed**, with coverage extraction completed. `make clippy`, `make build`
and patch whitespace checks pass (existing dependency future-incompatibility
notices remain). Standards/Spec reviews found no blocking issues in this slice.
Logs are under local `r1-08-dma-window/`; no additional commit or push.

### R1-08 third slice: structural DMA ancestry

Added the missing structural prerequisite `FdtNode::parent`: immutable slice
identity rather than names, allocation-free O(node count) per lookup. Path
lookup and traversal now retain parent identity at the same position even when
legal NOP tokens follow node names. OF fixtures cover duplicate bus names and
NOPs using both entry points. This was found and corrected during review.
The review also caught ISA's name-based bus recognition (no `device_type`
required); flag-encoded ISA/PCIe names and PCI type aliases are now rejected
before numeric decoding, with a discriminating regression fixture.

`of::dma::resolve_dma` returns CPU-resolved single-window facts and optional
inherited coherency, separately from device masks/platform fallback. Every
translated child window must fit wholly in each ancestor mapping. Empty ranges
mean local identity; missing ranges cannot silently widen access. Unsupported
IOMMU/interconnect paths, PCI/ISA bus encodings, multiple windows, malformed
markers and invalid cell layouts fail explicitly. This is a conservative subset
of Linux's OF DMA handling, not a complete OF/IOMMU implementation.

Neither firmware discovery delivery nor the host allocator is wired yet.
In particular the QEMU root-child case still yields no explicit DMA range;
platform policy must supply a justified window instead of treating this as
full-address permission. Production MMIO and deferred IRQ acceptance remain open.

Final reviewed-source validation: aarch64 QEMU `of,driver_os_api,driver_model`
**98 passed, 0 failed**, including the five tree-resolution cases and both
review regressions; coverage extraction succeeded. `make clippy`, `make build`,
new-file header and whitespace checks pass. Both review findings are closed.
Local logs: `r1-08-dma-resolution/ut-reviewed.log`, `clippy.log`, `build.log`.
No additional commit or push.

### R1-08 fourth slice: discovery delivery and model conversion

Firmware discovery now carries the DMA resolver result alongside MMIO/IRQ.
A DMA error does not hide the device from non-DMA consumers. The host integration
entry `firmware_dma::from_firmware` preserves resolver errors, requires explicit
range/coherency, validates the separately supplied device mask and revalidates
window endpoints before constructing `DmaInfo`. No firmware-to-model dependency
was added. This is configuration conversion, not production binding/allocation.

QEMU platform investigation found the following primary-source basis:

- [QEMU v10.0.0 ARM virt board](https://raw.githubusercontent.com/qemu/qemu/v10.0.0/hw/arm/virt.c),
  `create_virtio_devices`: root MMIO nodes declare coherent DMA.
- [QEMU v10.0.0 VirtioBus](https://raw.githubusercontent.com/qemu/qemu/v10.0.0/hw/virtio/virtio-bus.c),
  `virtio_bus_device_plugged`: initial DMA address space is system memory;
  a transport may override it in the IOMMU path.
- [QEMU v10.0.0 MMIO transport](https://raw.githubusercontent.com/qemu/qemu/v10.0.0/hw/virtio/virtio-mmio.c)
  does not install a `get_dma_as` override.

These support an explicitly selected direct-MMIO platform policy, not a generic
fallback for every `virtio,mmio` node. Source evidence is upstream v10.0.0; the
local binary is Debian QEMU 10.0.11, not a source-identical verified build.
Platform selection, restricting fallback to known RAM, runtime Version=2 and
actual host DMA validation remain pending. Missing ranges still fail conversion.

Validation: aarch64 QEMU `khal,kdriver,of,driver_os_api,driver_model` **158 passed,
0 failed**, coverage extracted. A deterministic private-loop DT fixture delivers
both a normal node and a rejected DMA node with identities/MMIO and absent IRQ
unchanged; it does not replace global firmware state. Conversion tests cover
missing facts, typed rejection, invalid masks/endpoints and independent devices.
`make clippy`, `make build`, header and whitespace checks pass. Review's
deterministic-discovery coverage gap is closed; no outstanding slice findings.
Logs: local `r1-08-dma-discovery/{ut-final,clippy,build}.log`. No new commit/push.

### R1-08 prerequisite: coherent allocation failure rollback

The host DMA adapter exposed a prerequisite in kdma: mapping or platform prepare
failure after page acquisition previously lost the allocation, while ordinary
free ignored cleanup failures before returning pages to the common kalloc palloc.
The user approved addressing this prerequisite before continuing the adapter.

Implemented a private transaction module in `io/kdma/src/allocation.rs`, not a
new public allocator. Acquisition is pages -> mapping -> platform preparation;
failure unwinds attempted stages in reverse order, including partially changed
full ranges. Ordinary free shares the same cleanup. If platform release or
mapping restoration fails, retain/log pages rather than allow unsafe reuse;
there is no retry queue. Successful-allocation trace records survive failed
cleanup. The AArch64 optional KVM provider now propagates failed share/unshare
operations rather than reporting success after warnings. Design, security and
public contracts describe intentional capacity retention and its limitations.

Validation: aarch64 QEMU `kdma,khal,kdriver,of,driver_os_api,driver_model`
**163 passed, 0 failed**, with coverage extracted; five new tests cover acquisition
stages, partial-attempt rollback, cleanup failure, successful free, zero length,
page rounding and alignment forwarding through local fault injection.
Default `make clippy` and `make build` pass; formatting, new-file headers and
whitespace checks pass. Two read-only reviews found no blocking issue.
These tests validate transaction ordering, not actual KVM hypercall/cache behavior.
Evidence: local `r1-08-dma-rollback/{ut,clippy,build}.log`.
The optional `KFEAT_KVM_GUEST_MEM_SHARE=y` configuration also passes `make clippy`
(`kvm-clippy.log`); the default configuration was restored afterward. This is
compile/lint coverage only, not a KVM runtime acceptance result.
No commit/push. Per-device host DMA adapter and explicit platform policy remain
pending; `desc.dma` remains the sole device DMA authority, not `ResourceMap`.
