# kplat-aarch64 Test Security Scope

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
# AArch64 DMA Hook Reliability

The DMA provider trusts the hypervisor's status for each page and the caller's
exclusive allocation and device-retirement protocol. Inputs are physical address
and byte length; alignment and endpoint overflow are checked before any hypercall.

`peripherals::kvm::guest_mem_share_page` and `guest_mem_unshare_page` return whether
SMCCC's result is zero. `src/dma.rs::transition_dma_pages` must propagate any false
result as a range error. Logging followed by success would allow `kdma` to return
still-shared backing to buddy. Later pages are attempted even after an error;
range failure therefore does not specify which subset completed.

There is no new unsafe operation in the range loop. Physical storage lifetime,
cache attributes, device quiescence and software aliases remain caller concerns.
`kdma` restores attributes only after platform release succeeds and retains the
complete allocation on retirement failure. This conservative policy can retain
RAM permanently after hypervisor failures; it provides no IOMMU isolation and
cannot detect a hypervisor that falsely reports success.
