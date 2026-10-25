# entropy — Design

## Fallible work callbacks

The VirtIO collector uses `KBox<dyn Fn() + Send + Sync>` with default KERNEL
allocation. Boot initialization runs from kruntime's late-init task; source::init_all
iterates static sources without a lock. Class availability callbacks are dispatched
after releasing the class registry and driver-core locks. note_device releases its
SELECTED_SOURCE/collector-state checks before constructing a source and reacquires
the selection mutex only for publication. Preparation requires sleepable,
reclaim-safe task context and rejects non-sleepable callers before owner allocation.
No NOWAIT restriction is needed at this lock-free construction point; callers must
preserve the reclaim/IO/filesystem re-entry contract on future notification paths.

## Scope

The crate owns the ChaCha20 entropy pool and source-selection policy. Sources
are CPU instructions, SMCCC TRNG, VirtIO RNG and software jitter, each gated by
its Cargo/Kconfig feature. `source.rs` owns the common trait, availability and
trust types, registry, selection functions and shared read helpers.
The four `EntropySource` implementations live in `source/arch_cpu.rs`,
`source/smccc_trng.rs`, `source/virtio.rs` and `source/jitter.rs`.
They delegate to the existing crate-level backends; `virtio.rs` adapts the
Hwrng class into asynchronous pool replenishment.

Consumers retain the operation-only HwrngHandle facade, not generic
driver-class payload handles. The backend depends on subsys-hwrng only.
Every read and failure query acquires a permit; withdrawal returns BadState.
The collector leaves a terminal or withdrawn instance without losing its
outstanding demand. Fixtures explicitly unregister providers before shutting
down their private collection work.

## Architecture and execution context

Initialization mixes eligible eager sources and registers VirtIO discovery.
VirtIO initialization prepares work, subscribes to available and removed events,
then queues enumeration. It does not synchronously read hardware during probe or boot.
Enabling `entropy_virtio_rng` trusts host-provided entropy for pool seeding.

Entropy prepares one reusable `kworkng::WorkHandle`, independent of provider
lifetimes, after normal workqueue services are ready. A demand queues a callback
that performs blocking `HwrngDevice::read`, mixes the result, and returns. No
dedicated entropy task or idle demand wait remains. An unresponsive device or
persistent retry errors can still occupy one shared worker and its queue active
allowance indefinitely; this migration does not add completion timeouts.

## State and concurrency

`COLLECTION` retains the prepared `EntropyCollection` behind an initialization
mutex. Work captures the collector, which never owns Work. Its task-context
`SpinNoPreempt<CollectorState>` holds coalesced demand, the selected handle and
failed-instance handles. Only the serialized worker changes selection/failures;
requesters only set demand. Readiness events merely enqueue a rescan: they never
clear a selection based on an old event, wait for work, or acquire management locks.
Subscriptions retain Weak collection references, not per-provider work owners.

Demand is published before nonblocking submission. Work permits one pending
occurrence while running, including after the callback's final state check.
AlreadyQueued coalesces requests. Other submission errors are logged without
discarding demand; later requests/events can retry. Production never shuts down
this kernel-lifetime work. Work preparation failure leaves initialization unset
so a later init can retry; it installs no partial subscriptions.

At each collection pass, the worker enumerates the registry outside its state
lock, preserves a still-eligible selected instance, otherwise selects another
virtio-rng instance. Failed records are pruned against the current registry;
old references and records are dropped outside state locks. No read, hardware
failure query, mixing or retry sleep holds the state lock. Without a source,
the worker returns with demand set, rather than polling for registration.
The pool mutex protects mixing; readiness is published after the mix completes.
The source's synchronous sampling method requests work even when currently
unavailable, then returns `NoEntropy`. `read_all_available` admits requests by
trust tier, leaving availability validation to each adapter. Synchronous adapters
already reject unavailable hardware; asynchronous VirtIO retains its first demand.
Eager sampling still excludes HostTrusted sources, and trust policy is unchanged.
After a reseed attempt yields no immediate sample, the next attempt still waits
for another output interval; the pending VirtIO read does not trigger a new
request on every pool read.

## Failures and resource lifecycle

