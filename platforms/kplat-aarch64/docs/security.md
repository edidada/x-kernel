# kplat-aarch64 Test Security Scope

## DMA Hook Failure Boundary

The optional KVM DMA hooks propagate share/unshare failures instead of treating
warnings as success. Partial sharing can leave host-visible pages; the allocator
must retain the allocation if whole-range unshare cannot establish success.
Release attempts all pages even after one failure. This trades capacity loss for
preventing potentially shared pages from being reused as ordinary kernel memory.
The transaction's mock tests do not prove hypervisor behavior on repeated or
unprepared-page unshare; runtime validation requires the matching KVM extension.

## Scope

This note covers the guest tests in `ut-tests/peripherals_smccc.rs` and
`ut-tests/peripherals_trng.rs` and their mounts in `src/peripherals/smccc.rs`
and `src/peripherals/trng.rs`. It is not a whole-platform unsafe-code audit.

## Boundaries

The SMCCC tests call pure conduit helpers, not HVC/SMC assembly, and do not
change the global conduit. The TRNG tests supply local closures and buffers;
they do not call firmware or change the global availability flag. Neither
test file contains unsafe code or accesses live peripheral registers.

The original architecture/platform gates and `cfg(unittest)` keep these
modules out of non-test builds. Private module visibility is retained;
extraction does not widen the production API or change the existing unsafe
firmware-call contracts. These tests check result handling, not firmware
quality or entropy quality.
