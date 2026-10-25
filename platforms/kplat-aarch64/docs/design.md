# kplat-aarch64 Design

## DMA Preparation Failures

With `kvm-guest-mem-share`, DMA preparation stops and returns `KError::Io` on
the first rejected share hypercall. Release attempts every page and returns an
error if any unshare fails. It must not report success after only logging a
hypervisor failure: the DMA allocator uses success to authorize page reuse.
After a partial prepare, kdma releases the entire attempted range. If the
hypervisor rejects unsharing an unprepared page, the allocation is conservatively
retained, not assumed safe. TCG tests do not validate these vendor hypercalls.

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
