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

### R1-08 fifth slice: per-device host coherent DMA

Previous reviewed slices are now committed: `ba056eff0` contains kdma rollback
and `456b29d11` contains firmware resolution/discovery and bounded model views.
Following the user's updated delivery instruction, subsequent implementation
slices receive self-review, standards/spec review and validation before commit.

`kdriver::host_dma::alloc_coherent` implements the host DMA service operation
using `DeviceContext` and authoritative `desc.dma`. It checks the complete
page-rounded native allocation against the window/mask and translated alignment,
zeroes it including padding and returns a CoherentMem guard. Native CPU/bus/layout
ownership is independent of the translated bus address, including when rejecting
an allocation. No duplicate device DMA configuration or allocation registry is
introduced. Missing configuration and non-coherent devices are rejected; cache
maintenance, constrained allocation search and streaming DMA remain unsupported.

Standards review found no issue. Spec review identified two test gaps: successful
allocation after rejection did not prove rollback, and plain Drop did not prove
original-token release exactly once. A private release-operation seam and local
observer test now verify original CPU/native-bus/layout, no release while live,
one release after Drop/rejection, full-padding zeroing and no writes on rejection.
Follow-up review confirmed both gaps closed with no remaining blocker.

Aarch64 guest regression: **169 passed, 0 failed**, coverage extracted. Tests use
actual native pages for allocation and two translated views, plus a model callback
fixture through DeviceResourceServices that allocates, unbinds and rebinds using
each descriptor. No addresses are submitted to hardware; native cleanup failure
remains subject to kdma's logged-retention policy. Logs are local under
`r1-08-host-dma/`, with final guest evidence in `ut-final.log`.
Final `make clippy` and `make build` pass (`clippy.log`, `build.log`); formatting,
new-file license headers and whitespace checks pass. No outstanding review
finding remains for this slice. Existing third-party future-incompatibility
warnings are unchanged. This checkpoint is committed locally, not pushed.

Next: explicit QEMU MMIO platform policy and production host service composition,
then unique enumeration handoff and real RNG acceptance within R1-08. This helper
does not by itself enable the production MMIO boot path or close the IRQ gate.

### R1-08 sixth slice: host MMIO ownership and native publication prerequisite

Pre-wiring inspection found that the flattened firmware device record does not
prove direct attachment to the QEMU root bus. A generic `virtio,mmio` match must
not silently select a QEMU identity-DMA fallback. Platform policy remains pending
explicit provenance validation; this slice completes the native MMIO ownership
operation needed by the same production host service composition.

The native `memspace::iomap_device` path created a mapping before attempting
registry Vec growth. If that growth failed, the caller received no token for
cleanup. Capacity reservation now precedes mapping creation under the existing
registry lock. Page-rounded endpoint overflow is also rejected before side
effects. These are narrowly scoped prerequisite fixes, not a claim to have solved
all platform-prepare/page-table internal rollback. The capacity-failure ordering
is reviewed structurally; no allocator OOM injection facility was introduced.

`kdriver::host_mmio::iomap` owns one native mapping reference per model IoMap
guard and releases the exact returned address on Drop. It preserves explicit
bus-resolved ranges, page offsets, counted sharing, pinned mappings and distinct
Invalid/NoMemory/Io errors. Unmap failures are logged rather than swallowed.
No device resource facts are copied into another registry, and no real device is
enabled by adding this helper. Design/security/rustdoc contracts are synchronized.

Tests cover byte/page-end overflow and local counted/pinned lookup semantics.
The real mapping smoke test acquires two guards for one synthetic physical
interval, verifies first Drop retains the mapping and final Drop removes both
registry entry and VMA. A model callback test maps a descriptor-indexed resource,
unbinds, rebinds and removes using the production host helper. Both use serial
guest execution and never access the synthetic MMIO addresses. They do not prove
hardware quiescence or optional KVM MMIO guard hypercall behavior.

Self-review and independent standards/spec reviews found no blocker, including
the follow-up model callback and VMA assertions. Final validation evidence is
recorded under local `r1-08-host-mmio/`.

