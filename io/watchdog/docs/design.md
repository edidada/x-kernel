# Watchdog Design

## Purpose and scope

`watchdog` detects stale per-CPU scheduler and interrupt heartbeats. Soft-lockup
checks use timer IRQs and the Stop thread. Optional hard-lockup checks run from
a periodic NMI source. The source's hardware programming remains in the platform
backend behind `kplat::nm_irq::PeriodicNmiSourceIf`.

The crate owns detection policy, the published per-CPU check list and fatal
report coordination. It does not own interrupt-controller claims or PMU counters.

## Components

- Soft lockup: each CPU's timer IRQ queues `stop_one_cpu_nowait` so the
  Stop thread updates a timestamp; the same callback checks whether that
  timestamp has expired. A touch that never runs while IRQs still fire is
  the softlockup signal.
- Hard lockup: NMI support (the `nmi` feature) periodically checks whether the local
  CPU's timer-IRQ heartbeat timestamp is older than the hard-lockup threshold.
  A failed check starts a bounded terminal report and stops the participating CPUs.
- `src/init.rs` installs the timer IRQ hook, sampling timer, watchdog task and
  optional periodic NMI callback on each initialized CPU.
- `src/lockup_detection.rs` owns the cache-aligned per-CPU heartbeat state.
- `src/watchdog_task.rs` publishes the local check list once before NMI arming.
- `src/rendezvous.rs` coordinates a terminal best-effort multi-CPU report.
- `src/rendezvous/state.rs` implements report admission and bounded polling using
  atomics without architecture or clock dependencies.
- `ktask::snapshot::TerminalSnapshot` owns the report's permanent copied frames.

## Execution contexts

| Entry | Context and contract |
|-------|----------------------|
| `init_primary`, `init_secondary` | Once on the pinned booting CPU after local scheduler/timer setup. |
| Watchdog sampling timer | Local timer IRQ, ordinary IRQs masked; queue Stop work, check softlockup and rearm without sleeping. |
| Timer IRQ note | Every local timer IRQ; refresh the hardlockup heartbeat. In an active fatal report, collect the current frame and park terminally. |
| Stop-thread touch | Pinned Stop thread with IRQs masked; update the softlockup timestamp without sleeping. |
| Watchdog task | Sleepable pinned kernel task. Optional workqueue inspection runs here, never in NMI. |
| Periodic NMI | Check the immutable local task list or join an existing report. No allocation, normal console lock or scheduling. |
| Fatal report coordinator | The first failing CPU's NMI, pinned with ordinary IRQs masked throughout collection, output and final stop. |

## Detection

Softlockup means that this CPU's Stop-thread touch has not run for more than
20 seconds even though timer IRQs continue. Each CPU retains a `ktask::Timer`;
every four seconds its handler queues `stop_one_cpu_nowait`, checks the timestamp
and rearms relative to handler completion. Softlockup reports print scheduler
statistics and task backtraces, rate-limited to one per threshold interval.

The initial hardlockup heartbeat is seeded during local watchdog initialization
before source arming, so losing interrupts before the first timer IRQ is also
detectable. Subsequent IRQs refresh it normally.

A separate pinned watchdog task sleeps for four seconds between optional
workqueue-stall inspections. It does not update the softlockup timestamp.

Hardlockup means that the latest local timer IRQ timestamp is more than ten
seconds old. Every timer IRQ refreshes that heartbeat; the check uses monotonic
wall time, not a count of PMU overflows or four-second sampling callbacks.
The default NMI request is ten seconds; PMU backends translate it to a cycle
budget, while the explicit x86 peer backend schedules another CPU's timer.
Actual delivery cadence and independence are properties of the selected source.

The local check list is mutable only during initialization. Release publication
precedes NMI arming; NMI readers acquire publication before traversing the now
immutable slice. Arming failure leaves the list published but excludes the CPU
from the report's watched membership. An unavailable NMI mechanism or failed
arming is logged explicitly.

## Terminal report protocol

The report is one-shot and has no reset or resource-reclamation path:

