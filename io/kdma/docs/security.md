# kdma — Security And Reliability

## Scope

This analysis covers the entire crate: `src/lib.rs` (public unsafe API
and address translation), `src/dma.rs` (`DmaAllocator`, mapping
bookkeeping, `DmaPageTableIf`), and `src/bounce_pool.rs` (the TLSF-backed
coherent pool). No modules are excluded; the private modules are reachable
through the public unsafe operations audited below. `src/device.rs` validates masks.

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
  path. Devices that ignore programmed descriptors can still access
  memory outside the pool without hardware isolation.
- **Platform hooks** (`kplat::dma::prepare`/`release`): called with
  page-granular ranges for share/unshare bookkeeping; a hook failure
  fails the allocation (`AllocError::NoMemory`), never proceeds
  silently.

### External Input Entries

All raw-pointer inputs enter through the public unsafe functions
in `src/lib.rs`; every parameter is kernel-caller supplied (there are no
device callbacks, user pointers, or network/file inputs in this crate):

| Entry | Pointer-bearing parameters | Source | Checked properties and failure results |
|---|---|---|---|
| `allocate_dma_memory(device, layout)` | none (size/align only) | driver, at probe/init | page allocation, flag update, platform `prepare` must succeed → `AllocError::NoMemory` |
| `deallocate_dma_memory(device, dma, layout)` | `DmaInfo.cpu_addr: NonNull<u8>` | returned by a prior `allocate_dma_memory` | exact-once, layout must match; mismatched frees detected and skipped under `dma-trace` |
| `map_dma_buffer(device, buffer, direction)` | `buffer: NonNull<[u8]>` | driver-owned kernel buffer (TX/RX staging) | non-zero length; bus address not already active → `AllocError::InvalidInput`; pool capacity → `AllocError::NoMemory` |
| `unmap_dma_buffer(device, dma_addr, buffer, direction)` | `dma_addr: DmaBusAddress`, `buffer: NonNull<[u8]>` | must match a prior `map_dma_buffer` result | mapping must be active and length must match — panics otherwise |

A production call site exercising `allocate_dma_memory` /
`deallocate_dma_memory` is the device-res provider adapter
`drivers/adapters/xkernel/device-res/src/dma.rs`
(`alloc_coherent` / `free_coherent`), which drivers reach through the
`DmaCoherent` contract.

Devices never pass pointers into this crate; they only read/write the
ranges the drivers granted (see Threat T-02).

## Unsafe Code

Concentrated in public unsafe functions plus internal forwardings,
each with `# Safety` sections and `SAFETY:` comments:

- `allocate_dma_memory(device, layout)` — hands out coherent, device-reachable
  memory. Caller obligations: deallocate exactly once with the same
  layout, never forge or offset the CPU address, expose the bus address
  only to devices permitted to DMA it.
- `deallocate_dma_memory(device, dma, layout)` — matching release; the layout
  must match the allocation.
- `map_dma_buffer(device, buffer, direction)` — stages the buffer through the
  bounce pool. Caller obligations: buffer stays initialized, live and exclusively
  owned until `unmap_dma_buffer`, device access stops before unmap.
- `unmap_dma_buffer(device, dma_addr, buffer, direction)` — exact-once, matching
  address and length.
- Internal blocks: `NonNull::new_unchecked` on freshly allocated (never
  null) pages; `copy_nonoverlapping` between distinct live regions of
  caller-declared length; TLSF pool deallocation with a containment
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
  before the pages return to the general allocator, so cached-vs-coherent
  aliasing cannot occur.

## Invariants

- A bus address is mapped at most once at a time; a duplicate
  `map_dma_buffer` is rejected (`AllocError::InvalidInput`) and the
  bounce buffer recycled.
- `unmap_dma_buffer` asserts the original CPU pointer, direction and length match the recorded mapping;
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
| T-02 | Device DMA targeting memory outside its grant | High — kernel memory corruption | Mis-programmed device or descriptor | Grant sizes are page-exact and devices only learn the bounce/coherent addresses the driver programmed; cannot defend against arbitrary device DMA (no IOMMU here) — recorded as the core residual risk of the crate. |
| T-03 | Double map / mismatched unmap by a driver | Medium — aliasing or allocator corruption | Driver bookkeeping bugs | Public `map_dma_buffer` (`io/kdma/src/lib.rs`) rejects a duplicate bus address via the `active_mappings` lookup in `DmaAllocator::map_dma_buffer` (`io/kdma/src/dma.rs`, `contains_key` check → `AllocError::InvalidInput`); public `unmap_dma_buffer` panics on a `remove()` miss in `DmaAllocator::unmap_dma_buffer` and `assert_eq!`s the recorded length; `bounce_pool::BouncePool::deallocate` (`io/kdma/src/bounce_pool.rs`) asserts pool containment. |
| T-04 | Bounce buffer retention after unmap failure | Low — stale kernel data readable by device | Caller forgets to unmap before dropping | RAII does not span map/unmap; the `active_mappings` table (`DmaAllocator.active_mappings`, keyed by bus address) keeps every leaked mapping enumerable, and the `dma-trace` feature (`trace_coherent_alloc`/`trace_coherent_free` in `src/dma.rs`) logs allocation sites and flags duplicate or mismatched coherent frees so leaks can be located in trace builds. |
| T-05 | Uncached mapping left cached on free | Medium — cache-coherency corruption on reuse | Attribute-restore failure ignored | Failed release or attribute restoration retains the pages; memory with uncertain attributes never returns to the general allocator. |

## Failure Modes

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | Page allocation or flag update fails | `AllocError::NoMemory` | Driver init/probe fails | 3 | Allocation failure propagates; uncertain attributes after hook failure quarantine pages. |
| F-02 | Platform `prepare`/`release` hook fails | `NoMemory` on alloc; failed release retains pages | Share/unshare accounting skew on TEE platforms | 3 | Allocation aborts; failed release quarantines pages. |
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

## Device address policy

Every DMA operation receives `&DmaDeviceContext`. The context holds immutable
streaming and coherent masks; probe owns the device/provider association. The
backend, active mapping table and lazy 4 MiB bounce pool remain shared across
all current contexts using the platform offset and uncached/shared attributes.
The default context preserves the existing unrestricted address policy; a
restricted device declares its masks explicitly through `new`.

Allocation checks the whole page-rounded bus interval against the coherent
mask and checks bus alignment. Streaming checks the complete bounce interval
against the streaming mask before copying or publishing it. A rejected candidate
is released. This backend offers no low-address allocation guarantee: an
unreachable candidate returns `InvalidInput` even when other reachable memory
exists. Address translation uses checked arithmetic before changing page flags.

`sync_dma_buffer` retains the mapping while copying at CPU/device ownership
handoffs. Sync and unmap validate the original pointer, length and direction.
The caller must use the original device/provider and stop the previous owner;
mask values are policy, not a unique identity or hardware isolation boundary.
Contexts own no allocator, pool or teardown work. Dropping a context does not
cancel DMA or release outstanding mappings; retained providers and driver
cleanup keep the original resources live until hardware access has stopped.

The shared allocator lock wait and runtime page-table lock wait service pending
TLB acknowledgements through `DmaPageTableIf::poll_pending_tlb`. This preserves
progress when a lock owner changes page attributes with IRQs disabled. The hook
pins the CPU and handles only TLB invalidations, not arbitrary IPI callbacks.

Direct mapping, device cache-coherency selection, segment constraints,
firmware DMA ranges, IOMMU and restricted physical allocation remain separate
features. Tests cover mask overflow, distinct masks sharing one backend,
rejected-map rollback, synchronization and receive cancellation.