Final aarch64 guest regression selects
`memspace,kdma,khal,kdriver,of,driver_os_api,driver_model`: **234 passed, 0 failed**,
coverage extracted (`ut-final.log`). This includes all selected memspace cases,
not only the new mapping cases. Runs use this worktree's disk with network/vsock
disabled. No platform identity-DMA fallback, production service installation or
IRQ acceptance result is implied by these tests.

Final `make clippy` and `make build` pass (`clippy.log`, `build.log`); formatting,
new-file headers and whitespace checks pass. Existing third-party future-Rust
compatibility warnings remain unchanged. The native prerequisite and host helper
are committed separately after review; no push is performed. Next is explicit
platform/node provenance for the DMA policy and production host composition,
not broadening the generic converter into an implicit identity fallback.

### R1-08 seventh slice: provenance-bound QEMU MMIO DMA policy

HAL discovery now offers a transient FirmwareDeviceView with private original
node/tree provenance, while preserving the existing flat visitor. Host policy
queries actual compatibles, available direct-root attachment and one complete,
nonempty root RAM interval. Matcher aliases cannot forge any of these facts.
No additional metadata is stored on model devices.

The explicit kdriver QEMU Arm virt policy fills only MissingWindow, and only for
direct available virtio,mmio children of linux,dummy-virt on aarch64. It uses a
bounded identity RAM window, preserves independent mask/coherency requirements,
and never overwrites explicit windows or firmware errors. Generic from_firmware
remains strict. Native allocator ownership/reserved-memory checks still apply.

Self-review and independent standards/spec review found an overly permissive
disabled-RAM string check and a deterministic precedence-test gap. Both are
fixed: only exact disabled status is ignored; malformed trailing bytes reject
the RAM evidence. Deterministic tests cover explicit translated windows, rejected
facts, invalid masks/windows, absent/false coherency and mask-limited fallback.
Topology fixtures cover matcher spoofing, nested buses, unavailable nodes,
missing/multiple RAM and malformed tuples without replacing the global boot DT.
Boot discovery tests additionally exercise the actual QEMU policy path.
Both independent re-reviews report no remaining blocker.

This is not production provider installation or real RNG hardware acceptance.
R1-08 still needs host service composition, exclusive new/legacy MMIO ownership
handoff and modern-MMIO RNG bring-up. Deferred IRQ acceptance stays separate.
Final validation logs for this slice are under local `r1-08-dma-policy/`.

Final aarch64 guest regression selects
`memspace,kdma,khal,kdriver,of,driver_os_api,driver_model`: **241 passed, 0 failed**
with coverage extracted (`ut-final.log`). The worktree-local QEMU run disables
network/vsock and does not claim an exclusive performance measurement window.

`make clippy` and `make build` pass (`clippy.log`, `build.log`). Clippy's
nonminimal-bool suggestion was applied without changing the topology predicate.
Existing third-party future-Rust compatibility warnings remain unchanged.
Formatting and whitespace checks pass. This slice is committed locally after
review; no push is requested or performed.

### R1-08 eighth slice: host service composition

`host_services::new_device_services` now composes native MMIO, descriptor-scoped
DMA and per-device management locks behind the existing DeviceServices contract.
The caller supplies the existing OS-api IrqOp capability; there is no new IRQ
abstraction, global IRQ provider lookup or change to kirq. Firmware routes map
through that provider, while resolved virqs bypass mapping. Each DevIrq guard
retains the exact mapped resource, returned action token and provider until its
single release. The adapter does not independently mask/enable shared lines.
Unsupported clock/provider families fail explicitly; extension tables are local
to each aggregate, and device facts remain solely in descriptors.

Existing lifecycle test services now delegate to this production composition,
retaining only factory-call instrumentation and an unavailable IRQ capability.
Real MMIO/DMA, management-lock and hwrng lifecycle regressions therefore exercise
the same composition rather than reimplementing host behavior. Controlled IRQ
tests cover nonidentity translation, resolved-number bypass, original release
identity, unbind/rebind, failed mapping/request/probe and extension isolation.
They do not validate physical IRQ delivery, IRQ drain or hardware shutdown.

