# RNG stop and cancellation — library checkpoint

The library owns the VirtIO protocol. The OS adapter owns reader admission,
publication removal, scheduling and IRQ synchronization. This checkpoint adds
library primitives; the kernel RNG adapter now exposes explicit close/finish
helpers, while production bus removal wiring is still pending.

## Protocol

`begin_stop` permanently closes submission and normal completion, then calls
`Transport::begin_reset` once. `poll_stopped` delegates confirmation to
`Transport::poll_reset`; a reset request alone does not enable cancellation.
The caller determines when to
poll or yield outside request locks. No timeout permits resource release.

There is no generic driver run-state enum. RNG owns a permanent request gate
(`is_closed`) and remembers confirmed device quiescence (`is_reset_confirmed`).
Confirmation can only become true after closing the gate; once true it is
never polled or cleared again. Repeated begin/poll calls are harmless.
After begin, IRQ acknowledgement and notification configuration do not touch
transport or queue state. A confirmed stop cannot be restarted.

Reset protocol belongs to the transport. Its default implementation writes zero
status and confirms zero by readback, without a busy-wait loop. A transport can
override the pair when extra completion steps are needed. Both SomeTransport
and the host PCI wrapper forward the pair so those overrides are preserved.
Existing initialization and transport Drop paths are unchanged by this patch;
neither is a substitute for the explicit stop protocol.

This follows Linux's responsibility split: virtio-rng owns removed/request state
(`drivers/char/hw_random/virtio-rng.c:remove_common`), while
`virtio_reset_device` delegates hardware reset to the transport. Linux modern
PCI waits for zero status and synchronizes vectors in `vp_reset`; local Linux
MMIO's `vm_reset` only writes zero. We do not copy those implementations
literally: this OS-independent interface does not sleep or synchronize host
IRQs. The OS chooses polling/yield policy and separately synchronizes software
callbacks before reclaiming resources. No reset state is added to driver-base.

After device reset confirmation **and** host IRQ/user quiescence, the owner may
call unsafe `cancel_request_entropy` with the original token and buffer. It
reclaims the descriptor mapping without requiring a used-ring entry. Unoccupied
or out-of-range slots are rejected. A token reused during normal operation is
not distinguishable from an older token with the same value; callers must
retain the current submission, not an obsolete buffer. Once stopped, no new
submission can reuse a slot, so repeated cancellation is rejected safely.
Cancellation returns only `()`, never an
entropy byte count: backend copy-back does not make canceled data usable.

Completion and cancellation require exclusive access to the same RNG object.
Normal completion before stop releases the mapping; cancellation of that token
then fails. Stop before normal completion routes the outstanding request only
through cancellation. The OS must serialize the encompassing request state and
must not confuse a reused token with an older submission.

## Ownership and safety

The owner keeps transport/MMIO, ring memory, buffer, DMA backend and IRQ guard
alive until their respective stop/drain conditions hold. Reset does not wait
for a software handler's notification tail. Cancellation's unsafe contract
therefore requires the OS to quiesce software users separately.

Drop is not a stop or cancellation API. Existing callers retain their old
queue-unset behavior unless stop was explicitly confirmed. Even after stop,
every outstanding mapping must be reclaimed before the owner releases its
buffer or drops the RNG. The library does not retain caller buffers for it.

The contract relies on a conforming transport's reset-completion guarantee,
including ordering of device DMA writes. Raw zero status is only the default
transport implementation's criterion, not RNG's criterion. Tests use emulated
transport transitions, not real hardware evidence.
Device refusal to reset leaves the owner waiting with resources retained.

## Checkpoint evidence and remaining work

Standalone locked tests pass: all features 76 unit tests + 6 doctests;
no default features 40 unit tests + 4 doctests. Added tests hold a synthetic reset pending,
require mappings/rings to remain live, reject new requests and late IRQ ack,
then cancel without a device completion and reject duplicate reclamation.
Another test completes normally before stop and verifies no second unmap.
The used-entry-before-pop interleaving cancels without reporting entropy and
rejects later completion/repeated cancellation. Token-reuse coverage completes
A, submits B in the same slot, then stops and cancels B using B's actual buffer.
An overridden fake transport keeps reset pending even with zero raw status:
RNG retains DMA until the transport confirms completion, starts reset once,
does not poll before stop, and caches successful confirmation.
An MMIO register fixture covers the default reset/readback pair through
SomeTransport for both legacy and modern MMIO.

The kernel adapter now supplies reader closure/wakeup and a finish helper that
takes resources out of the lock, confirms reset, releases the IRQ guard and
cancels pending requests. Controlled guest tests compose these with explicit
hwrng withdrawal and retained runtime references. Production lifecycle wiring,
probe rollback and concurrent IRQ-tail coverage remain R1-07/R1-08 work. Real MMIO/DMA
integration and hardware validation remain R1-08. The user assigned IRQ core
refactoring elsewhere; this change neither modifies nor certifies that work.
