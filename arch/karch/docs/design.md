# Cache Isolation Layout

## Scope

This document covers `src/cache.rs` and `src/<arch>/cache_layout.rs`, the pure
layout part of `karch`.
Architecture-specific instruction operations remain described in the crate
README and their source rustdoc. The DMA-specific maintenance extensions are
covered separately below.

## Contract and Implementation

`CacheAligned<T>` owns one `T`, uses architecture-selected `repr(align(...))`, and exposes ordinary
shared/mutable borrows through `Deref`/`DerefMut`. `new` is const and does not
allocate, run privileged instructions or depend on CPU/runtime initialization.
Normal Rust moves and destruction apply; this is not pinned storage.

Each architecture owns a `src/<arch>/cache_layout.rs` file. It invokes a private
macro with its alignment literal; the common module selects that file by
`target_arch` and supplies the shared API implementation. There is no generic
fallback for an unknown architecture. `SMP_CACHE_BYTES` derives from the selected
`CacheAligned<()>`, keeping the numeric policy and representation consistent.

| Architecture | Layout policy | Basis |
| --- | ---: | --- |
| AArch64 | 64 bytes | Linux arm64 generic L1/SMP layout, distinct from DMA alignment |
| x86_64 | 64 bytes | Linux X86_64 default; legacy x86-32 CPU choices are not applied |
| RISC-V32/64 | 64 bytes | Linux generic RISC-V layout |
| LoongArch64 | 64 bytes | Linux default L1_CACHE_SHIFT=6 |
| ARM32 | 128 bytes | Conservative coverage of Linux's 32/64/128-byte ARM configuration choices |

The source comments link to the Linux definitions behind each choice. These
are architecture-owned build-time policies, not claims that every CPU of an
ISA has the same hardware line size. A platform needing a different policy must
extend its architecture's selection rather than changing the common wrapper.
UP builds use the same policy. Stronger alignment required by `T` is preserved;
nonempty arrays have padded strides. Zero-sized values need not have distinct
addresses. This policy is not detected cache geometry, DMA alignment or an
instruction stride, and may not isolate larger hardware coherence lines.

## Consumers and Validation

Timer CPU slots wrap the complete spinlock and protected queue. Watchdog wraps
its per-CPU heartbeat state while preserving its public type and methods.
Neither caller gains synchronization from padding; their existing concurrency
protocols remain authoritative. The host timer harness imports this exact pure
source file, rather than linking the kernel configuration dependencies of
`karch`. Tests cover array isolation, larger payloads, stronger alignment and
value access; consumer tests cover timer slots and watchdog state.


## DMA maintenance primitives

AArch64 data-cache maintenance now reads CTR_EL0.DminLine for instruction stride.
Clean, invalidate and clean/invalidate complete to PoC with DSB SY. Invalidation
APIs are unsafe and require pinned live RAM, complete line endpoints, exclusive
ownership and stopped CPU/device aliases. The DMA host chooses a separate,
conservative whole-page isolation policy; the stride is not that isolation limit.

x86_64 reports CLFLUSH availability and stride from CPUID leaf 1. Its unsafe
clean/invalidate range completes with MFENCE and is used before DMA page memory
types change. Coherent streaming DMA still uses ownership ordering without cache
instructions. kdma and the runtime validate full range and alignment before these
primitives. These additions retain the independent SMP layout policy above.