A read error only permanently fails the source when `HwrngDevice::is_failed`
confirms a terminal device failure. Otherwise the same worker retains its
current demand and retries after a 100 ms sleep, without waiting for another
pool read or getrandom call. Repeated failures log once per demand and never
busy-loop. This delay backs off failed read setup; it does not time out or poll
an in-flight device request. Invalid successful lengths remain contract
violations and permanently fail the source. New demands still coalesce while
recovery runs. Context errors are not evidence of device failure; this worker
always calls read with no source/device lock in sleepable task context.
Retries have no failure-count cutoff; a persistent host failure can still
prevent seeding, but cannot establish a terminal device failure.
Withdrawal is not terminal hardware failure. Either withdrawal or terminal failure
restores the active demand before reselecting, so an already registered sibling
can satisfy it without another getrandom or registration event. A terminal instance
is not retried on rediscovery; records cover only instances in the last registry
snapshot and are pruned at the next pass after removal. There is no historical
name/path blacklist. New publication instances remain eligible.

The collector does not stop/reset hardware or release DMA. Drivers must stop and
wake reads before synchronous unregister; removed notification occurs after drain
and is not what wakes the blocked read. This supports capability handoff, not
proof that the legacy concrete VirtIO driver implements hardware hot removal.

## Validation

Guest unit tests live in crate-root `ut-tests/` and are mounted with
`unittest::path_mod_test!` in their original private parent modules.
All seven modules retain `cfg(unittest)`, their module names and parent access.

| Test file | Module | Cases | Additional gate |
| --- | --- | --- | --- |
| `ut-tests/lib.rs` | `tests` | 12 | None |
| `ut-tests/source.rs` | `source::tests` | 8 | One case requires `entropy_virtio_rng`; existing in-body source gates remain |
| `ut-tests/arch_cpu.rs` | `arch_cpu::tests` | 2 | `entropy_arch_cpu` |
| `ut-tests/smccc_trng.rs` | `smccc_trng::tests` | 2 | `entropy_smccc_trng` and AArch64 |
| `ut-tests/jitter.rs` | `jitter::tests` | 5 | `entropy_jitter` |
| `ut-tests/virtio.rs` | `virtio::tests` | 10 | `entropy_virtio_rng` |
| `ut-tests/virtio_real_device_tests.rs` | `virtio::real_device_tests` | 3 | `entropy_virtio_rng` |

There are 42 cases across all gates. Controlled work fixtures and real-device
cases are serial; the name filter test needs no serialization. No cases carry a
`user` flag. The real-device module remains
registered, requires a published VirtIO RNG device, and fails rather than skips
when no device is present. Use the configured kernel guest unittest workflow;
these files are not host Cargo integration-test targets.

Controlled tests run real Work and stack-scoped Completion waits: idle/in-flight
withdrawal, a preexisting sibling, late old events, absent-source retention,
filtering, terminal/invalid instances, pruning, transient retries, coalescing,
reuse and request/registration at callback return. Private test hooks run only
under cfg(unittest). Five-second watchdogs detect lost progress, not performance;
an outer guest timeout isolates an arbitrarily stuck implementation. Fixtures stop
all synthetic readers before unregister and shutdown, including on assertion failure.
Test sinks never mix synthetic bytes into the real pool. One serial test briefly
overrides COLLECTION with RAII while invoking the real read_all_available→VirtIO
adapter path, restores it before waiting, then proves a single later registration
satisfies the retained demand. The harness runs serial tests before parallel tests.
Private static test registries retain only bounded inert Weak subscriber closures
after fixture destruction because hwrng subscriptions currently last for registry
lifetime. `kcompletion` is cargo-shear-exempt solely for these external UTs;
production entropy uses ktask/kworkng and does not implement a new wait primitive.
Real-device QEMU tests cover empty/short reads, destination guards and repeated
queue reuse. A separate VirtIO-only guest getrandom test exercises initialization
and replenishment without other credited entropy sources.

## VirtIO source configuration

`KFEAT_DRIVER_VIRTIO_RNG` only enables the concrete driver and Hwrng class.
`KFEAT_ENTROPY_VIRTIO_RNG` independently enables this crate's VirtIO source
adapter and depends on the driver switch in Kconfig. `entropy` consumes the
Hwrng contract and class registry, not the concrete driver crate.
Enabling the source also trusts host-provided bytes: they are mixed into the
pool and may establish readiness without another entropy source. There is no
separate trust switch. All QEMU/crosvm defconfigs enable the driver and source
to preserve their existing behavior. Isolated deployments that do not trust
the host must disable the source; the driver may remain enabled.
When migrating a configuration that formerly enabled the source but disabled
host trust, disable `KFEAT_ENTROPY_VIRTIO_RNG` to preserve that policy.

VirtIO source selection uses only the source driver_name containing virtio-rng;
there is no publication-name fallback. The existing production bridge already
provides that driver identity. Other RNG drivers are not automatically trusted
by this change, and source selection still compares publication instances.
