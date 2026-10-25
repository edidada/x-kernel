# Instance DMA backend

The starting revision is `5464361169822b3dbd3956017083bf8bbf7a7c2e`
(kvirtiodrivers 0.13.5). This change belongs to this library, not to a
consumer's Cargo registry cache.

## Ownership and compatibility

`DmaBackend` owns one device's DMA address space and policy. Its clones refer
to the same instance. Allocation and mapping return `Result`; a failed call
must retain no resources. Successful addresses may be zero. Release uses the
same backend and original arguments, and does not return a recoverable error.
The provider is responsible for address constraints, cache synchronization,
and its execution-context requirements. No global current-device slot or TLS
is introduced.

`StaticHal<H>` forwards to the existing static `Hal` methods. It preserves
the old zero-address allocation failure sentinel and deallocation assertion.
It does not require `H: Clone`, `Send`, or `Sync` merely to construct the
adapter. The original MMIO translation contract stays on `Hal`.

`Dma<H, B = StaticHal<H>>` retains its backend through destruction, together
with the allocation's direction and access-platform setting. Invalid page
counts are rejected before calling the backend. Its Send/Sync implementations
require the corresponding property of the retained backend.

## Safety boundary

The backend is an unsafe implementation contract. It must supply valid,
page-aligned, zeroed and exclusive storage, keep the address space alive, and
honour acquire/release and mapping/cache-maintenance pairing. Returning an
error after retaining a mapping violates that contract. Clone must not switch
device domains. Backend destruction must not replace explicit hardware stop.

Ring allocations must remain coherent for their entire lifetime. The queue
performs ordering barriers, not ring cache maintenance; a non-coherent host
must provide a suitable coherent mapping. A one-time allocation flush is not
enough. Streaming buffer mappings instead use share/unshare for ownership
transfer and required synchronization or bounce-buffer copy-back.

Dropping `Dma` only releases memory; callers must already have stopped device
access. The new backend does not make dropping an active queue safe.

## Current delivery boundary

`VirtQueue<H, SIZE, B = StaticHal<H>>` and
`VirtIORng<H, T, B = StaticHal<H>>` now provide `new_with_dma` constructors.
The old `new` constructors retain their call shapes and select `StaticHal`.
Legacy and modern ring allocations retain clones of the supplied instance;
direct buffers, indirect table mappings and RNG requests use that same backend.
The indirect table's CPU storage remains a library-owned Box, mapped and
unmapped through the instance rather than a global address translator.

Submission validates buffer sizes before acquiring mappings. A direct-chain
failure unmaps the successful prefix without advancing the free-list head or
available index. An indirect failure unmaps its successful buffer prefix and
frees the table; ownership transfers to the queue only after mapping the table
succeeds. Neither failure publishes the request to the device. Completion
unmaps through the same backend with original direction and access policy.
If modern-ring allocation fails halfway, the first allocation is released by
its owner before any transport queue configuration.

R1-06 delivers the library mechanism and simulated instance-policy isolation,
not the real host DMA adapter (R1-08). The kernel now selects
this vendored source through its root Cargo patch while retaining the 0.13.5
version requirement. Provenance and update policy are in `../README.md`
relative to the library root. Hardware stop/cancellation is R1-07, not
provided implicitly by this allocation/mapping change.

## Validation

With nightly-2026-03-08:

- `cargo test --all-features`: 70 unit tests and 6 doctests pass.
- `cargo test --no-default-features`: 34 unit tests and 4 doctests pass.
- The three new allocation tests cover distinct instances (including device
  address zero), allocation failure without release, and invalid sizes.
- Queue tests cover both concurrent instance domains, both ring layouts,
  complete legacy-MMIO queue construction/recycling, each direct/indirect
  mapping failure including the indirect table, modern-ring allocation failure,
  queue reuse after failure, and RNG failure/retry/completion with copy-back.
- Policy fixtures check translated ring addresses and descriptor mapping
  ownership, one device's mask rejection without affecting another device,
  full-allocation range rejection with cleanup, and per-instance coherent vs
  non-coherent synchronization callback counts for direct and indirect chains.
- Disabling direct-prefix unmapping makes its rollback test fail on a retained
  mapping. The injected fault was restored before final passing tests.
- Strict all-target Clippy fails in untouched library/test code. No claim of
  a clean all-target lint run or complete kernel acceptance is made.
- `cargo clippy --all-features` succeeds with two warnings in untouched code.
- After vendoring and rebasing the kernel onto main `240fcbfd1`, the aarch64
  QEMU defconfig passes `make build`, `make clippy` and the combined
  driver_model/kdriver/subsys_hwrng/entropy guest suite (161 passed, 0 failed,
  SMP4, networking and vsock disabled). The build log selects this vendor path.
  This verifies compatibility of existing kernel callers, not new-model host
  DMA constraints or hardware stop/cancellation.

The queue fixtures use distinct translated address offsets and per-instance
ledgers, not real hardware. Synchronization counts prove calls reach the right
instance, not that real CPU caches are correctly maintained. Real host
mask/coherency handling and hardware teardown remain outside this evidence.
Two-axis review found no remaining blocking issue in R1-06; the initial RNG
and full legacy-transport test gaps have been addressed. No new stop/cancel
guarantee is implied by this delivery.
