# kdma — Security and reliability

## Trust and external boundaries

Drivers provide initialized pinned kernel RAM, exact layout/mapping metadata,
DMA directions and hardware-stop guarantees. kdma trusts the kernel allocator,
page-table query/protect provider, cache provider and platform sharing hooks.
Inputs are kernel pointers and device-visible bus intervals; there is no direct
userspace, firmware parser, MMIO register or syscall boundary in this crate.
A device can alter mapped bytes and timing; software metadata never comes from
DMA storage. Independent hardware isolation belongs to the platform/IOMMU.

Typed allocation and streaming tokens separate owned pages from temporary grants.
They retain original metadata, expose raw CPU/bus addresses, and require explicit
unsafe release after hardware and aliases stop. Dropping a token retains memory;
losing a streaming token still requires keeping original RAM pinned. Existing
flat APIs continue to require original provider, pointer, direction and layout.
Masks check addressability and do not authenticate a hardware device or domain.

## Unsafe inventory and enforced invariants

| Path | Location | Obligation / enforcement |
|---|---|---|
| coherent allocation/free | `dma.rs`, `mapping.rs` | allocator supplies contiguous pages; whole extent initialized before attribute/share transition; token retains exact layout; caller stops hardware and aliases |
| physical range query | host `dma_integration.rs` | kernel page-table lock covers all leaves; check uniform RAM flags and physical continuity; caller pins attributes and storage after query |
| streaming map/sync/unmap | `dma.rs`, `mapping.rs` | initialized exclusive original RAM stays live; active records retain original arguments; direction/length/pointer validated before memory access |
| bounce copy/recycle | `dma.rs`, `bounce_pool.rs` | separate source/destination cover the validated length; private record retains exact TLSF layout; pool containment checked on recycle |
| cache maintenance | `cache.rs`, host `dma_integration.rs` | private range construction, stopped owners, stable RAM mapping, complete exclusive isolation boundaries; host checks hardware stride before instructions |
| architecture instructions | `karch` cache modules | AArch64 CVAC/CIVAC/IVAC followed by DSB; x86 CLFLUSH availability/stride from CPUID and completion with MFENCE |

Direct mapping performs no ordinary-buffer page attribute changes. The host
rejects DEVICE/USER mappings and private guest sharing configurations. AArch64
cached direct requires complete pages, isolating suballocation neighbors; hardware
maintenance still uses its reported line stride. Tests run on homogeneous QEMU
CPUs. Heterogeneous CPU cache capability discovery is outside current validation.
Uncached direct mappings still require exclusive buffer ownership and ordering.

Every published interval is nonempty, checked for address overflow and mask
reachability. The active table rejects overlapping device intervals through
ordered predecessor/successor lookup. Matching sync verifies original metadata
before copying or invalidating. This does not pin mappings by itself: raw-pointer
callers must prevent remapping, freeing or attribute changes until successful unmap.

## Ownership and failure handling

Mappings start device-owned. Only a successful sync changes ownership; repeated
sync to the existing owner leaves bytes unchanged. Unmap synchronizes only when
the device still owns the buffer. CPU-owned bounce bytes are retained, including
edits after a prior sync. Receive-only buffers are CPU read-only until unmap.

The typed unmap returns its original token on maintenance failure for retry;
backend bookkeeping and storage stay active. Flat unmap keeps its existing void
contract and asserts success, retaining the record on failure. Ordinary selection,
allocation, capability and maintenance failures use `AllocResult`. Required-direct
failure publishes nothing. A rejected bounce candidate recycles its exact slot.
Release completes maintenance before platform unshare can revoke access through
old shared attributes. Failed page attribute/share transitions quarantine backing
pages; release does not return uncertain memory to the general allocator. Optional `dma-trace` rejects
mismatched coherent frees; safety of untraced flat free remains the unsafe contract.

Coherent memory is initialized across the rounded extent, including padding.
Receive bounce initialization prevents cancellation and short writes from
copying stale pooled bytes over the original buffer. Cache clean before a direct
receive preserves initial CPU contents before invalidation. Directional cache
operations and CPU/device publication fences provide distinct guarantees.

## Thread and execution safety

One static registered SpinNoIrq protects shared state and pool lifetime. Runtime
kernel-layout access uses IRQ/preemption exclusion and the same TLB-acknowledgement
polling protocol as backend contention. Lock order remains backend before page
allocator/layout. Hooks do not sleep or dispatch arbitrary pending work. IRQ
latency depends on allocation, copies, page queries and cache maintenance; no
fixed bound is claimed. NMI and reentrant DMA entry are unsupported.

## Threat analysis

| ID | Trigger and impact | Implemented control / residual risk |
|---|---|---|
| T-01 | forged/duplicate release corrupts memory | typed tokens retain unique original metadata; flat release requires unsafe exact-match/exact-once contract; trace adds diagnostics |
| T-02 | DMA outside the programmed grant corrupts/discloses memory | checked software intervals and private metadata; arbitrary device DMA still requires hardware isolation |
| T-03 | neighboring cached bytes lost during invalidation | AArch64 whole-page direct isolation and host instruction alignment checks; caller must retire aliases |
| T-04 | stale data or lost CPU edits at ownership handoff | direction-aware maintenance/copies, ordered state transitions, idempotent sync and CPU-owned unmap |
| T-05 | CPU-buffer remapping/free while DMA is active | explicit pin/lifetime contract and caller hardware-stop requirement; page query alone does not enforce lifetime |
| T-06 | receive cancellation copies prior tenant data | initialized receive bounce storage and complete coherent allocation zeroing |
| T-07 | shared-domain/private guest mismatch exposes unrelated memory | host disables direct for KVM page-sharing configurations and retains page-owned coherent bounce setup |

## Failure modes

| ID | Failure | Local/system effect | Handling |
|---|---|---|---|
| F-01 | pool/page exhaustion | submission fails | Result; no DMA address published |
| F-02 | direct prerequisites unmet | required-direct fails or Auto stages | deterministic selection; no hidden attribute mutation |
| F-03 | maintenance failure | ownership cannot safely move | record/token retained; retry before access/release |
| F-04 | attribute/share hook failure | mapping state uncertain | quarantine pages, no allocator reuse |
| F-05 | forgotten cleanup | memory/pool capacity retained | explicit must-use tokens and caller cleanup; retained state protects against premature recycling |
| F-06 | IRQ-off lock contention during shootdown | CPU progress depends on TLB acknowledgement | poll TLB-only requests while waiting; hook and CPU progress remain trusted |

## Privacy, limits and audit

Buffers can contain network, storage or other application data supplied by trusted
kernel drivers; the crate performs no logging of their contents. The shared pool
retains bytes after recycle, but receive storage is initialized before publication.
A faulty device can retain a previous DMA address; preventing post-unmap device
access remains the driver/hardware isolation obligation.

Current support is linear translation and single-segment mapping in one domain.
Cached direct maintenance is implemented for AArch64/x86_64; other hosts retain
cached bounce fallback. Low-address candidate search, independent domains,
IOMMU and SG are deferred. Mainline VirtIO still has an infallible static share
interface, so exposing mapping failure cleanly to its callers needs separate
library work with a concrete failure case.

Review complete interval arithmetic, uniform RAM provenance, isolation alignment,
CPU/device stop ordering, unchanged original metadata, no sync on persistent
coherent storage, retry ownership after hook error, and quarantine on uncertain
attribute/share transitions. Guest QEMU tests verify actual mapping/cache hook
integration and simulated ownership; non-coherent physical devices need additional
hardware validation.
