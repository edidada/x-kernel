# entropy — Security and reliability

## Fallible work callbacks

`EntropyCollection::new` returns `KResult`: callback KBox allocation failure is
`NoMemory`, invalid work context is `InvalidInput`, and unavailable worker services
are `OperationNotSupported`. It checks sleepable context before allocating owners.
KERNEL allocation additionally requires reclaim-safe context outside source,
collector and registry locks; `register_sources` releases `COLLECTION` before
preparation and reacquires it only to publish if still empty.

Fallible preparation errors are logged without publishing a collection, installing
subscriptions or starting work; the unpublished collector and callback are
released. Initialization remains retryable if no other caller published a winner.
A successfully prepared candidate has subscribed and queued its initial scan
before publication. A concurrent loser is dropped outside `COLLECTION`; its
registry-lifetime Weak subscriber closures can remain inert, but do not retain
the losing collection. This is distinct from allocation failure rollback.

Arc, subscription and WorkHandle storage retain their existing allocation
behavior; this is not a claim that every nested allocation is fallible. Existing
mock-device tests exercise collector callbacks and cleanup; no global OOM injection
is added. Callback execution and eventual worker-owned destruction retain their
existing task-context requirements.

## Trust model and external boundaries

The audit scope includes production code under `src/` and guest fixtures under
`ut-tests/`. Test modules remain private children of their original parents,
compiled only with `cfg(unittest)` and the existing source/architecture gates.
The layout change adds no production entry point or hardware access path.
Real-device fixtures retain serial execution and borrow boot-owned RNG handles;
the controlled collector fixture retains its serial device-registration and
cleanup sequence.

HwrngHandle does not expose the raw driver Arc. Reads and failure queries
are gated; a withdrawn publication is rejected without calling its hardware.
Unavailable publication and terminal hardware failure both trigger reselection,
but withdrawal does not blacklist the hardware. Unsatisfied demand survives either.
A held consumer reference alone
neither keeps registration active nor grants permission to access the device.

Hardware, firmware and VMM-produced bytes enter through source adapters. This
crate does not access user pointers, MMIO or DMA directly. VirtIO bytes depend
on host trust expressed by enabling `entropy_virtio_rng`; the Hwrng class itself
provides no entropy-quality proof.
The bootstrap seed is not sufficient to mark the pool ready. Secure consumers
must use the readiness contract before treating pool output as securely seeded.

## Memory and thread safety

The VirtIO collector contains no unsafe code. It gives a temporary stack buffer
to `read`; the Hwrng contract requires driver-owned storage for outstanding
DMA. A nonempty success is checked against the buffer capacity before mixing.
State snapshots use a short task-context lock; only the single work callback
changes selected/failed records. All reads, driver failure queries, reference
retirement, mixing and retry sleeps occur outside it. Demand is published before
nonblocking submission; running Work accepts a pending occurrence at its tail.
Subscriber callbacks only enqueue a rescan and restore context before returning;
they do not wait for hardware, old work or device management. Late events carry
no authority to clear a newer selected instance.
Pool mixing completes before readiness is published. Class handles retain Rust
objects, but do not establish hardware resource validity after hot removal.

## Threats and failures

| Failure or threat | Impact | Control and remaining limitation |
| --- | --- | --- |
| Untrusted VMM supplies predictable bytes | Compromised entropy quality | Disable `entropy_virtio_rng` when the host is untrusted; no statistical quality guarantee |
| Hardware never completes | Collector remains blocked | Occupies a shared worker and queue active allowance; no device timeout or reset |
| Read error without terminal device failure | Delayed seeding | Retain demand and sleep 100 ms before retry; log once per demand |
| Confirmed terminal device failure or invalid successful length | Hardware source becomes unavailable | Warn and permanently fail the instance |
| Repeated demands | Excess work/threads | One reusable Work with serialized callbacks and one coalesced pending demand |
| Device removed while a handle survives | Resource lifetime hazard | Closed handles reject new operations; driver stops/wakes readers before unregister and owns hardware cleanup |
| Old event after new selection | Lost demand or wrong source | Events queue a fresh registry scan, never blindly clear selected |
| Terminal instance repeatedly discovered | Repeated invalid input or busy retry | Per-publication failure records; prune revoked instances on the next scan |
| No provider for pending demand | Sleeping consumer cannot request again | Keep demand without requeueing a busy loop; later availability schedules the same work |

## Privacy and audit

Warnings contain errors or lengths, never random bytes. Review trust gating,
readiness publication, buffer-length validation, work reuse, failure logging,
and absence of blocking calls under the source spinlock. These tests establish
functional behavior, not randomness quality or a hard IRQ latency bound.

Initial reseed requests now reach asynchronous adapters even if unavailable;
trust-tier checks remain before dispatch, and synchronous adapters check their
own availability. HostTrusted still requires entropy_virtio_rng, eager boot reads
still exclude it, and no other RNG family is accepted. Test-only sink injection
avoids crediting predictable synthetic bytes to the real pool. A hung hardware
read or endless transient error still prevents this one worker from serving
another provider until the driver wakes/stops that read; this ticket adds no reset
or cancellation guarantee. Failed-instance pruning is eventual after that progress.

VirtIO discovery retains its driver-name filter and removes the independent
publication-name fallback. This does not broaden acceptance to all RNG devices.
The driver name is trusted integration metadata, not cryptographic authentication.
