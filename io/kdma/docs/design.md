# kdma — Design

## Purpose and scope

`kdma` owns coherent allocation, streaming mapping, backend selection, cache
handoff and software mapping lifetime. Current devices share one platform DMA
domain, physical-to-bus offset, page allocator and backend lock. Mainline VirtIO
uses its static HAL and needs no transport/queue context extension for this domain.

`mapping.rs` exposes distinct allocation and streaming tokens. `dma.rs` owns
shared state and page-table hooks; `cache.rs` defines cache capabilities and
operations; `bounce_pool.rs` owns TLSF staging storage. `device.rs` retains the
existing immutable mask policy for flat APIs. Masks do not identify a domain.

## API and lifetimes

`CoherentAllocation::allocate(layout)` returns persistent, zero-initialized,
physically contiguous memory with separate CPU and bus addresses. Its complete
page-rounded extent uses `READ | WRITE | UNCACHED | SHARED`. CPU/device cache
consistency does not require streaming sync, but publication order and device
coordination still belong to callers. The token retains its original layout.
`free(self)` requires stopped hardware and retired aliases, including any
streaming mappings of those pages.

`StreamingMapping::map(buffer, direction, policy)` maps existing initialized,
pinned kernel RAM with stable attributes. It retains the original arguments and
reports `Direct` or `Bounce`. `cpu_addr()` always identifies the original buffer;
`bus_addr()` identifies the device view. Mapping starts with device ownership.
`sync_for_cpu` and `sync_for_device` complete an ownership handoff without changing
the DMA address. `unmap(self)` returns ownership and releases mapping resources;
a failed handoff returns the error and original token for retry.

Tokens are not Copy/Clone. They expose raw addresses rather than references whose
validity would conflict with asynchronous hardware ownership. Dropping a token
retains storage and bookkeeping; it cannot infer that DMA has stopped. The caller
must keep mapped original RAM live until a successful unmap even after a token
is lost. Explicit cleanup makes this hardware-quiescence obligation visible.

The existing flat allocate/free/map/sync/unmap APIs use the same backend. Flat
unmap preserves its void ABI and asserts successful matching release; the typed
API provides recoverable unmap. `DmaInfo.cpu_addr` is the DMA storage CPU view,
which is bounce storage when staging is selected. It is separate from a typed
streaming token's original CPU address.

## Architecture

```text
allocation token                   streaming token
       |                                  |
       +---------- shared backend --------+
                       |
          +------------+-------------+
          |                          |
  coherent page allocation     Auto / Direct / Bounce
  zero rounded extent               |
  cache attribute handoff     query contiguous RAM
  UNCACHED | SHARED           checked full bus interval
  platform prepare            cache isolation/capability
          |                    /                  \
          |              original RAM         coherent TLSF pool
          |                    \                  /
          +------------ active mapping records --+
                              |
                    CPU <--- sync ---> Device
```

## Coherent flow

The allocator obtains contiguous DMA32 pages through the existing kalloc API.
It checks physical-plus-offset arithmetic and the complete extent, initializes
all bytes including padding, completes cache clean/invalidate where the host
supports it, updates page attributes and calls `kplat::dma::prepare`. Allocation
then checks the coherent mask and bus alignment. Rejected candidates are released
with their exact layout. This allocator rejects unsuitable candidates and provides
no search or reserved-pool guarantee for arbitrary low-address masks.

Release completes cache maintenance while the current shared mapping is still
accessible, calls the platform release hook, restores cached attributes and
returns pages to kalloc. Maintenance precedes any platform unshare which can
revoke access through the old shared mapping. Failure during
attribute/share transitions quarantines the pages instead of reusing uncertain
state. `dma-trace` optionally records allocation sites and detects mismatched frees.

## Direct and bounce selection

`Auto` tries direct and falls back to bounce when its prerequisites fail. `Direct`
returns an error when direct is ineligible; `Bounce` forces staging.

The host queries every covering page-table leaf under one kernel-layout lock.
The whole range must be writable/readable kernel RAM, physically contiguous with
uniform attributes and free of DEVICE/USER mappings. Runtime integration rejects
private guest memory when KVM page-sharing is enabled, preserving the existing
coherent staging path and avoiding incidental sharing of neighboring pages.

