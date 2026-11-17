# x86 native NMI and periodic sources

The platform implements mechanism initialization and source-neutral periodic
arming. `NativeNmiSourceIf` provides narrow producer/controller operations to
the x86 IRQ adapter; KIRQ retains both source registrations, local action
publication, NMI/RCU context, disable/teardown and retirement.

Native vector 2 is not a PMU IRQ identity. The adapter probes notification and
periodic causes independently and dispatches source identities 100h/101h through
KIRQ. Cookie zero explicitly carries no LAPIC ISR/EOI obligation. A notification
request never fabricates PMU overflow status. An unrelated NMI claims no action.

Two explicit periodic implementations are selectable:

- PMU: Intel architectural fixed reference cycles, or supported AMD core-cycle
  counters through the OS-neutral x86-pmu register crate. Unsupported PMUs log
  per-CPU degradation and do not silently switch mechanisms.
- Peer timer: SMP-only maskable timer on each armed CPU samples the next armed
  peer via NMI IPI. The receiver can have ordinary IRQs disabled. This supports
  x86 TCG, whose accelerator does not implement an x86 vPMU; it cannot detect
  all CPUs simultaneously losing timer progress. `qemu_watchdog_defconfig`
  selects it explicitly. On x86 KVM/physical systems, PMU is the independent
  local sampling source.

Runtime registers source actions once through PeriodicNmiSourceIf::register_source
before activating sources. Local arming reserves a source, prepares both disabled
KIRQ actions, publishes callback/period, enables delivery and starts production.
Peer membership includes only armed CPUs. PMI completion checks armed admission
before reload so a callback's quiescence is never undone. NMI notification sends
publish a per-target request before ICR submission. Busy sends fall back to the
next timer/NMI and never remove the report's collection deadline.

Global shutdown closes starts before retaining both original KIRQ Lines. Partial
claim acquisition, local stops and finalization remain in a retryable Closing
state. It stops every boot participant synchronously before clearing callbacks
and retiring handlers. Restore retains partial registration progress, then
reopens starts only after both actions exist. Peer timer storage is permanent. Quiescence closes its producer flag; a
pending one-shot may complete once and sees the closed flag before rearming.
Shutdown retains that timer owner for reuse rather than waiting from the pinned
IPI callback context.

The native entry is in kcpu, not the platform. Local irq_work uses permanent
ordinary vector F4h. An NMI publication finding an interrupted ICR writer leaves
a pending flag for that writer to submit before returning. Accepted ICR commands
have bounded hardware-only completion polling; stuck delivery stops locally
without invoking panic/console locks. The notifier never waits for an interrupted
software holder.
