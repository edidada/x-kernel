# driver-os-api-xkernel — Security And Reliability

## Trust and ownership

This host-only adapter implements driver-os-api using memspace, kdma, kalloc,
KIRQ and the host clock. It does not own device lifecycle state, install a
global provider or stop hardware. The caller must stop device DMA and interrupt
generation before resource cleanup and obey the original allocation/token
pairing contracts.

## Clock register access

`IoMapClkReg` is the only clock-facing path to device registers. It holds an
`IoMap`, so every register read/write is bounds-checked against the mapped
region and rejected when misaligned, the accesses carry the mapping's
acquire/release fences, and the mapping is released when the last clock node
holding the `Arc` goes away. The adapter reports `region.size` through
`window_bytes`, which is what lets the clock framework reject a table entry that
would reach past the mapping instead of faulting at first access. A window that
is mapped too small is therefore a probe-time `ClkError::InvalidValue`, not
memory corruption.

## DMA allocation

`alloc_coherent` selects the host's cacheable allocation path when the caller
asked for `DmaCoherency::Manual`. Those pages stay cacheable and are shared
with userspace; the provider performs no cache maintenance. A caller that
skips `flush_cache` before a device reads, or `invalidate_cache` before
reading device writes, observes stale data — a correctness failure, not a
crash. `Host` remains the default and keeps the non-cacheable mapping
contract. Both allocation paths share one deallocation, which restores the
Normal mapping and is a no-op for the cacheable one.

### Unsafe allocation boundary

`alloc_coherent` makes one unsafe call per `DmaCoherency` arm into `kdma`
(`allocate_dma_memory`, or `allocate_dma_memory_cacheable` for `Manual`). The
preconditions and the code or contract that establishes each are:

- **Valid layout**: `layout` is built by
  `Layout::from_size_align(spec.len, spec.align)` in the same function and
  rejected with `InvalidResource` on failure, so the allocator receives a
  valid, non-zero layout;
- **Exclusive ownership**: the returned pages are owned exclusively by the
  `CoherentMem` handle that receives the `DmaAllocation`;
  `free_coherent` reconstructs the same layout from `alloc.spec` and calls
  `deallocate_dma_memory` exactly once when that handle drops;
- **Manual cache maintenance**: `DmaCoherency::Manual` assigns the
  requesting driver responsibility for `flush_cache` before any device
  reads and `invalidate_cache` before reading device writes. The provider
  performs no maintenance; `Host` allocations stay non-cacheable and keep
  the coherent contract;
- **Bus-address exposure**: the address leaves this provider only inside the
  `DmaAllocation` returned to the requesting driver. That driver is trusted
  kernel code and owns device admission, so the address never reaches an
  unprivileged caller through this API.

The same requirements are restated in `allocate_dma_memory_cacheable`'s
rustdoc; this section records them for the provider's unsafe boundary.

## IRQ teardown

IRQ resources must refer to descriptors prepared by mapping, MSI-X allocation
or fixed-source setup. KIRQ rejects a missing descriptor before publishing
handler ownership; the adapter does not reconstruct one from a raw IRQ number.

Registration may invoke the callback before returning. KIRQ owns dispatch,
mapping validation and synchronization. Explicit route domains are preserved;
plain virtual IRQs are not reinterpreted as hardware numbers.

`map_fw_irq` passes the untrusted firmware binding cells to the controller's
registered translator by exact controller-node identity. The adapter does not
guess a controller family from compatible strings and does not fall back to the
raw cell as a logical IRQ. Malformed, unsupported, out-of-range, not-ready and
conflicting declarations remain distinct failures; the returned resource is
KIRQ's stored descriptor, so later registration cannot silently replace its
trigger or ownership metadata.

Action release requires sleepable context without locks needed by the handler.
Both KIRQ errors and missing actions fail closed rather than returning permission
to reclaim callback resources. This deliberately strengthens mainline's missing
action warning behavior. No shared peer is removed as a fallback.
Tests cover the result classifier without inducing a kernel panic.

MSI reservation and IRQ action lifetimes remain distinct. `try_free_msix` returns
the unchanged resource on rejection without release side effects. The x86 core
bool cannot distinguish context, identity or live-use rejection: all map to Busy,
not an exact diagnosis or a promise of eventual success. Non-x86 release returns
Unsupported with ownership intact. The inherited terminal `free_msix` wrapper
panics on rejection instead of discarding the resource and continuing cleanup.

The owner must stop message production, order outstanding messages, detach/drain
actions, logically disable the line and serialize owner operations. Release needs
a watched, sleepable task and running RCU service, with no spinlocks, IRQ/preemption
exclusion or RCU read scope. Neither action drain nor allocation-free MSI deletion
proves device-side message quiescence or DMA stop. Returned ownership enables an
explicit retry only while an owner retains it; no automatic retry, recoverable
Drop, complete dynamic removal or new threaded IRQ support is implemented here.

## Memory boundaries

MMIO mappings hold the acquisition returned by memspace; unsafe unmap requires
that exact live token and no remaining accesses. This is a mapping reference,
not proof of exclusive physical-range ownership.

Exclusive physical-range ownership is a separate contract. Claim entries are
kept under one task-context mutex with no IRQ-handler accessor; a claim checks
overlap, reserves capacity and inserts in one critical section, and every
failure path leaves the table unchanged. Release matches the opaque token
exactly, so an unknown or stale token cannot release another owner's range;
such a token logs a warning and is otherwise ignored. The table is
process-wide but contract-scoped: code that never claims, and mappings made through other
entry points, are not excluded. It is not a system-wide MMIO ownership proof.

CPU storage uses the fallible allocator and original pointer/Layout release;
it performs no DMA sharing or page-attribute conversion. Existing memory tests
exercise sizes, alignment and initialization.

DMA retains the mainline DmaOp implementation and its caller obligations:
coherent metadata must be paired exactly, streaming buffers must remain valid,
and device access must end before release. DeviceDma adds one bounded,
device-scoped view: noncoherent devices are rejected rather than defaulted,
every coherent allocation is checked over its page-rounded span against both
the window and mask, and streaming guards retain the native kdma release token
so the translated address cannot orphan its bounce mapping. It does not add
IOMMU support, multiple windows, cache maintenance for noncoherent devices or
hardware validation. The existing zero-bus streaming-release shortcut on the
legacy DmaOp path is unchanged and is not certified as a general DMA-address
rule. Time forwarding retains the host monotonic-clock assumption.

## DMA and MMIO execution context

Coherent DMA allocation/free and runtime MMIO map/unmap require sleepable task
context; invalid teardown retains its allocation/mapping. The host prepares the
fixed streaming pool before driver publication. Streaming map/unmap can then
run in ordinary IRQ/BH context without PTE changes or TLB waits; NMI is excluded.