The first aarch64 regression run selects
`memspace,kdma,khal,kdriver,of,driver_os_api,driver_model`: **244 passed, 0 failed**,
coverage extracted (`r1-08-host-services/ut.log`). QEMU uses the worktree-local
disk with network/vsock disabled. Production global installation, the concrete
IRQ provider connection, exclusive new/legacy MMIO handoff and real RNG hardware
acceptance remain in R1-08. The deferred IRQ acceptance gate is not reopened or
claimed complete by this composition slice.

Self-review and independent standards/spec reviews found no blocker. Final
`make clippy` and `make build` pass (`clippy.log`, `build.log`); formatting,
new-file license headers and whitespace checks pass. Cargo regenerated the
lockfile to include kdriver's direct optional driver-os-api dependency, without
changing package versions. Existing third-party future-Rust warnings are
unchanged. The slice is prepared for a local commit; no push is requested.

### R1-08 ninth slice: concrete RNG instance-DMA forwarding

Tracing the production handoff exposed a remaining prerequisite: the vendored
RNG already accepts DmaBackend, but the concrete VirtIoRngDev wrapper still fixed
every binding to the global static HAL. Switching enumeration first would not
make queue/request DMA obey the new device's constraints. This slice closes that
forwarding gap before enabling new/legacy ownership selection.

VirtIoRngDev now carries a defaulted DMA backend through its request and vendor
RNG. The new try_new_irq_with_dma constructor retains one backend instance for
rings, sharing and release; the legacy constructor uses StaticHal unchanged.
The backend must be Send for a cross-task runtime, and its completion cleanup
must be legal under the request lock. Reset, IRQ drain and cancellation ordering
remain unchanged; no host-kernel dependency or new allocator contract is added.

Two controlled tests cover separate instance ownership with pending requests,
stopping one without releasing the other, old closed references and IRQ-constructor
failure rollback. Static-HAL counters remain untouched on the instance path.
The first aarch64 guest selection `virtio,kdriver,driver_model,subsys_hwrng,entropy`
passes **232 tests**, with coverage extracted. Build and clippy also pass.
This suite selection differs from the previous host-service slice's 244 tests.

Independent review identified an early-assert cleanup gap in the new pending
test; it now snapshots observations, stops both instances and drops references
before asserting. Final regression evidence is recorded in local
`r1-08-rng-instance/ut-final.log`. Production native instance DMA adaptation,
concrete IRQ-provider connection, exclusive MMIO enumeration handoff and real
modern-MMIO RNG lifecycle acceptance remain pending. No boot ownership changes
or hardware-acceptance claims are made by this slice.

Final regression passes **232/232**, with coverage persisted and extracted.
Self-review and independent standards/spec re-review have no remaining blocker.
Formatting and whitespace checks pass; existing third-party future-Rust warnings
are unchanged. This slice is prepared for a local commit, not a push.

### R1-08 tenth slice: unpublished streaming DMA rollback prerequisite

Before implementing the host instance backend, inspection found that kdma's
receive/bidirectional unmap copies bounce bytes back. Rejecting a native mapping
after device-window validation must instead preserve the original buffer: no
device has received that mapping yet. The host must not fake a transmit direction
to bypass copy-back, nor implement another bounce allocator.

kdma now provides explicit unsafe discard_dma_buffer for this unpublished-only
case. It consumes the original mapping record and recycles bounce storage without
copy-back, sharing the ordinary finalization path. Normal completion semantics
remain unchanged. It is not cancellation of an already published DMA request.

Tests cover all directions for discard versus normal completion. Independent
review requested exact resource-recovery evidence; a locked baseline regression
now verifies active-record removal and restoration of available bounce capacity,
with assertions after cleanup/unlock. The capacity observation is test-only.
The re-review reports no remaining blocker. Logs are under local
`r1-08-streaming-rollback/`.

This is a narrowly scoped native prerequisite, not completion of the host
instance DMA backend or production MMIO RNG handoff. The model's held DMA
capability, host adapter and VirtIO binding still need integration before the
old enumeration path can relinquish device ownership.

