# PCI virtio-net lifecycle requirements for future migration

## Scheduling decision — 2026-11-04

The user deferred this work to the future migration of network device drivers
onto the new framework. Design, implementation and acceptance of network close
will be performed together with that migration, not as a separate immediate
follow-up or a blocker for the completed RNG-focused R1 work.

The findings and ownership requirements below remain migration acceptance
inputs, not resolved defects. Before starting migration, re-audit the then-current
driver, vendor and KIRQ contracts and incorporate the remaining issues into its
requirements, design and tickets. This scheduling decision does not authorize
vendor changes or claim the existing network teardown is safe.

## Original handoff

Accepted on 2026-11-04 by explicit user delegation to the driver-framework
workstream. This expands the previously RNG-focused scope; the earlier statement
that only IRQ confirmation remained applies to R1-08, not this added work.
Requirements are recorded here before implementation design and ticket splitting.

## Evidence and scope

Reported source baseline: main `7541acb4c`. Read-only supporting audit:
`/root/codes/x-kernel-worktrees/kirq-msi-owner-close/drivers/devices/virtio/docs/design.md`
(PCI Network Shutdown Contract), its sibling `security.md`, and the ignored
local `03-msi-owner-close.md` process record. These are uncommitted evidence,
not delivered fixes. Do not copy the ignored process record into the repository.
At receipt the driver branch was `046bf2fc2`. It has since been rebased onto
main `289e2276b`, with compatibility checkpoint `e1ab737f9` and software IRQ-tail
acceptance `00b4f0fc0`. The reported network audit remains evidence from its
stated baseline; recheck current network/MSI contracts before implementation.

Initial target: existing x86 PCI virtio-net with MSI-X. Preserve legacy/MMIO
behavior and regression coverage. No vendor changes, generic hotplug, affinity
framework or hidden global quarantine service are authorized by this handoff.

## Driver workstream ownership

- Network subsystem: withdraw publication, stop admission/rearming and drain
  owned work, including operations through retained references.
- VirtIO network driver: explicit operational close distinct from final Arc
  destruction; keep posted RX buffers, pools and queue ownership valid until
  device and software access end. Cover failure after every RX posting stage.
- PCI integration: terminal MSI-X suppression distinct from restoring saved
  Enable/Mask bits; cover queue and configuration notification sources. Retain
  MSI ownership independently of raw transport destruction.
- `device_res` and host adapter: expose rejection without consuming the only
  retry owner. Do not overwrite PCI ownership state with Legacy/Failed before
  an unsuccessful release can be represented and resumed safely.
- Device lifecycle integration: a failed close must not silently fall through
  bus removal, devres cleanup and registry removal while losing its owner.
  Resolve this with the framework's removal contract before wiring fallible
  network close. An error return alone is not ownership retention.

The current vendor may already supply the required PCI reset through raw-device
destruction: transport precedes queues and PCI transport Drop writes status zero
and waits for zero readback. Evaluate exclusive extraction/destruction of raw
state outside locks while retaining posted buffers. IRQ aliases must first be
unable to access raw state, and the MSI owner must survive reset and IRQ drain.
This is a candidate based on the audit, not a field-reordering fix or a promise
of recoverable reset timeouts. Pre-raw failures need their own rollback proof.

Workers needing hardware or completion IRQs must drain before those facilities
are stopped. No wait may hold locks needed by the work being drained. Normal
request completion may reclaim its own buffer earlier; teardown must not cancel
or free an already completed request a second time.

## KIRQ workstream deliverables requested

These are required contracts/evidence, not assertions that every capability is
missing or requests for a new API shape:

1. Action withdrawal excludes future snapshots and drains admitted callbacks
   through their full return, including lock-free notification tails. Specify
   claimed/pending interrupt coverage separately from callback completion.
2. Removing one shared action preserves peers and their usable line. Whole-line
   synchronization is acceptable; no per-action wait redesign is required.
3. MSI release transaction: unknown/non-MSI/in-use mappings do not reach backend
   release; rejection preserves the existing allocation/mapping identity and
   does not permit reuse; success removes ownership exactly once. State backend
   failure atomicity, execution context and locking requirements.
4. Allocation rollback before returning an owner: if composition or mapping fails
   and backend rollback is rejected, identify the retained owner and recovery
   contract. A consumer cannot retain a handle it never received.
5. Specify prerequisites for vector reuse after already-issued MSI messages,
   and which guarantees belong to device reset/PCI ordering versus the interrupt
   core/backend. Software drain or an allocator bit is not hardware retirement
   proof. Explicitly report any missing core/backend capability; do not fill the
   gap with a delay or device-specific reset logic in KIRQ.
6. Provide intended stable APIs, delivered revisions and positive/negative SMP
   evidence, including a reusable test-only dispatch seam if one is delivered.
   Do not count old tests as current acceptance without revalidation.

KIRQ does not own network close, device reset, posted buffers, worker drain or
consumer retry ownership. Driver-side `NativeIrq` integration and the concrete
RNG notification-tail tests remain with this workstream.

Items 1/2 now have driver-side software integration evidence in
[IRQ tail acceptance](irq-tail-acceptance-2026-11-04.md), including a real blocked
release task, retained resources, shared-peer preservation and a failing drain
bypass control. This does not discharge items 3–5 or establish PCI notification
retirement; the synthetic dispatch has no physical MSI/EOI obligation.

## Design and acceptance gates

Before coding, reconcile baseline changes and decide the owner that survives a
failed close/rollback, the removal commit point and resumable cleanup phases.
Do not invent a global retention table to bypass that decision. Then split
implementation into reviewable tickets with explicit KIRQ dependencies.

Required tests include RX-posting construction failures, all initial MSI-X
Enable/Mask combinations, rejected release followed by retry of the same owner,
retained references, concurrent I/O/work and removal, queue/config callbacks,
and no premature vector reuse. Keep deterministic software-order evidence
separate from device-write/MSI ordering proof and actual hardware tests.

Linux reference supplied with the handoff:
`8d3ae59288f1e7d58d76558a6ee96d533bc5019f`, `virtnet_remove`,
`remove_vq_common`, `vp_reset`, `vp_synchronize_vectors`, and `vp_del_vqs`.
Inspect these implementations during design; this receipt does not claim a new
Linux audit or establish equivalent X-Kernel hardware guarantees.

Status: deferred to network-driver migration; no production implementation or
acceptance completed by this handoff. The other worktree and vendor remain
untouched.
