# kdma — Design

## Purpose and scope

`kdma` owns host DMA constraints, translation and allocation state. All coherent
allocation, streaming mapping, synchronization and release entry points take a
`&DmaDeviceContext`. Host integration selects that context using the device
identified during probe. Reusable drivers consume the OS-neutral `DmaOp`
contract; they do not import `kdma`.

`device.rs` defines `DmaConstraints`, `DmaWindow` and `DmaDeviceContext`.
`dma.rs` implements coherent memory and active mapping bookkeeping;
`bounce_pool.rs` manages bounce suballocations. `lib.rs` exports the public
unsafe operations, `DmaBusAddress`, `DmaInfo` and the page-table provider hook.
`tests.rs` exercises constraints, offsets, ownership handoffs and cancellation.

## Constraint sources and architecture

```text
probe / host integration identifies device
    -> Arc<dyn DmaOp> bound on DeviceObject
        -> XKernelDmaProvider owns DmaDeviceContext
            -> immutable DmaConstraints + DmaWindow
            -> SpinNoIrq<DmaAllocator>
                 -> constrained page allocator + page-table/platform hooks
                 -> TLSF bounce pool + active mappings
```

`DmaConstraints` separates the streaming and coherent address masks and records
minimum bus alignment, maximum streaming segment length and segment boundary.
Masks consist of contiguous low bits; alignment is a power of two. Validation
checks the inclusive last byte using checked arithmetic. Streaming segments
must fit one boundary and one device segment limit.

`DmaWindow` records one contiguous physical-to-bus translation. Translation and
device reachability are separate checks: a valid translated address can exceed
the device mask. The allocation path intersects the window with the relevant
bus mask, translates that interval back into a physical range, and searches
free buddy blocks within the range. Legacy `p2b`/`b2p` retain the platform offset
conversion for compatibility; mapping decisions use the context's window.

`platform_default` constructs a distinct DMA32 context using `PHYS_BUS_OFFSET`.
Current VirtIO probe uses this policy for each hardware device. Probe may bind
a provider configured through `XKernelDmaProvider::new` beforehand. Firmware
`dma-ranges`, PCI capability negotiation, IOMMU and direct mapping are outside
this implementation. A device's context identity is carried explicitly; its
address capabilities remain supplied by trusted host integration.

## Execution context and concurrency

The page allocator, linear map and `DmaPageTableIf` implementation must be
initialized before use. Allocation and first streaming map can allocate pages,
change page tables and invoke platform prepare hooks; they belong in task
context. Map, sync and unmap use the context's IRQ-disabling spin lock and copy
the requested bytes while holding it. They require exclusive ownership handoff
and permit no sleeping or recursive acquisition of the same context.

Completion can recycle an existing bounce allocation in IRQ/softirq context
with the current host hooks. Coherent destruction and context teardown belong
in task context. This design removes cross-device mapping-table serialization;
page allocation and page-table changes still share their subsystem locks.
Copy cost and BTreeMap allocation mean submit/completion latency has no fixed
microsecond bound.

## Coherent allocation flow

1. Reject empty requests and derive the page-rounded extent.
2. Intersect the physical translation window with `coherent_mask`, then request
   pages through `kalloc::alloc_dma_pages_in_range`, bypassing per-CPU caches.
3. Validate translation for the entire page-rounded extent and apply
   `READ | WRITE | UNCACHED | SHARED` through `DmaPageTableIf::protect`.
4. Call `kplat::dma::prepare`, then check requested bus alignment and mask.
5. Return paired CPU and bus addresses. Callers initialize payload before
   hardware exposure; the VirtIO adapter zeroes all requested pages.

Release uses the original context and layout, invokes platform release and
restores normal read/write mapping attributes before returning pages. A failed
flag transition or platform hook retains pages outside the general allocator,
since their cache/share state is uncertain. `dma-trace` optionally records
coherent ownership and reports mismatched releases.

## Streaming state and algorithm

```text
caller owns initialized buffer
    -> map: allocate bounce, copy original bytes, record original arguments
    -> device owns bounce mapping
    -> sync for CPU: copy received bytes, retain allocation
    -> sync for device: copy transmitted bytes, retain allocation
    -> unmap after DMA stops: copy received bytes, remove record, recycle
```

Every streaming mapping uses bounce memory in this version. The first mapping
creates a 1024-page pool reachable through `streaming_mask`, independent of the
coherent mask. Each context pays this 4 MiB cost lazily. TLSF allocates padding
for final **bus address** alignment and boundary placement. The active record
keeps both the allocation base used for TLSF free and the aligned DMA pointer.

All directions copy initialized original bytes at map time. This preserves
untouched receive bytes when descriptor publication fails or hardware writes
only part of the buffer. Sync/unmap copy back for DeviceToDriver/Bidirectional;
DriverToDevice copyback is skipped. Sync for device refreshes outgoing data for
DriverToDevice/Bidirectional. Direction, original pointer, length and bus address
must match the active record in the same context.

Queue integration prepares buffer and indirect-table mappings transactionally
before descriptor publication. Preparation errors unmap earlier mappings and
retain queue capacity. Resource ownership retains the provider through cleanup.

## Errors and resource release

Invalid masks, alignment, window intervals, zero lengths, segment excess and
unreachable translated ranges yield `InvalidInput`. A window wholly above the
mask, unavailable pages, pool exhaustion or platform/flag failure yields
`NoMemory`. Unmap argument mismatch is an unsafe caller contract violation and
asserts; synchronization returns `InvalidInput` on mismatched records.

Dropping a context with no active streaming mappings releases pool backing.
If mappings remain, backing pages are retained to avoid reuse during unresolved
DMA. Dropping the context does not stop hardware or unmap original buffers.
Outstanding coherent allocations require explicit paired release before the
context is dropped. Arc-owned providers in coherent handles and VirtIO queues
carry that lifetime through cleanup.

## Design limits

Cache policy is uniformly uncached shared memory. Hardware coherency is not
inferred from a device boolean: cacheable direct mapping needs proven physical
contiguity, stable backing and platform cache/ownership rules. One window and
one segment are supported per mapping; segment overflow returns an error and
higher layers choose descriptor splitting. Boundary placement reserves up to
one boundary's padding, so large boundaries can exhaust the fixed pool.
A reachable window smaller than 4 MiB cannot provide the fixed streaming pool.
Device contexts separate software ownership; hardware isolation requires an
IOMMU or equivalent platform enforcement.

## TLB progress during lock contention

DMA page attributes are updated with IRQs disabled and can wait for remote TLB
acknowledgements. A CPU spinning for the same device allocator or the shared
kernel page-table lock must still acknowledge that request. Context lock wait
uses try_lock and DmaPageTableIf::poll_pending_tlb; the runtime provider applies
the same policy while acquiring the kernel layout lock. The provider pins the
CPU and excludes IRQ reentry while servicing only pending TLB invalidations.
It executes no arbitrary IPI callback, device callback or scheduler operation.
This avoids the DMA lock/shootdown wait cycle observed by x86_64 SMP tests.