Final aarch64 guest selection
`kdma,kdriver,virtio,driver_model,subsys_hwrng,entropy` passes **240/240** with
coverage extracted (`ut-final.log`). QEMU uses the worktree-local disk and no
network/vsock. `make clippy` and `make build` pass after adding the required
Safety rustdoc to the private finalization helper; no behavior changed in that
follow-up. Formatting, new-file license and whitespace checks pass. Existing
third-party future-Rust warnings remain. Local commit only; no push requested.

### R1-08 eleventh slice: held device DMA capability and native adapter

The model now acquires a held OS-neutral DeviceDma capability through the
callback-bound resource view. The host retains the original DeviceRef, not a
second DmaInfo or a global current-device selector. Coherent allocations and
streaming mappings consult the same authoritative descriptor. The legacy DmaOp
interface remains unchanged; this is the new model's owned-capability path.

StreamingMapping owns one release obligation. Native adaptation translates the
complete bounce-buffer physical interval, while retaining the original native
bus token for release. Rejected unpublished mappings use discard_dma_buffer,
preserving caller bytes. Normal Drop performs directional copy-back without
freeing the caller-owned buffer. Missing/non-coherent configuration and invalid
masks cannot fall back to unrestricted native DMA.

Tests cover callback lifetime, two independent descriptor views, retained
capabilities after logical removal, unique guard release, full-range rejection,
directional copy-back and repeated rejection beyond total bounce-pool capacity.
These are software ownership/translation regressions, not hardware acceptance.

VirtIO's raw DmaBackend token adaptation, production provider installation,
exclusive new/legacy enumeration handoff and real RNG acceptance remain pending.
In particular, rejecting a host mapping before publication is not cancellation
of a published request or proof of vendor descriptor-rollback semantics. The
deferred IRQ acceptance gate is unchanged. Validation evidence for this slice
is recorded under local `r1-08-device-dma/`.

Self-review and independent standards/spec review caught an RX initialization
obligation: native receive bounce storage can contain unwritten bytes. The host
now initializes the complete receive interval before publication, with an
immediate-Drop regression. Acquisition/release context documentation matches the
native non-sleeping path, including request-lock use and lock-order restrictions.
The re-review reports no remaining blocker. This does not promise allocation-free
operation or bounded spinlock latency.

Final aarch64 selection
`kdma,kdriver,virtio,driver_os_api,driver_model,subsys_hwrng,entropy` passes
**258/258**, with coverage persisted and extracted (`ut-final.log`). The first
compile exposed two test assertion temporary-guard lifetimes; both now assert
on snapshots after releasing the mutex. `make clippy`, `make build`, formatting
and whitespace checks pass. QEMU used the worktree-local disk without network
or vsock. Existing third-party future-Rust warnings remain. Local commit only;
no push requested.

### R1-08 twelfth slice: portable VirtIO device-DMA adapter

DeviceDmaBackend bridges the held OS-api capability to the vendored raw tuple
ABI. Clones retain one device's private allocation/mapping guards; complete
release tuples select the original guard. Provider calls and guard destruction
occur outside the record locks. The adapter has no host dependency, copied DMA
configuration or cross-device registry. Metadata retains the current infallible
allocation policy. The integration contract now explicitly records this private
guard-storage refinement rather than pretending the raw vendor descriptors can
hold an OS-api guard directly.

Queue rollback revealed that the previous slice's receive bounce zeroing was
insufficient: ordinary unshare of an unpublished mapping overwrote its original
bytes, and partial completion clobbered untouched tails. Native RX initialization
now copies the initialized original buffer, matching Linux swiotlb's transparent
bounce semantics. DeviceDma requires no-write round-trip and partial-write
preservation. Internal address-validation rejection still uses discard; no new
vendor cancellation protocol is introduced.

