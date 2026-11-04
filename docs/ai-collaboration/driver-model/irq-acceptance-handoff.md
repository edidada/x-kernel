# Remaining IRQ acceptance handoff

Checkpoint: driver branch `ca8e22486`, 2026-10-25. This is a read-only evidence
reconciliation and execution plan, not a new IRQ implementation or a completed
G-IRQ gate. It preserves the approved synchronous release contract.

## Revalidation baseline — 2026-11-04

The driver branch is now based on main `289e2276b`, with compatibility checkpoint
`e1ab737f9`. Current KIRQ retains dispatch admission through the complete action
callback and drains the original Line after withdrawing the exact action token.
The missing evidence is native adapter and RNG notification-tail integration,
not a request to replace that production mechanism. The older independent
worktree below remains historical evidence and is not imported.

Revalidation uses one KIRQ-owned test-only synthetic dispatch entry, exercising
the production admission, action snapshot and drain without a fabricated
controller claim. NativeIrq registration and release remain real. The two
integration layers below are the agreed acceptance boundaries. A temporary
negative-control mutation must bypass only the fixture-owned Lines' release
drain, leave real IRQ Lines unaffected, and be restored before final validation.
An assertion detecting early release is required; a hang is not success.

The current test implementation and execution evidence are recorded in
[Native IRQ/RNG tail acceptance](irq-tail-acceptance-2026-11-04.md).

## Current evidence and missing dependency

The independent `xiaohui/irq-sync-acceptance` worktree is still based on
`11fd96a60` with uncommitted changes. Its `arch/kirq/docs/irq-sync-acceptance.md`
records 247 passing tests and a negative control that omitted the in-flight
wait. Those are historical results, not a delivered commit or a fresh run on
the driver branch. This audit neither changes that tree nor imports its patch.

Its two SMP lifecycle tests exercise real kirq action fanout and scheduler
waiting, using synthetic hardirq context and a resource lifetime sentinel.
They do not call `NativeIrq`, construct a concrete RNG runtime, deliver a real
controller interrupt, or exercise RNG's completion notification tail.

Current `kdriver/ut-tests/host_irq.rs` checks adapter argument/error conversion
using injected closures. Those tests do not exercise the real synchronous
release wait. Real MMIO lifecycle, isolation and withheld-response runs remain
valid within their recorded scope, but do not fill this concurrency gap.

## Ordered delivery

### 1. IRQ core prerequisite

On the current baseline, audit the delivered core and validate ordinary/shared
action release, actual blocking, resource-drop order, peer preservation,
repeated release and the negative control. The independent historical patch is
not itself a required dependency when current-core integration supplies this
evidence. Preserve the distinction between software fanout and controller
delivery. An ignored SMP case is not acceptance.

The existing candidate synthetic-dispatch helper belongs to kirq's test-only
support: it must enter the scoped hardirq context, use the production action
snapshot/in-flight guard and avoid acknowledging or completing a nonexistent
hardware claim. Reuse a delivered seam when available; otherwise add only this
test-only entry in KIRQ, rather than maintaining a second driver-private IRQ
dispatcher or copying the entire unfinished external patch.

The current public `generic_handle_irq` is not an alternative test seam: its
contract requires an active irqchip invocation and a genuine pending claim.
Calling it with a fabricated cookie would also invoke controller completion.

### 2. Native adapter integration

On the driver branch, use a fixture-owned software descriptor and register two
shared actions through `NativeIrq` itself. Descriptor setup must satisfy its
preflight without touching a real device's routing or platform enable state.
Dispatch on a second CPU and hold action A in a lock-free callback tail.
Release A through the native provider on a sleepable task and observe that task
actually blocked while a resource sentinel is still retained. After allowing A
to return, require one release, then dispatch again and require only B to run.
Finally release B. This proves the adapter/core/wait chain, not a hardware IRQ.

Use explicit atomic handshakes and fixture cleanup, not elapsed-time assertions.
On early failure, open the handler gate before joining tasks and removing
remaining actions. A host watchdog only reports a hang; it is not a teardown
timeout policy. Do not add a new IRQ registry, per-action waiting algorithm or
production testing hook to obtain this evidence.

### 3. Concrete RNG notification tail

Exercise the concrete RNG callback with the native IRQ provider. The existing
`RngCompletion` injection boundary can distinguish a notification originating
in hardirq from the task-side close notification. A test-owned, non-sleeping
gate may hold the callback after its request lock has been released, before
returning from notification. Do not gate inside `SyncWake::notify`, hold the
request/completion lock while waiting, or hold task-side close notifications.

Start teardown in that interval. For a normally completed request, require
synchronous IRQ release and destruction of the queue and remaining resources
to wait for the callback tail. The completed token may already have been
reclaimed before notification; it must not be cancelled or released again.
For a separate case with a genuinely outstanding token, establish that
precondition and verify cancellation exactly once after reset confirmation and
IRQ drain. Preserve a peer shared action. Record whether the
transport/backend and interrupt delivery are controlled or real; a synthetic
run cannot be relabeled as GIC delivery, EOI, late hardware response or DMA
quiescence evidence. Any remaining hardware case stays explicitly open.

## Exit decision

Each layer needs its own source revision, configuration, command, result and
negative-control rationale. The core prerequisite's delivery and driver-level
integration are distinct obligations. Only reconcile G-IRQ and R1-07/08 after
the required evidence is present; R1-09 retirement remains gated. This handoff
does not authorize modifying another owner's dirty worktree or pushing changes.
