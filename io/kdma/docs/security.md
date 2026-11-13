# kdma — Security And Reliability

## Scope

This analysis covers the entire crate: `src/lib.rs` (public unsafe API
and address translation), `src/dma.rs` (`DmaAllocator`, mapping
bookkeeping, `kiface_mm_api::DmaMappingIf`), and `src/bounce_pool.rs` (the TLSF-backed
coherent pool). No modules are excluded; the private modules are reachable
only through the four public unsafe entry points audited below.

## Trust Model

The crate guards the kernel-device memory boundary: it decides which
kernel memory becomes device-visible and for how long. Drivers are
trusted kernel components, but the crate is designed so that driver
mistakes (double map, mismatched unmap, wrong layout) surface as loud
errors or panics instead of silent corruption. Devices are untrusted
writers within the regions explicitly granted to them — and today, all
temporary grants are confined to the bounce pool.

## External Boundaries

- **Device-visible memory**: bus addresses returned by
  `allocate_dma_memory` (coherent regions) and `map_dma_buffer`
  (bounce copies). Devices are allowed to DMA exactly those ranges;
  the `# Safety` contracts put enforcement on callers.
- **The bounce pool**: a 1024-page coherent region sub-allocated by
  TLSF. Driver buffers are never directly device-visible on the mapping
  path, bounding the blast radius of a mis-programmed device to the
  pool. Its TLSF control block is const-initialized in the global DMA
  allocator. Boot preparation registers coherent backing memory in place under
  the same lock; failed coherent allocation leaves the pool uninitialized
  and retryable during explicit preparation. No runtime path constructs or moves the large control
  block on the caller's stack, including IRQ-exit softirqs on an idle stack.
- **Platform hooks** (`kplat::dma::prepare`/`release`): called with
  page-granular ranges for share/unshare bookkeeping; a hook failure
  fails the allocation (`AllocError::NoMemory`), never proceeds
  silently.

### External Input Entries

All raw-pointer inputs enter through exactly four public unsafe functions
in `src/lib.rs`; every parameter is kernel-caller supplied (there are no
device callbacks, user pointers, or network/file inputs in this crate):

| Entry | Pointer-bearing parameters | Source | Checked properties and failure results |
|---|---|---|---|
| `allocate_dma_memory(layout)` | none (size/align only) | driver, at probe/init | page allocation, flag update, platform `prepare` must succeed → `AllocError::NoMemory` |
| `deallocate_dma_memory(dma, layout)` | `DmaInfo.cpu_addr: NonNull<u8>` | returned by a prior `allocate_dma_memory` | exact-once, layout must match; mismatched frees detected and skipped under `dma-trace` |
| `map_dma_buffer(buffer, direction)` | `buffer: NonNull<[u8]>` | driver-owned kernel buffer (TX/RX staging) | non-zero length; bus address not already active → `AllocError::InvalidInput`; pool capacity → `AllocError::NoMemory` |
| `unmap_dma_buffer(dma_addr, buffer, direction)` | `dma_addr: DmaBusAddress`, `buffer: NonNull<[u8]>` | must match a prior `map_dma_buffer` result | mapping must be active and length must match — panics otherwise |

A production call site exercising `allocate_dma_memory` /
`deallocate_dma_memory` is the driver OS API provider adapter
`drivers/adapters/xkernel/provider/src/dma.rs`
(`alloc_coherent` / `free_coherent`), which drivers reach through the
`driver_os_api` `DmaOp` contract (for example `CoherentMem`).

Devices never pass pointers into this crate; they only read/write the
ranges the drivers granted (see Threat T-02).

## Unsafe Code

Concentrated in five public unsafe functions plus internal forwardings,
each with `# Safety` sections and `SAFETY:` comments:

- `allocate_dma_memory(layout)` — hands out coherent, device-reachable
  memory. Caller obligations: deallocate exactly once with the same
  layout, never forge or offset the CPU address, expose the bus address
  only to devices permitted to DMA it.
- `deallocate_dma_memory(dma, layout)` — matching release; the layout
  must match the allocation.
- `map_dma_buffer(buffer, direction)` — stages the buffer through the
  bounce pool for every direction, preserving RX initialization state in the
  bounce copy. Caller obligations: buffer stays live and exclusively
  owned until `unmap_dma_buffer`, device access stops before unmap.
- `unmap_dma_buffer(dma_addr, buffer, direction)` — exact-once, matching
  address and length.
- `cancel_dma_mapping(dma_addr, len)` — releases a mapping that was never
  published to a device, without copying bounce contents back. Caller
  obligations: exact live mapping, cancelled once, device never received
  the address.
- Internal blocks: `NonNull::new_unchecked` on freshly allocated (never
  null) pages; the bounce module owns `copy_nonoverlapping` between distinct
  live regions after matching slice lengths; TLSF pool deallocation with a containment
  assert.

## Protected Resources

- `active_mappings: BTreeMap<u64, BounceMapping>` — the single source of
  truth for in-flight mappings; keyed by bus address, guarded by the
  global `SpinNoIrq`.
- The coherent region bookkeeping: `dma-trace` records every coherent
  allocation with its call site and rejects duplicate records; mismatched
  frees are detected and skipped rather than corrupting the allocator.
- Page-table attributes: coherent pages are mapped
  `READ | WRITE | UNCACHED | SHARED`; on release the flags are restored
  before the pages return to the DMA allocator, so cached-vs-coherent
  aliasing cannot occur.

## Invariants

- A bus address is mapped at most once at a time; a duplicate
  `map_dma_buffer` is rejected (`AllocError::InvalidInput`) and the
  bounce buffer recycled.
- `unmap_dma_buffer` asserts the length matches the recorded mapping;
  unmapping an unknown address panics (contract violation, not a runtime
  condition).
