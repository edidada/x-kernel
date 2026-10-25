# Driver model R1 implementation checkpoint — 2026-10-25

## Scope

Branch: `xiaohui/driver-base-hwrng`. Implementation started from the accumulated
driver-model branch; the pre-checkpoint HEAD is `75a68bfbc`, incorporating main
`240fcbfd1`. This checkpoint does not claim a new rebase or upstream merge.
The approved design remains unchanged:
`bdc322d03950d8395f23e73daa8624a3ad27ec69bd4d75219c758db6d2f86249`.

- R1-01/02: per-device management serialization, lifecycle closure and probe
  progress acceptance, including controlled controller/child teardown.
- R1-03/04: hwrng-owned registration, explicit synchronous withdrawal, reliable
  host drain and stable binding provenance.
- R1-05: kernel-lifetime entropy work and demand handoff across source changes.
- R1-06: vendored instance DMA ownership and failure rollback; legacy Hal
  compatibility remains. Real host DMA/cache adaptation is not certified.
- R1-07: transport reset contract, RNG admission closure, teardown outside
  driver spinlocks, synchronized IRQ guard release,
  pending cancellation, constructor rollback and actual RNG/model integration
  tests. Teardown runs in sleepable context.

The integration fixture uses actual model callbacks, registration, host
completion/drain and the concrete RNG driver. A real consumer task blocks before
normal removal or post-publication probe failure wakes/drains it. HAL, transport
and IRQ delivery are controlled test implementations, not physical hardware.
Production MMIO binding/remove/rebind wiring belongs to R1-08.

## Validation

- Final configured aarch64 QEMU guest suite:
  `virtio,driver_model,kdriver,subsys_hwrng,entropy`: **210 passed, 0 failed**.
  SMP4, worktree-local disk, `--no-net --no-vsock`.
- `make build` and `make clippy` pass with existing dependency
  future-incompatibility notices.
- Standalone vendor tests: all features **76 unit tests + 6 doctests**;
  no default features **40 unit tests + 4 doctests**. Library clippy passes
  with two existing upstream warnings; strict all-target warning-free status
  is not claimed.
- Incremental Standards/Spec reviews have no outstanding findings for the
  completed slices. This is not a new full-branch security audit.
- Local command logs and recoverable snapshots are under ignored
  `.scratch/driver-model-r1/evidence/`, including `r1-07-model-checkpoint/`.
  Build directories, disk images and local tickets are not source deliverables.

## Deferred IRQ acceptance

The user explicitly deferred IRQ confirmation on 2026-10-25 and authorized
moving to the next development step. Do not change the independently maintained
IRQ worktree or redo Completion. R1-07 remains partially accepted:

- Shared-action preservation and concurrent handler notification-tail drain
  still require confirmation.
- Mock release-order tests are not proof of the real IRQ core's synchronization.
- R1-08 development may proceed against the synchronous IRQ provider contract.
  Hardware unbind/rebind safety and final acceptance must not be declared
  complete until the actual provider contract and pending IRQ cases are checked.

## Next: R1-08

First trace aarch64 QEMU firmware discovery, MMIO ownership and DMA constraints
(mask, offset, coherency) before selecting the host adapter. Do not infer an
unlimited DMA mask solely from absent dma-ranges. Then implement the single
RNG discovery handoff, instance resources and production model binding.
Retain separate evidence for controlled inputs, real Version=2 MMIO, sampling,
pending unbind, stale handles and rebind. Legacy PCI RNG is a regression target,
not a substitute for modern MMIO evidence.

Only local commits are authorized at this checkpoint; no push or main PR.
