# kdma — Security And Reliability

## Trust model and external boundaries

Trusted probe/glue supplies device masks, alignment, segment limits and physical
to bus translation. Context validation proves representable intervals and
requested address reachability, while correctness of those declarations depends
on platform and driver capability knowledge. Returned addresses must be
programmed only into the selected device. A per-device namespace provides
software ownership separation; an unrestricted device can still DMA outside
its granted interval in the absence of hardware isolation.

Raw buffer pointers enter the unsafe map, sync and unmap APIs. Callers retain
initialized, live, nonoverlapping storage and serialize CPU/device access.
DMA memory carries network, filesystem or other I/O payload; this crate parses
none of those protocols and never directly accesses userspace pointers or MMIO.
Page-table and platform prepare/release providers are trusted host boundaries.

## Unsafe inventory and invariants

| Path | File | Invariant and guard |
| --- | --- | --- |
| allocate/deallocate entry points | `src/lib.rs`, `src/dma.rs` | Original context, addresses and Layout identify one live allocation; free exactly once after hardware and CPU aliases retire |
| map copy-in | `src/dma.rs` | Caller-provided initialized source covers len bytes; constrained bounce allocation is a separate live region |
| sync and unmap copyback | `src/dma.rs` | Active record matches context, bus address, original CPU pointer, length and direction; previous owner has stopped |
| padded bounce pointer | `src/bounce_pool.rs` | Padding is bounded by alignment minus one and fits the allocated padded Layout |
| TLSF release | `src/bounce_pool.rs` | Allocation base belongs to this pool and uses its original padded Layout |

Page allocation validates the whole rounded extent inside the translated mask
window. Requested coherent alignment applies to CPU and bus addresses.
Streaming masks and segment limits apply to the whole requested interval.
Exclusive IRQ-disabling locking protects each context's pool and mapping table.
Fences order ownership copies; platform uncached/shared mappings supply the
cache policy. Memory barriers alone do not authorize concurrent CPU/DMA access.

Coherent trace metadata is diagnostic and feature-gated. Production frees trust
the unsafe caller's original context, pointer and layout. Streaming records are
always tracked. DmaInfo and bus addresses are copyable values, not unique owners.

## Threat analysis

| ID | Threat | Impact | Trigger | Implemented control and residual risk |
| --- | --- | --- | --- | --- |
| T-01 | Address truncation or translation outside a device window | High | Low DMA mask, nonzero bus offset, end crossing limit | Checked whole-interval translation, independent masks and bounded physical allocation; declarations must describe actual hardware |
| T-02 | Wrong-device release or stale mapping arguments | High | Context/provider replaced before completion | Device-bound retained provider; streaming original pointer, length and direction checks; coherent matching requires unsafe contract |
| T-03 | Copyback reads stale pool contents | High | Cancelled receive mapping or short device write | Copy initialized original bytes for every direction before publication, including receive buffers |
| T-04 | Device overwrites unrelated memory | High | Malicious device exceeds programmed extent | Declared addresses are constrained in software; hardware DMA isolation remains absent |
| T-05 | Freed pages retain uncertain cache/share attributes | High | Page-table or platform hook failure | Quarantine backing on failed transition; accepted memory loss until reboot |
| T-06 | Original buffer disappears during DMA | High | Caller drops or accesses pending storage | Unsafe lifetime/ownership contract plus queue/provider retention; context Drop retains unresolved backing but cannot preserve caller-owned original storage |

## Failure modes and handling

| Failure | Local effect | Handling |
| --- | --- | --- |
| Invalid constraints/window/segment | Mapping cannot be published | InvalidInput before hardware handoff |
| No suitable physical block or bounce capacity | I/O/probe fails | NoMemory; prepared queue mappings roll back |
| Platform prepare or page protection fails | Allocation fails with uncertain page state | Log and quarantine pages |
| Platform release or attribute restoration fails | Backing remains allocated | Log and retain pages; general allocator never reuses them |
| Active mapping mismatch on unmap | Kernel contract violation | Assertion prevents wrong-buffer copy/recycle |
| Context dropped with active mappings | Outstanding transaction loses software owner | Log and retain pool; caller must separately stop DMA and retain original buffers |

## Privacy, limitations and audit

Coherent pages are uninitialized until the consumer initializes them. VirtIO
zeroes coherent pages before publication; streaming copies the full initialized
original range. Freed pages are not scrubbed. Each lazy pool reserves 4 MiB and
large boundary padding can reduce useful capacity; reachable windows smaller
than the pool cannot create streaming mappings. Bound searches scan free
lists under the page allocator lock. Firmware DMA ranges, IOMMU, SG splitting,
cacheable coherent policy and direct buffer mapping are outside the current
implementation.

Review each new consumer for accurate device capability selection, retained
provider lifetime, initialization before map/exposure, stop-DMA-before-release,
exact original unmap arguments and rollback before descriptor publication.
Review platform failures for quarantine before general reuse, and avoid treating
software mask checks as hardware protection against an untrusted device.

## TLB progress during lock contention

DMA page attributes are updated with IRQs disabled and can wait for remote TLB
acknowledgements. A CPU spinning for the same device allocator or the shared
kernel page-table lock must still acknowledge that request. Context lock wait
uses try_lock and DmaPageTableIf::poll_pending_tlb; the runtime provider applies
the same policy while acquiring the kernel layout lock. The provider pins the
CPU and excludes IRQ reentry while servicing only pending TLB invalidations.
It executes no arbitrary IPI callback, device callback or scheduler operation.
This avoids the DMA lock/shootdown wait cycle observed by x86_64 SMP tests.