- Coherent allocations are page-granular, so attribute updates and
  platform prepare/release hooks always cover whole pages.
- Copies are bracketed by `fence(SeqCst)` before device handoff and after
  device ownership returns.

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | Driver exposes a coherent bus address to a device that must not read it | Medium — kernel data disclosure to a device | Caller violates the `# Safety` contract | Contract documentation; allocation is the only path to device-reachable memory and is explicit. Residual risk accepted at the driver trust level. |
| T-02 | Device DMA targeting memory outside its grant | High — kernel memory corruption | Mis-programmed device or descriptor | Coherent grants are page-exact (bounce grants are byte-granular TLSF sub-allocations sharing the pool) and devices only learn the bounce/coherent addresses the driver programmed; cannot defend against arbitrary device DMA (no IOMMU here) — recorded as the core residual risk of the crate. |
| T-03 | Double map / mismatched unmap by a driver | Medium — aliasing or allocator corruption | Driver bookkeeping bugs | Public `map_dma_buffer` (`io/kdma/src/lib.rs`) rejects a duplicate bus address via the `active_mappings` lookup in `DmaAllocator::reserve_mapping` (`io/kdma/src/dma.rs`, `contains_key` check → `AllocError::InvalidInput`); public `unmap_dma_buffer` panics on a `remove()` miss in `DmaAllocator::take_mapping` and `assert_eq!`s the recorded length; `bounce_pool::BouncePool::deallocate` (`io/kdma/src/bounce_pool.rs`) asserts pool containment. |
| T-04 | Bounce buffer retention after unmap failure | Low — stale kernel data readable by device | Caller forgets to unmap before dropping | RAII does not span map/unmap; the `active_mappings` table (`DmaAllocator.active_mappings`, keyed by bus address) keeps every leaked mapping enumerable, and the `dma-trace` feature (`trace_coherent_alloc`/`trace_coherent_free` in `src/dma.rs`) logs allocation sites and flags duplicate or mismatched coherent frees so leaks can be located in trace builds. |
| T-05 | Uncached mapping left cached on free | Medium — cache-coherency corruption on reuse | Attribute-restore failure ignored | Release/restore failures are logged and the pages are quarantined; they are not returned even to the DMA-only pool. |

## Failure Modes

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | Page allocation or flag update fails | `AllocError::NoMemory` | Driver init/probe fails | 3 | Checked allocation; privately owned pages are retained if a mapping transition fails, preventing unsafe reuse. |
| F-02 | Platform `prepare`/`release` hook fails | `NoMemory` on alloc; release logs and continues | Share/unshare accounting skew on TEE platforms | 3 | Allocation aborts; failed release is logged and pages are quarantined. |
| F-03 | Zero-length map request | `AllocError::InvalidInput` | Caller fixes the request | 4 | Explicit input validation. |
| F-04 | Unmap of an inactive address | Panic | Kernel aborts the offending path | 2 | Contract violation — loud failure preferred over silent corruption. |

## Known Limitations

- No IOMMU: the crate cannot prevent a device from DMA-ing outside its
  grant; isolation depends on hardware and descriptor correctness.
- Temporary mappings always copy (bounce); large transfers pay the copy
  cost by design.
- The bounce pool is fixed at 1024 pages; concurrent mappings beyond its
  capacity fail with `NoMemory` rather than growing.

## Audit Checklist

- Every new unsafe block cites the mapping table or containment assert it
  relies on.
- Attribute updates remain page-granular and paired with platform hooks.
- The `active_mappings` map remains the only place bus addresses are
  handed out for temporary mappings.

## DMA metadata and TLB completion

The DMA metadata spinlock never spans page allocation, mapping transitions,
platform prepare/release, or buffer copies. Coherent allocations are private
until initialization completes. The host prepares bounce-pool candidates before driver publication, outside
the lock; publication is rechecked and unpublished losers are released outside it.
Preparation failure is logged and leaves streaming DMA unavailable; it does not
make unrelated boot services depend on a contiguous DMA32 allocation. Streaming
maps in sleepable task context retry missing-pool preparation once after releasing
the metadata lock. IRQ/BH, IRQ-off and other non-sleepable callers return an error
without entering MM. A ready pool never grows on capacity exhaustion.
Coherent allocation rejects IRQ-off/interrupt/BH callers before allocating pages.
A forbidden coherent release retains its pages and reports the contract violation;
it cannot restore mappings or safely reuse the range in that context.
The bounce module alone constructs a non-cloneable `BounceMapping` token from a
TLSF allocation. Safe copy methods validate equal slice lengths, preserve
initialization state through `MaybeUninit<u8>` and centralize the raw copies and
fences. Map retains this token outside the metadata lock until staging finishes,
then transfers it into `active_mappings`; recycling consumes it exactly once.
Removing a streaming mapping transfers exclusive ownership to the unmap path;
recycling happens only after copy-back finishes. These ownership transitions
prevent another mapper from reusing a bounce buffer during an unlocked copy.
An incomplete remote invalidation must never authorize page reuse; the TLB
provider returns an error when completion cannot be established. DMA mode
changes use the explicit `try_finish` boundary; errors retain the existing PageRef hold and
withhold publication/reuse. Firmware release failures also retain the pages.

Coherent allocation admits DMA32 pages through kalloc's existing PageRef path.
DmaInfo plus its original Layout carries one exact `into_raw` transfer; the unsafe
matching free restores that pair with `PageRef::from_raw`. Its source is the
original transfer, never an address lookup that fabricates a hold. Platform or
mapping errors retain this existing hold. Final successful release uses PageRef
Drop; mixing it with raw dealloc_dma_pages would violate the allocator contract.
