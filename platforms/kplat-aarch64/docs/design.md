# kplat-aarch64 Design

## Scope

This crate implements the generic AArch64 device-tree platform and common
peripherals. `src/lib.rs` gates the crate on `target_arch = "aarch64"` and
`k_plat_name = "kplat-aarch64"`; peripheral feature gates remain in
`src/peripherals/mod.rs`.

## Guest Test Layout

Guest tests live in crate-root `ut-tests/`, mounted by
`unittest::path_mod_test!` at their original parent modules:

| Parent module | Test file | Cases |
| --- | --- | --- |
| `peripherals::smccc::tests` | `ut-tests/peripherals_smccc.rs` | 4 |
| `peripherals::trng::tests` | `ut-tests/peripherals_trng.rs` | 6 |

The private `tests` modules retain access to parent helpers through `super`.
They are compiled only with `cfg(unittest)`, within the crate's architecture
and platform gates. The SMCCC conduit expectation uses `cfg!(feature = "vmm")`:
both feature settings register all four cases, with different expected HVC
handling. No case is serial or user-mode.

SMCCC cases exercise conduit decoding and selection without issuing firmware
calls. TRNG cases inject SMCCC results into `read_trng_random_with` to check
errors before data and partial progress after data. Moving these tests does
not change production code or firmware initialization.
# AArch64 Platform DMA Hooks

This document describes the DMA provider in `src/dma.rs`. The crate also provides
platform initialization, power, MMIO and peripheral support; those providers are
outside this DMA contract.

With `kvm-guest-mem-share`, `PlatformDmaIf::prepare` shares every base page using
`guest_mem_share_page`; `release` unshares every page. Without that feature the
platform uses the default DMA provider. `kdma` calls the provider after acquiring
physical backing and before ending the backing's final DMA hold.

`transition_dma_pages` validates base-page alignment and checked range endpoints
before invoking hypercalls. A successful range means all calls succeeded. A
failure is reported as `KError::Io`; the loop still visits the rest of the range
so one failed page does not prevent later pages from being released. Preparation
failure may leave a partially shared range for the caller to unwind.

The provider performs no allocation, page-table mutation or ownership transfer.
The caller retains backing throughout the synchronous transition and must stop
all device users before release. The provider does not issue DMA completion waits.
Unit tests inject per-page failures into the common loop without issuing actual
KVM hypercalls. Builds with the sharing feature also compile the actual provider.
