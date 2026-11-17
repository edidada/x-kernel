# Watchdog Security

## Trust boundaries

Watchdog consumes kernel-owned heartbeat timestamps, task-check registrations,
CPU identities and hardware timer/NMI events. It exposes no user-memory access
interface. Users can affect kernel workload and timing indirectly, but cannot
register checks or provide report pointers through this crate.

The periodic source backend owns counter reservation, capability validation,
interrupt promotion/routing and local quiescence. Watchdog trusts a successful
arming transaction and the backend's NMI-safe quiescence contract.

The optional `WorkqueueStallInspectorIf` provider receives only trusted CPU
identity, time and threshold values. It runs from a sleepable watchdog task
without held spinlocks, may skip busy or unpublished state, and must neither
recover queues nor enter the fatal report protocol. Exactly one provider is
required when the feature is enabled.

## State and resource inventory

- Cache-aligned per-CPU atomic soft/hard heartbeat state.
- A per-CPU task-check vector, immutable after Release publication.
- Retained timer owners and Stop-thread work storage.
- Report owner, frozen target mask, per-CPU admission and ready masks, and a
  monotonic closed flag. These fields have no reset or reuse path.
- A static `ktask::snapshot::TerminalSnapshot` containing one write-once frame
  slot per configured CPU. It owns no heap allocation.

`LockupDetection` embeds its atomic state in `karch::cache::CacheAligned`.
The wrapper changes layout only; atomics provide synchronization and the
alignment policy supplies no DMA or cache-maintenance guarantee.

`NR_CPUS <= usize::BITS` is checked at compile time. Report membership is formed
from successfully armed CPUs, rather than a contiguous firmware CPU count.
The owner is always included. Present but unarmed CPUs are visible as
`NOT COLLECTED`, but are not included in the wait target.

## Unsafe boundaries

The watchdog's per-CPU slot access and timer ownership retain their documented
pinning, IRQ exclusion and lifetime contracts. The task-check vector may be
mutated only before publication, and no further registration is permitted after
arming begins. Every NMI reads the published list through a shared reference.

`init_softlockup_detection` writes its retained timer owner once under
`NoPreemptIrqSave` after local timer/scheduler setup. Timer callbacks never
access the owner slot and rearm only through callback-scoped control. The plain
last-report timestamp is accessed only by the pinned IRQ-masked timer callback.
Stop-work storage remains permanent, and its queue flag excludes reuse before
the Stop callback returns. No hotplug or timer-owner teardown is supported.

Report admission and frame publication add no unsafe storage access. Terminal
frame publication uses `klazy::Once`; a separate atomic first-entry claim ensures its initializer has
only one caller for each CPU, avoiding a wait on an interrupted initializer.
It never borrows ordinary snapshot `UnsafeCell` slots or resets their session.
The coordinator's sole unsafe dump call requires every selected interrupted
stack to remain mapped and unchanged through output; ready responders park,
and the reporter retains its own NMI stack. The unsafe API documents that
copying a frame alone does not retain the stack it references.

## Concurrency invariants

1. Owner election stores the reporter id itself. There is no request-visible
   interval with an unknown reporter identity.
2. Targets are published once after election. A peer seeing no targets does
   not spin or park, and later CPU registration cannot extend the active wait.
3. A CPU claims capture once. Nested IRQ/NMI participation returns immediately
   so the original collector can finish.
4. A ready bit is set only after frame publication. The owner acquires ready
   bits, and the snapshot reader independently acquires each Once publication.
5. Remote responders quiesce their local periodic source and park until reset.
   Their interrupted stacks remain live for backtrace reads.
6. Closing chooses a final collected mask. Already admitted writers may finish
   later in distinct immutable slots; no slot is cleared or reused.
7. Partial reports inspect only collected running stacks. They avoid task
   metadata formatting, which can try ordinary IRQ locks, and do not walk saved
   contexts of tasks that an unresponsive CPU could still migrate or execute.
8. Missing CPUs are not certified stopped. Fatal reporting never authorizes
   cleanup, memory reclamation or a return to normal operation.

## Threat analysis

| ID | Threat | Mitigation and residual limit |
|----|--------|-------------------------------|
| T-01 | NMI interrupts a holder of an ordinary console/IRQ lock | Reporting uses emergency output and never enters the normal panic logger or platform halt logger. It stops through `karch::stop_cpu`. |
| T-02 | A CPU never receives another NMI | Timer IRQs can also collect. The owner waits at most five seconds or a finite poll budget, prints missing CPUs and stops. |
| T-03 | Diagnostic clock stops or regresses | The 50,000,000-poll cap terminates collection independently of elapsed-clock progress; it is not a wall-time estimate. |
| T-04 | A CPU marks arrival before its frame exists | Ready publication follows the completed local frame copy. Readers acquire publication before accessing the slot. |
| T-05 | A late collector races snapshot reuse | Terminal snapshots are separate, write-once and retained until reset. There is no finish/reset/reuse after timeout. |
| T-06 | An ordinary snapshot is already active | Terminal storage and ownership are independent, so the failure is still reported without clearing that session. Output may interleave. |
| T-07 | Nested NMI interrupts a timer-IRQ collector | First-entry admission rejects the nested participant instead of waiting or parking over the unfinished collector. |
| T-08 | A missing CPU continues scheduler activity | Only stable interrupted stacks of responders are expanded. No saved task-context walk or global stop guarantee is inferred. |
| T-09 | Simultaneous failures or arming during a report | One atomic owner wins; losers join that report. Targets are frozen, including the owner, and later registration does not extend the wait. |
| T-10 | Late softlockup diagnostics cause timer catch-up loops | Rearm relative to completion once; out-of-range rearm logs and stops sampling rather than replaying missed deadlines. |
| T-11 | False softlockup reports flood the console | Stop-thread touches distinguish scheduler starvation from a delayed lower-priority task, and reports are limited to one per threshold interval. |

## Failure handling

Unavailable NMI support or failed source arming logs a per-CPU degradation and
leaves softlockup sampling available. Normal softlockup reports remain
rate-limited and do not stop the system.

A failed NMI task check is terminal. The elected reporter prints the failure
identity before waiting, then target/collected/missing masks, available stacks
and a final marker. Report participants halt until reset; the reporter also
halts directly after output. Missing CPUs can remain active, and power remains
on. Emergency console output is best effort; corrupted backtrace state or stalled
hardware output can still prevent report completion. The collection deadline
does not promise a total time bound for all diagnostic code.

The backend's current AArch64 cycle conversion still assumes 2.5 GHz. Actual
sampling intervals depend on the platform, hardware and emulation. That backend
limitation is separate from the heartbeat's monotonic-time comparison.

## Privacy

The initial local heartbeat is seeded before arming, so an IRQ-off lockup before
the first timer IRQ cannot evade detection through uninitialized state.
The x86 peer source is explicitly dependent on another CPU taking timer IRQs;
all-peer stalls require the independent PMU source for periodic sampling.

Reports can expose kernel addresses, CPU state and task identities. Access to
the console and exported logs belongs to platform/deployment policy.

## Audit checklist

- Are source arming and NMI-safe quiescence owned by the backend?
- Are checks immutable before NMI arming and readers paired with publication?
- Are snapshot ready bits published after frame copies?
- Can nested entry return without waiting for an interrupted publisher?
- Are late writers excluded from any reset, reuse or cleanup path?
- Does collection terminate if CPUs or the diagnostic clock stop responding?
- Does a partial report avoid non-running task contexts and normal console locks?
