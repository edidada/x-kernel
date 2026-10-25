# hwrng — Design

## Purpose and scope

`src/lib.rs` defines the OS-independent hardware entropy read contract;
`src/completion.rs` defines the host-injected wait capability. Neither owns
scheduler state, DMA storage, entropy accounting, or source trust policy.

`src/registration.rs` defines HwrngRegistry, its HWRNG instance, HwrngRegistration
and the operation-only HwrngHandle. This subsystem owns registration, admission
and synchronous drain; it does not use generic Class/Registration/Publication.
It does not depend on the device binding model in `drivers/base/model`.

The registry is a subsystem provider registry, not a Linux device class.
The remaining driver_class dependency supplies only the existing read-only
PublicationSource interface. The model now constructs immutable per-binding
provenance through DeviceAccess::publication_source; hwrng does not depend on
that model or inspect its binding state. Retiring the legacy trait dependency
remains R1-09 work.

## Architecture and execution context

Consumers use `HwrngDevice::read`; concrete drivers receive an
`Arc<dyn RngCompletion>` implemented by the host. Nonempty reads require
sleepable task context, including kernel workers; no process identity is needed.
Empty reads return zero without I/O. Successful nonempty reads may be short.
Drivers must keep in-flight DMA storage independent of the caller's buffer.

## Wait flow and concurrency

`wait_until` registers for notification before checking its predicate. A true
predicate commits the result. Spurious notification only causes another
predicate check. Task interruption does not cancel this internal read. The
driver publishes completion or a terminal fault under its own lock and calls
IRQ-safe `notify` after releasing it. No device or IRQ-disabling lock may span
a wait. There is no polling timeout.

## Failures and resource lifecycle

Reads and host waits return the shared `KError`. `InvalidInput` identifies
a context that cannot sleep. Fallible wait preparation must finish before
invoking the readiness predicate; subsequent registration must be infallible
and allocation-free. These errors alone do not establish device failure, and the error type
does not encode whether a failure came from the host or the device. Entropy
consumers use `HwrngDevice::is_failed` before permanently disabling a source
on a read error. This non-sleeping query reports a monotonic terminal device
state; false does not promise hardware progress. Host wait errors never set it.
Retries require a valid sleepable context and consumer-owned backoff. A read
error delivers no bytes and does not invalidate a submitted DMA request. The
contract does not define hotplug, reset or an offline event protocol; the
driver and host own resource lifetime.

## Publication lifecycle

HwrngRegistration is non-cloneable and explicitly unregisters through a mutable
borrow. Its Drop does not unregister, wait, panic or invoke callbacks. The registry
keeps a strong runtime reference; losing the credential leaves an observable
registration rather than prematurely freeing hardware. HwrngHandle clones retain
the original provider and expose no raw runtime accessor.
Both read and is_failed acquire operation permits. is_failed returns KResult<bool>:
BadState means the capability is unavailable, not terminal hardware failure.
There is no raw runtime accessor or Deref on the consumer facade, preventing
consumers from cloning the underlying driver Arc out of an operation permit.
The provider trait HwrngDevice remains unchanged; its raw failure query is bool.
Existing reads retain their permits until completion. Withdrawal cannot cancel a
hung device request; DMA stop/reset and joining hardware operations remain the
concrete driver's responsibility. The legacy boot bridge intentionally discards
its credential; registry retention keeps the boot-only device alive. This bridge
has no removal path and is not evidence of new-model hardware hot removal.

Publication handles have no independent name. Publishers supply source identity
and runtime plus a preconstructed reliable drain provider. Source/path uniqueness
is not imposed. Source device-online state is not the operation gate; each provider
has its own closed bit and in-flight count under one SpinNoPreempt lock.

## Reliable synchronous drain

RngDrain is separate from RngCompletion, with a separate host WaitQueue. A host
may reject invalid context only before calling the private predicate. After the
first call it must wait until true, without allocation failure, timeout or signal
errors. Host construction precedes publication; X-Kernel uses the already merged
CompletionWait stack waiter, not a new synchronization primitive.

The predicate's first call closes admission, removes the registry entry, then
checks the in-flight count. It executes no business callbacks. The credential
retains the entry, and the removed registry reference is dropped outside locks
after waiting, preventing final runtime destruction inside the predicate.
Operation guards decrement under the admission lock and notify outside it when
the closed provider reaches zero. Registry locks never span runtime methods,
waits, source metadata calls or subscriber callbacks. Registry/handle APIs require
task context; host drain notification itself is non-sleeping.

After drain, unregister marks the credential completed and invokes removed
subscribers before returning. Successful repeats do nothing. Removed is not the
driver's read wakeup and does not certify reset/IRQ/DMA teardown. Drivers must stop
and wake readers before draining; callbacks cannot wait for work needing the
publisher's management lock. Retained handles do not delay drain but may delay
final memory release; final source/runtime/wait-provider destructors must not
sleep. Hardware guards must be explicitly released during driver teardown.

External tests cover preparation rejection with zero predicate calls, unchanged
registration on error, residual registration after credential Drop, old-handle
rejection, in-flight drain, notification ordering, repeated unregister, early
wake and waiter detachment. The deliberately forgotten test registration is
private to its test registry and retained for that test kernel's lifetime.
The in-flight test uses nonempty reads and explicit task-state handshakes.
Five-second watchdogs only bound failure detection, not correctness; the test
releases its read gate before collecting task results. A broken implementation
that leaves a task permanently stuck still requires the outer guest timeout.