kdma checks translation overflow, the entire streaming mask interval and cache
isolation. Uncached memory needs ordering only. Cacheable memory uses the global
host cache contract: x86_64 supplies hardware consistency; AArch64 uses explicit
maintenance and conservative whole-page isolation. AArch64 instructions stride
by CTR_EL0.DminLine rather than the isolation alignment. Other hosts currently
report unsupported cacheable direct maintenance and use bounce for cached RAM.
Current verification covers homogeneous AArch64/x86_64 QEMU CPU configurations.

Successful direct mapping preserves page attributes and uses the original RAM.
Bounce mapping allocates a TLSF slot from one lazy 4 MiB uncached/shared pool,
checks the complete bus extent and copies initialized receive bytes before
publication. Transmit/bidirectional bytes are copied by the initial device
handoff. Both choices publish one record containing original pointer, length,
direction, current owner and backend-specific state. A predecessor/successor
BTreeMap check rejects overlapping DMA intervals in logarithmic time.

## Ownership state and cache contract

```text
prepare -> Device-owned -> sync_for_cpu -> CPU-owned
               ^                              |
               +-------- sync_for_device -----+
Device-owned or CPU-owned -> unmap -> released
```

The previous owner stops accessing the range before sync. The new owner can use
it only after success. Repeated sync to the current owner is idempotent, including
final unmap when the CPU already owns the latest bytes. This avoids overwriting
CPU modifications with stale bounce bytes. CPU access to receive-only mappings is
read-only; writing requires bidirectional mapping.

| Direction | Prepare / sync for device | Sync for CPU / device-owned unmap |
|---|---|---|
| DriverToDevice | clean, or original-to-bounce copy | ordering only |
| DeviceToDriver | clean+invalidate; initialize bounce on first map | invalidate, or bounce-to-original copy |
| Bidirectional | clean+invalidate, or original-to-bounce copy | invalidate, or bounce-to-original copy |

Cleaning before receive preserves dirty original bytes on cancellation or partial
writes. Non-coherent invalidation operates on exclusively owned whole lines.
`DmaCacheIf` completes maintenance synchronously to PoC. Its borrowed
`DmaCacheRange` has private construction, so safe code cannot forge requests over
arbitrary addresses. Cache errors retain the record and old owner. SeqCst fences
surround actual ownership transitions independently of cache operations; coherent
memory still requires ordering. kdma does not replace device doorbell/descriptor
protocol barriers or establish hardware quiescence.

## Execution context and concurrency

Memory management, kernel mappings and host hooks must be initialized first.
The backend supports task and ordinary IRQ contexts and performs no sleeping;
NMI/reentrant entry is unsupported. Coherent allocation and first bounce use
allocate pages and update mappings. Direct query, cache instructions, copies and
BTreeMap operations run on the calling CPU under a shared SpinNoIrq backend lock,
with no fixed latency bound.

Lock order is backend -> allocator/kernel layout. Backend/layout lock waits poll
only incoming TLB acknowledgements via `DmaPageTableIf::poll_pending_tlb` to retain
progress during IRQ-off shootdowns. Original RAM and its page attributes remain
the caller's pinned exclusive responsibility between calls; a query is not a pin.

## Boundaries and verification

The domain supports one segment and linear physical-to-bus translation.
Scatter/gather, IOMMU, dma-ranges, reserved low-address pools and independent
hardware domains remain separate features. Software masks and bounce storage
provide checks and staging, not containment of arbitrary hardware DMA.

Tests cover interval masks/overflow, rejected bounce slot recycling, cache policy
and direction selection, failed/duplicate ownership handoffs, fully initialized
coherent extents, direct mappings of coherent and cached pages, preserved page
attributes, forced bounce, cancellation and CPU edits retained after sync/unmap.
QEMU also runs existing VirtIO tests against the mainline static HAL. Simulated
CPU writes to cached direct memory are explicitly cleaned before the receive-side
invalidation test; this verifies instruction/ownership integration, not physical
non-coherent device hardware.
