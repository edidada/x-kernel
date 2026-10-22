# hwrng — Design

## Purpose and scope

`src/lib.rs` defines the OS-independent hardware entropy read contract;
`src/completion.rs` defines the host-injected wait capability. Neither owns
scheduler state, DMA storage, entropy accounting, or source trust policy.

`src/class.rs` defines HwrngClass, its HWRNG instance, HwrngRegistration
and the operation-only HwrngPublication consumer facade. Generic admission,
withdrawal and in-flight teardown belong to `drivers/base/class`; this crate
does not depend on the device binding model in `drivers/base/model`.

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

HwrngRegistration is non-cloneable and owns generic Registration. Its drop
withdraws the capability; HwrngPublication clones only keep shared memory alive.
The registry and callbacks expose HwrngPublication, never the generic payload.
Both read and is_failed acquire operation permits. is_failed returns KResult<bool>:
BadState means the capability is unavailable, not terminal hardware failure.
There is no raw runtime accessor or Deref on the consumer facade, preventing
consumers from cloning the underlying driver Arc out of an operation permit.
The provider trait HwrngDevice remains unchanged; its raw failure query is bool.
Existing reads retain their permits until completion. Withdrawal cannot cancel a
hung device request; DMA stop/reset and joining hardware operations remain the
concrete driver's responsibility. The legacy boot bridge retains its registration
for kernel lifetime and does not implement hot removal.
