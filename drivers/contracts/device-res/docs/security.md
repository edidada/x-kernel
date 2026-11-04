# device-res — Security And Reliability

## Trust boundary

Device descriptors and host providers are kernel-supplied, not trusted
user pointers. The framework must validate firmware ranges/routes before
acquisition. This facade neither validates device presence nor proves that
hardware has stopped. IRQ identities and operations now use os-api types.

## Lifetime and concurrency

Successful acquisition transfers an owning guard to device cleanup. MMIO and
DMA raw views are usable only while that acquisition remains live. Cleanup
must occur once, in reverse registration order, after driver quiescence.
An external reference to the device object does not authorize use after
unbind. DeviceResource itself provides no lock or liveness gate.

IRQ cleanup calls synchronous provider release with the original resource and
action token. Normal return must establish callback quiescence. Invalid
calling context or rejected release cannot be treated as successful teardown.
Threaded requests remain Unsupported unless a provider implements the matching
registration and drain protocol.

## Unsafe inventory

This crate contains no unsafe implementation or unsafe block. Actual mapping,
DMA and IRQ implementation obligations belong to os-api contracts and their
host provider. Returning a safe-to-construct raw pointer does not make arbitrary
dereferencing safe; drivers must obey bounds, alignment and lifetime rules.

## Failure analysis

| Failure | Required behavior / limitation |
|---|---|
| Provider rejects acquisition | Propagate error; register no cleanup. |
| Probe fails after earlier acquisitions | Framework runs their cleanup once in reverse order. |
| IRQ callback still in flight | Provider release waits; resources used by it remain live. |
| Cleanup in atomic context or under handler-needed lock | Caller contract violation; may panic/deadlock in host; framework must prevent it. |
| Cleanup closure/storage allocation failure | Existing infallible allocation policy remains; no recoverable transaction is claimed. |
| Device continues DMA after driver remove | Cleanup cannot repair this; driver stop protocol is required before memory release. |
| MSI reservation release rejected | Current os-api unit-returning release limitation remains; this facade does not supply retry ownership. |

No global registries, logging or user data are introduced here. Existing kernel
provider diagnostics may expose resource identifiers through kernel logs.

## Audit checklist

- Only successful acquisitions are registered for cleanup.
- Provider and original acquisition identity remain available through release.
- Driver stop precedes cleanup, and no required callback locks are held.
- Raw views cannot be used after cleanup.
- No framework identity or registry is moved into os-api to retire this facade.

Tests under ut-tests verify cleanup order and failure pairing. Callback-tail
drain and shared-peer isolation are covered by provider/kdriver SMP fixtures,
not inferred solely from mock cleanup tests.