Construction is unsafe: callers must establish IRQ/request-lock-compatible
provider contexts and usable addresses/platform-sharing effects in both VirtIO
ACCESS_PLATFORM modes. The intended QEMU identity binding satisfies this; the
adapter does not claim arbitrary translated/IOMMU-mode support. Existing static
HAL paths are unchanged. Production provider installation, exclusive MMIO handoff,
real RNG acceptance and deferred IRQ acceptance remain separate work.

Tests exercise exact clone release, independent instances, zero bus addresses,
native model-to-VirtIO adaptation, allocation failure, direct/indirect queue
rollback and RX partial-write preservation. Evidence is recorded under local
`r1-08-virtio-device-dma/`; mock queues do not prove hardware shutdown.

Self-review and independent standards/spec review identified a mock pointer
provenance issue: writable bounce addresses must not come from a shared slice
pointer. The test now transfers Box ownership to a raw allocation and uniquely
reconstructs it after copy-back. Re-review reports no remaining blocker.

The aarch64 guest selection
`kdma,kdriver,virtio,driver_os_api,driver_model,subsys_hwrng,entropy` passes
**267/267**, with coverage persisted and extracted (`ut.log`). This includes
native capability-to-backend clone release and RX round-trip under a request
spinlock, but not real concurrent device IRQ acceptance. QEMU used the
worktree-local disk without network or vsock.

`make clippy` and `make build` pass (`clippy-final.log`, `build.log`). Clippy's
initial run caught four test-only pointer-arithmetic blocks containing multiple
unsafe operations; bounded pointer arithmetic now uses wrapping_add, leaving
only the volatile access unsafe. Formatting, headers and whitespace checks pass.
The lockfile adds only virtio's optional driver-os-api dependency; package
versions are unchanged. Existing third-party future-Rust warnings remain.

The post-lint regression also passes **267/267**, with coverage extracted
(`ut-final.log`). This slice is ready for a local commit; no push requested.

### R1-08 thirteenth slice: model IRQ acquisition for concrete RNG

The native ordinary IRQ provider adapts os-api routes and exact action tokens
to kirq without a second registration table. Explicit firmware domains survive
mapping; resolved virqs remain handles rather than being guessed as hwirqs.
The native service constructor only composes capabilities, without globally
installing them or triggering enumeration. Release delegates synchronous drain
to kirq and cannot report successful reclamation after a failed detach.

The portable RNG accepts a one-shot IRQ factory that can borrow the real model
callback's Resources and return an owned DevIrq. Legacy static-provider callers
remain compatible. Both paths share construction, weak callback dispatch,
failed-acquisition reset and explicit stop ordering. The real model lifecycle
regression now acquires through Resources::request_irq_at rather than bypassing
the model with a test-global legacy provider.

This does not enable production startup or complete deferred IRQ-core hardware
acceptance. The next integration step must classify MMIO devices without
constructing a transport whose Drop resets hardware, then stage model discovery
until after releasing the legacy enumeration lock: add_device can auto-probe and
must run in sleepable context. Exclusive handoff must precede old device_desc_add;
the new path must never reset or register an already-owned legacy device.

Self-review added a native plain-virq preflight: reject a missing descriptor or
a conflicting specified trigger rather than silently using the core's legacy
fallback. Unknown trigger leaves configuration unchanged. This check does not
pin the mapping; its owner must keep it stable. Independent standards and
spec/logic review report no blocking findings on the final implementation.

The aarch64 guest selection
`kdma,kdriver,virtio,driver_os_api,driver_model,subsys_hwrng,entropy` passes
**275/275**, with coverage persisted and extracted. Evidence is local under
`r1-08-model-irq/ut-final.log`; QEMU used the worktree disk without network or
vsock. The first compile caught a missing Msi controller match; the unsupported
default-domain case is now explicit and tested. Tests cover pure adapter
conversion and error decisions, weak callbacks, reset-before-release,
release-before-pending-DMA cleanup, and model removal/failed-probe rollback with
old consumer handles retained. They do not validate a physical IRQ route.

`make clippy` and `make build` pass (`clippy.log`, `build.log`), as do targeted
format, header and whitespace checks. Existing third-party future-Rust warnings
remain. This slice is ready for a local commit; no push requested.
