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
  regions through `kalloc::alloc_dma_pages_ref` (`UsageKind::Dma`), applies
  `READ | WRITE | UNCACHED | SHARED` mapping flags through
  `kiface_mm_api::DmaMappingIf::set_mode`, and registers the range with the platform
  (`kplat::dma::prepare`). `deallocate_dma_memory` restores the exact PageRef transfer, reverses platform/mapping preparation, then drops that hold.
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

## Architecture

```text
coherent allocation/free             streaming map/unmap
  | kalloc pages                       | use boot-prepared pool
  | DmaMappingIf::set_mode              | reserve/remove mapping under lock
  | platform prepare/release            | copy bytes outside lock
  | trace record under lock            | recycle bounce allocation under lock
  +-------------------- SpinNoIrq<DmaAllocator> ---------------------+
                           TLSF, active mappings, optional trace records
```

## Execution Context

- Coherent allocation and release require sleepable task context,
  after MM and CPU services are initialized. They synchronously change kernel
  mapping attributes. Allocation rejects an invalid context before taking any
  pages; an invalid release retains its pages instead of entering MM.
- `init_streaming_pool` prepares the fixed coherent pool after CPU services and
  before driver discovery/publication. Failure is logged without stopping boot;
  a later sleepable-task streaming map retries missing-pool preparation once
  outside the metadata lock. IRQ/BH or IRQ-off callers return an error instead
  of preparing mappings. A prepared pool never grows.
- Streaming map/unmap/cancel supports task, ordinary IRQ and BH contexts after
  preparation. These paths only suballocate, track, copy and recycle existing
  coherent memory. Only the missing-pool task path may prepare mappings; the
  ordinary IRQ/BH paths never request a TLB shootdown. They are not NMI-safe.
- Copying runs outside the metadata lock while retaining exclusive ownership of
  the reserved bounce extent. The static TLSF control block never moves through
  a runtime stack, including the IRQ-exit idle stack.
- No operation may hold a lock needed by a target CPU across a synchronous
  mapping transition. The optional dma-trace feature records coherent lifetimes.

## Concurrency Model

- `SpinNoIrq<DmaAllocator>` protects only TLSF suballocation, active mappings,
  pool publication, and optional coherent trace records. Coherent page
  allocation, mapping attribute transitions, platform hooks and buffer copies
  run outside it, so a metadata contender cannot block a shootdown owner.
- Explicit preparation allocates and prepares a private coherent candidate
  outside the lock, then rechecks and publishes under one lock acquisition.
  Concurrent losers release their unpublished candidates after unlocking.
  The large TLSF control block remains static and is initialized once in place.
- `BounceMapping` is the non-cloneable token for a reserved TLSF extent. Its
  fields stay private to the bounce module; recycling consumes the token.
  Safe slice copy methods own the staging/copy-back boundary and fences;
  `MaybeUninit<u8>` slices preserve RX initialization state. The public unsafe
  API only converts the caller's buffer contract into those borrowed slices.
- A reserved bounce allocation remains unavailable for reuse during copy-in.
  Only after staging finishes does the allocator publish it in `active_mappings`.
  Unmap removes its record under the lock, copies back while retaining exclusive
  ownership, and only then recycles it under the lock. No address is published
  to a device until copy-in and fences complete.
- `active_mappings` makes double-map of one bus address an error
  (`AllocError::InvalidInput`) and keeps `unmap` exact-match; unmapping an
  inactive address panics — that is a driver contract violation, not a
  runtime condition.
- Memory ordering: explicit `fence(SeqCst)` before device handoff and
  after device ownership returns, bracketing the bounce copies; UNCACHED
  attributes make the coherent window itself uncached for the device.

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

Coherent backing uses the existing metadata-owned `PageRef` range protocol,
including DMA32 validation and exact-page admission. The legacy DmaInfo/layout
API transports one `into_raw` transfer; matching release uses `from_raw` exactly
once and drops the restored hold after platform release and confirmed mapping
restoration. It never calls raw `dealloc_dma_pages` on referenced storage. The
boot-prepared pool retains its backing transfer for kernel lifetime. Preparation
or completion failures retain the existing hold rather than returning pages
with uncertain device/platform/mapping state.

Raw direct-map PTEs remain access aliases, not permanent owners of every RAM
page. Managed mappings elsewhere keep their existing PTE-owned references and
RetiredReferences. No duplicate ownership index or counter is added here.
