# Native IRQ and RNG notification-tail acceptance

## Baseline and scope

Implementation baseline: driver branch `e1ab737f9`, rebased onto main
`289e2276b48934a6f813e9fec488527710f5e17c`. This change adds tests and one
KIRQ-owned `cfg(unittest)` synthetic dispatch entry, not production IRQ
registration, dispatch or drain behavior. No external IRQ worktree is changed.

Current core withdrawal removes the exact shared-action token, retains the
original Line, and waits for its in-flight dispatches. Dispatch admission covers
the entire synchronous callback, not merely its device-lock critical section.
The driver integration tests below validate that contract through NativeIrq and
the host scheduler, rather than relying on the old uncommitted acceptance patch.

## Tests and assertions

- `kdriver::host_irq::smp_tests::native_irq_smp_release_waits_for_callback_tail_and_preserves_shared_peer`:
  register A and B through NativeIrq; hold A's hardirq callback on CPU1; start
  release on CPU0 and observe it actually Blocked while its independent resource
  sentinel is alive. Open the gate, join both tasks, check repeated A withdrawal
  returns `Ok(None)`, then dispatch B again without invoking A. Finally release B.
- `kdriver::device_management::virtio_rng_lifecycle_unittest::notify_tail::concrete_rng_finish_stop_waits_for_native_irq_notification_tail`:
  construct the concrete RNG with controlled transport/DMA and real NativeIrq;
  complete its sole request and gate the lock-free completion notification on
  CPU1. Close reads, join the reader and call `finish_stop` on CPU0. Reset is
  confirmed, but native IRQ release must remain Blocked and the two ring
  allocations must remain live until notification returns. The completed mapping
  is already reclaimed exactly once, not a pending token to cancel. After stop,
  the separately registered shared peer still dispatches; its final release
  removes the fixture descriptor.

Both tests require two online CPUs and execute serially. The coordinator is
pinned to CPU0, and observations verify callback/release CPU identities. Gates
use atomic handshakes, not elapsed-time assertions. Workers are joined and peers
released before assertions can return a failed test. The synthetic dispatcher
uses KIRQ's real admission and action snapshot under scoped hardirq context but
never fabricates a controller cookie or programs a physical IRQ.

## Reproduction and evidence

Local evidence directory:
`.scratch/driver-model-r1/evidence/irq-tail-2026-11-04/`.
Use the worktree-private regular `disk.img`; do not share it with another QEMU.

```sh
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
timeout --kill-after=10s 600s make unittest \
  UNITTEST_CRATE=kdriver::host_irq::smp_tests,kdriver::device_management::virtio_rng_lifecycle_unittest::notify_tail \
  XKMAKE_ARGS='--no-net --no-vsock'
```

The canonical configuration selects PCI, four CPUs and the ordinary kernel
providers. This is a non-exclusive functional run, not a performance measurement.
The first focused run passes **2/2, zero ignored**, including coverage export.

### Negative control

Temporarily guard only `try_free_irq_inner`'s `IrqDrain::wait` with
`if !matches!(virq, 0x7f00_1000 | 0x7f00_1001)`. These are the two fixture Lines;
each test verifies its descriptor identity is unused before creating it. All
real IRQ Lines retain their drain, including unrelated boot/background users.
Run exactly the same focused selection, then restore the original bytes.

The final negative run reports **0 passed, 2 failed, zero ignored**, exits with
Make status 2 and completes guest reporting rather than timing out. RNG fails
`!observations.returned_early`; NativeIrq fails
`!returned_early && !destroyed_early`. Both failures occur after worker/peer
cleanup. The callback retains independent Arc-backed gate storage, and the RNG
notification tail accesses no freed ring pointers, so the mutation does not
require a test-induced use-after-free. `negative.log` and
`negative-mutation.patch` retain the evidence. The earlier negative run also
failed both tests at the Blocked assertions; the final run orders early-return
assertions first for a more direct diagnostic.

`manager-before-negative.sha256` verifies byte-for-byte restoration, including
the legitimate test-module mount. No bypass or negative-control switch remains
in the submitted source. Final positive validation uses a fresh restored build,
not the negative binary.

## Acceptance boundary

The tests cover software callback-tail drain, adapter wiring, queue retention,
single reclamation of a normally completed request and shared-action isolation.
They do not certify physical GIC delivery/EOI, late hardware DMA, MSI write
ordering, vector reuse or PCI virtio-net shutdown. Pending-request cancellation
is covered separately by existing controlled and real-MMIO pending fixtures;
this normally completed request must not be described as cancellation evidence.

This satisfies the software callback-tail portion of G-IRQ on the recorded
baseline. Next reconcile R1-07/08 requirements with their separate hardware
records before R1-09 compatibility retirement and whole-framework acceptance.
The newly delegated PCI network shutdown scope still requires its own design
and implementation; it is not completed by these IRQ tests.

## Restored regression

The restored-source selection `kirq,kdriver,virtio,driver_os_api,driver_model,
subsys_hwrng` passes **468 tests, zero failures, five ignored (473 total)**,
with coverage export (`positive-final.log`). Both new SMP cases pass and are
not ignored. The five ignored opt-in hardware cases are not counted as acceptance.
The source hashes are retained in `tested-sources.sha256`.

Independent Standards and Spec reviews pass. The Spec review required adding a
shared peer to the concrete RNG case, rather than inferring that property solely
from the native-adapter test. This requirement is present in the final tested
source. `make clippy UNITTEST=y` passes, including the new test helpers. Its
initial run found five combined pointer-offset/volatile unsafe operations in
the controlled ring writer; pointer calculation is now outside each minimal
unsafe access, without a lint suppression. The final positive and negative runs
use that corrected source. Focused pinned formatting and whitespace checks pass.