```text
Dormant -> elected owner, targets not yet published
        -> collecting a frozen target set
        -> closed with a final collected mask
        -> output and terminal CPU stop
```

1. Only successfully armed CPUs register in `WATCHED_CPUS`. The first failed
   check atomically installs its own CPU id as owner. That owner freezes the
   registered mask and includes itself even if failure interrupted its arming
   transaction before registration completed. Membership is not inferred from
   firmware CPU count, and later initialization cannot extend the wait.
2. The owner publishes targets with Release. Peers acquire that publication;
   a peer observing election before targets are ready returns to the interrupted
   path and can retry at its next timer IRQ or NMI. It never waits for setup.
3. A per-CPU claim bit admits only the first entry. In particular, an NMI that
   interrupts an ordinary timer-IRQ collector returns immediately rather than
   parking the CPU before the interrupted collector can publish.
4. Each admitted CPU quiesces its local periodic source, copies its current
   exception frame into its dedicated `TerminalSnapshot` slot, then publishes
   its ready bit with Release. Remote CPUs immediately enter `karch::stop_cpu`;
   they do not wait for a dump-completion flag or resume normal execution.
5. The owner waits for at most five seconds or 50,000,000 unsuccessful polls,
   whichever limit is reached first. The finite poll budget still terminates
   collection if the monotonic clock stops or regresses. Ordinary timer IRQs
   let healthy idle CPUs participate without waiting a full PMU cycle budget.
6. The owner closes new collection admission and acquires a final ready mask.
   It prints target, collected and missing masks. CPUs that enter after close
   park without collecting. An already admitted collector can finish late;
   its immutable slot is not reused and is omitted if absent from the frozen mask.
7. The owner prints the saved interrupted PC/FP and only the running stacks
   selected by that mask. AArch64 uses saved ELR for the interrupted PC and LR
   only as the unwinder's return-address fallback. Raw address output bypasses
   optional in-kernel symbolication.
   All other present CPUs are explicitly reported as `NOT COLLECTED`. It does
   not look up or format task metadata (task-name formatting can try an IRQ
   lock), and does not walk saved contexts of non-running tasks: an unresponsive
   CPU could still migrate or execute such a task during partial collection.
8. Output uses `kprint_atomic!`. After the final report marker the owner enters
   `karch::stop_cpu` directly. The normal panic handler and platform halt hooks
   can take ordinary console locks, so neither is invoked from this fatal path.
   The result is a halt with power retained, not the previous panic/power-off.

`TerminalSnapshot` is independent of ordinary sysrq snapshot sessions. An active
ordinary snapshot cannot suppress a watchdog failure or have its frame slots
cleared by a terminal report. Terminal slots use existing `klazy::Once` storage;
there is no new unsafe publication cell.

## Concurrency and lifetime

Report ownership and each CPU's capture admission are monotonic. Acquiring the
ready bitmap observes the frame published before its bit was set. The snapshot
also checks each slot's own Acquire publication before reading it. Masked-out
slots are never read. No global clear, `nmi_finish` or report reset follows a
partial collection, so late writers cannot race reclamation or reuse.

Participating CPUs retain their interrupted stacks until reset. Missing CPUs
have not been proven stopped; a completed report is not a system-wide freeze or
cleanup barrier. Normal-operation timer hooks remain short and non-blocking;
only fatal-report participation enters the terminal stop path.

## Validation and limitations

`tests/report-protocol` imports the production atomic protocol for host tests of
concurrent ownership, sparse membership, nested entry, publication, late writers
and stopped/regressing clocks. Kernel builds and watchdog unit tests validate
integration through Makefile/Kconfig.

The owner requests prompt peer samples through the selected source. x86
publishes per-target notification requests and sends native NMI IPIs; current
AArch64 returns unavailable and observes the request at the next timer IRQ/NMI. The protocol accepts
partial collection and is suitable for adding such notification later without
making it a completion guarantee. The time/poll limits bound collection waits;
backtrace formatting and hardware emergency output remain best effort and are
not given a total execution-time guarantee.
