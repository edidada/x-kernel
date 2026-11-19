# kdma — Design

The bounce-pool guest fixture retains its original Vec backing without resizing
or relocating it after registration. The pool and its allocations are retired
before that buffer is released; no boxed-slice conversion is needed.

## Purpose

`kdma` is the kernel's DMA memory layer: it allocates coherent memory that
devices can read and write, maps existing CPU buffers into temporary DMA
transactions through a pooled coherent bounce area, and defines the bus
address type and physical↔bus conversion shared by all DMA-capable
drivers. It exposes two flat unsafe entry pairs —
`allocate_dma_memory`/`deallocate_dma_memory` and
`map_dma_buffer`/`unmap_dma_buffer` and consumes the MM-owned `kiface_mm_api::DmaMappingIf` capability for mapping
attribute transitions.

## Responsibilities

- Coherent allocation: `allocate_dma_memory(layout)` carves page-granular
  regions from the global DMA32 allocator (`UsageKind::Dma`), applies
  `READ | WRITE | UNCACHED | SHARED` mapping flags through
  `kiface_mm_api::DmaMappingIf::set_mode`, and registers the range with the platform
  (`kplat::dma::prepare`). Deallocation restores platform/mapping state before
  returning the original raw allocation to the allocator.
- Temporary mappings: `map_dma_buffer(buffer, direction)` stages the
  buffer through the coherent bounce pool (copy-in for every direction,
  including `DeviceToDriver`), tracks the mapping in `active_mappings` keyed
  by bus address, and fences (`SeqCst`) around the copies.
  `unmap_dma_buffer` copies data back (unless `DriverToDevice`), removes the
  tracking entry, and recycles the bounce buffer. `cancel_dma_mapping`
  releases a mapping the device never received without any copy-back, for
  rollback after a provider-side address check fails.
- Address translation: `p2b` / `b2p` over the linear
  `kbuild_config::PHYS_BUS_OFFSET` mapping, and `DmaBusAddress` as the
  `u64` bus address wrapper handed to devices.
- Result reporting: `DmaInfo { cpu_addr, bus_addr }` pairs the CPU view
  with the device view of each region.
- Bounce pool: a boot-prepared 1024-page coherent region sub-allocated
  by a TLSF byte allocator (`bounce_pool`), sized once from the same
  coherent path. The TLSF control block is const-initialized inside the
  global `DmaAllocator`; preparation registers the region in place instead
  of constructing and moving the multi-KiB allocator on the caller's stack.
  The optional region records this second phase under the existing allocator
  lock; a value-returning `Once<BouncePool>` initializer would still construct
  the large control block at runtime before publishing it.
- Mapping attributes: kiface-mm-api accepts only Coherent or Normal mode; memspace
  chooses the corresponding flags and owns page-table locking. DMA owns page
  allocation and platform sharing, not arbitrary page-table permissions.

## Non-Responsibilities

- No IOMMU management: bus addresses are direct physical+offset values;
  IOVA translation and IOMMU programming are out of scope.
- No cache maintenance beyond the chosen mapping attributes: coherence is
  achieved with `UNCACHED` mappings plus fences, not architecture cache
  flush calls.
- No streaming scatter-gather: `map_dma_buffer` handles one contiguous
  slice per mapping; descriptors chaining multiple buffers live in
  drivers.
- No device ownership policy: the crate cannot tell which device may
  access which bus address; that obligation stays with callers (see the
  `# Safety` contracts).

## Scope

```text
io/kdma/
├── src/
│   ├── lib.rs          # public unsafe API, DmaBusAddress, DmaInfo, p2b/b2p
│   ├── dma.rs          # DmaAllocator, coherent paths,
│   │                   # bounce mapping bookkeeping, dma-trace
│   └── bounce_pool.rs  # TLSF-backed coherent bounce pool
└── Cargo.toml
```

## Architecture and execution context

Coherent allocation/free and platform/mapping transitions run outside
`SpinNoIrq<DmaAllocator>` and require sleepable task context. Metadata, TLSF,
streaming copies and optional trace records retain the existing IRQ-safe lock.
`init_streaming_pool` prepares fixed backing before driver discovery; a missing
pool may be retried by a sleepable mapper after unlocking. IRQ/BH and IRQ-off
callers use prepared backing only. The pool never grows after publication.

## Concurrency Model

- Pool preparation runs outside the metadata lock, then rechecks publication
  under that lock; losing candidates are released after unlocking.
- Streaming map/unmap/cancel retain existing serialization, copies and SeqCst
  fences under the metadata lock. No streaming operation changes PTEs once
  backing exists, so it cannot wait for TLB completion while holding that lock.
- Coherent allocation stays private until platform preparation and synchronous
  mapping completion finish. Free returns pages only after device quiescence,
  platform release and Normal-mode restoration.

## Error Model

- `AllocResult` reports `NoMemory` for page, mapping or platform preparation
  failure. `InvalidInput` also rejects forbidden coherent/preparation contexts
  and missing-pool streaming maps from non-sleepable contexts. Sleepable task
  maps retry preparation once; capacity exhaustion of a ready pool never grows it.
- Panics mark contract violations: unmapping a non-active address,
  length mismatch between map and unmap, bounce recycling outside the
  initialized pool, and initializing an already registered pool.
- Failed platform release or Normal-mode restoration quarantines the owned
  pages rather than returning them to the allocator. Allocation failures after
  page allocation also retain the private pages; rollback is not assumed safe
  after a partial mapping or firmware transition.
- With `dma-trace` enabled, `deallocate_dma_memory` logs and skips mismatched
  frees instead of corrupting the allocator (without the feature no check is made) — the trace layer detects and
  skips duplicate or inconsistent records.

## External Boundary And Inputs

- Device-visible bus addresses are the trust boundary: the `# Safety`
  contracts require callers to expose an allocation's bus address only to
  devices allowed to DMA that buffer, and to stop device access before
  `unmap_dma_buffer`.
- The bounce pool stages all temporary mappings, so driver-owned buffers
  are never directly exposed to devices today; this bounds the blast
  radius of a mis-programmed device to the 1024-page coherent pool.
- Platform integration (`kplat::dma::prepare`/`release`) treats each
  page-granular coherent region as shareable; the `SHARED` flag keeps
  TEE/platform share-unshare hooks balanced per allocation.

## Unsafe Code

All unsafe surface is concentrated in five public functions and their
internal forwardings, each with `# Safety` sections and matching
`SAFETY:` comments:

- `allocate_dma_memory` / `deallocate_dma_memory` — exact-once free with
  identical layout; no forged or offset CPU addresses; bus addresses only
  to permitted devices.
- `map_dma_buffer` / `unmap_dma_buffer` — buffer stays live and
  exclusively owned between the calls; device access stops before
  unmap; exact-once unmap with the matching address and length.
- `cancel_dma_mapping` — the mapping is still unpublished (the device
  never received the address); exact-once cancellation with the matching
  address and length.
- Internal unsafe blocks: `NonNull::new_unchecked` on freshly allocated
  non-null pages, and `copy_nonoverlapping` between distinct live regions
  of validated length.

## Design Decisions

- Bounce-staged temporary mappings behind a stable API: staging through
  pooled coherent memory keeps driver code simple and safe today; a
  direct-map fast path can be reintroduced later without touching
  drivers. Staging is direction-independent so a mapping released without
  device access (queue-setup rollback) returns the original bytes for RX
  as well as TX; unpublished failure rollback uses cancellation instead of
  unmap so it never writes stale pool contents back.
- Page-granular coherent allocations: page-table attribute updates and
  platform prepare/release hooks stay balanced per allocation, avoiding
  partial-page flag aliasing between coherent and cached uses.
- `kiface_mm_api::DmaMappingIf` via `kiface`: the address-space owner implements flag
  updates as a provider, breaking the historical
  `kdma -> mm -> ... -> kdma` dependency cycle while keeping the kernel
  build platform-agnostic.
- Tracking map by bus address in a `BTreeMap`: double-mapping and
  mismatched unmaps are the classic DMA driver bugs; detecting them at
  the boundary turns silent memory corruption into a loud error or panic.
- `dma-trace` feature: coherent allocations record their call site and
  detect duplicate or mismatched frees at runtime, which is the cheapest
  way to debug DMA leaks without a full MMU tracer.

## DMA backing ownership

Coherent buffers retain the existing raw DMA32 allocator protocol, paired by the
original DmaInfo/Layout. Failed Coherent preparation attempts a Normal-mode
rollback before returning unpublished pages; a failed rollback or platform
preparation keeps them quarantined. Successful release returns pages only after
synchronous Normal-mode restoration. The streaming pool keeps its allocation
for kernel lifetime.

Coherent-free validation preserves its trace record until platform release and
Normal-mode restoration both succeed. A failed release keeps the original
allocation site and geometry visible alongside the quarantined pages; trace
and unittest bookkeeping are removed together only on successful teardown.

An unpublished coherent allocation whose Coherent transition fails attempts a
Normal-mode restore before returning its backing. Failure of that restore keeps
the pages quarantined. The synchronous `release_after_restore` boundary gates
both physical reuse and successful-free bookkeeping, including dma-trace.
