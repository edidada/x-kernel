# Two-phase wake protocol mechanism

## Scope and roles

kwake defines the source-side Signal contract and the owned values passed to
host delivery. No existing completion, poll, lock or scheduler API is migrated.
Host queues, runtime admission and standard Waker bridges remain separate work.

File responsibilities:

- src/lib.rs exports the public protocol and defines Signal, WakeDelivery and
  the delivery admission capability.
- src/value.rs owns target preparation, registration ownership and set transfer.
- src/adapter.rs privately implements KBox raw-owner conversion and link
  projection. It is an implementation detail of the values, not a public API.

- WakeTarget retains the callback identity and independently owned dependencies.
- WakeRegistration owns prepared storage for one independently selectable wait.
- A source implements Signal, serializing selection with its own enrollment and
  cancellation state, and transferring selected registrations into a WakeSet.
- WakeSet exclusively owns the selected notifications until delivery or Drop.
- The host supplies WakeDelivery callbacks and admits a DeliveryCtx at its
  delivery boundary. The ordinary signal caller carries the value out of locks.

## Preparation, selection and delivery

WakeTarget retains an existing Arc<dyn WakeDelivery>. The caller supplies the
already-prepared shared callback; target construction does not allocate or
wrap it in another owner. Cloning a target only increments the Arc reference
count. Callback storage creation follows the caller's allocation policy.

prepare_registration allocates a KBox containing a list link and a target clone,
returning KResult. The default uses AllocFlags::KERNEL in sleepable, reclaim-safe
task context. prepare_registration_with_flags applies caller-selected permissions
to the node allocation; invalid flags or allocation failure propagate without
publishing a registration. NOWAIT does not grant NMI safety or bounded latency.
Flags do not govern callback storage or destruction. Prepare before source
exclusion.
Sources may keep each registration in an Option within their own waiter record;
selection takes that value under the source lock and moves it into WakeSet.
The registration has no Clone or borrowed enqueue API, so it cannot be selected
into a second set. Reenrollment after selection requires fresh prepared storage.

Push and append transfer ownership without callbacks, allocation, reference-count
operations or queue-claim atomics. Source adapters own condition publication,
list membership, cancellation synchronization and registration storage. Selected
entries retain no borrowed source or waiter-stack pointer. They stay valid after
source or waiter retirement, including harmless late notification after cancel.

After releasing conflicting locks, a host explicitly calls WakeSet::deliver
with an admitted DeliveryCtx. Each entry is unlinked, its callback is invoked,
then its node storage is freed. A target may be delivered concurrently through
independent registrations; mutable callback state requires host synchronization.
Delivery returns the number of delivered registrations. WakeSet::len counts
actual owned entries, not event occurrences or consumable completion tokens.

## Independent sets and storage

Queue nodes belong to registrations, not targets. Two registrations referencing
the same target can belong to separate sets. Delivering either executes its own
notification regardless of whether the other set was delivered or dropped.
There is no cross-set coalescing, global queued flag or hidden delivery owner.
If one registration was already selected, a subsequent signal cannot select it
again; it can only select other or newly enrolled registrations.

WakeSet uses an intrusive list of uniquely owned KBox entries. Push is O(1),
append splices lists in O(1), and delivery/cleanup is O(n). The private link never
escapes its owning registration or list. KBox ownership and mutable list access
provide exclusion; no manual Send/Sync implementation is needed in this crate.
The private WakeAdapter implements the library's PointerOps using matching
KBox::into_raw/from_raw transfers, and projects the private links field through
a single consistent field offset. Collection removal/destruction therefore
recovers the kernel owner and never frees its storage as a standard Box.
Transferring sets across CPUs still requires the host's ordinary synchronized
handoff, which also publishes the source's preceding state changes.

## Execution context and lifetime

Empty construction, selection and append need no current task or CPU-local
state. They may run under source exclusion once registrations are prepared.
Registration preparation requires a live allocator and obeys its context
requirements. Retaining an existing callback as a target does not allocate.
Cancellation and final release free storage and may run a callback
object's destructor; the host must choose an appropriate teardown context.

Dropping a registration or set never invokes WakeDelivery::deliver. It cancels
only the entries it owns and can lose progress; must-use does not prevent
Drop/forget. Sources and callers must arrange eventual delivery of successful
notifications. Callback ownership cycles remain a host lifecycle responsibility.

DeliveryCtx is neither Send nor Sync. Its unsafe constructor is a host admission
contract, not a runtime check or M2 compile-time proof. No kernel adapter is
installed here, so IRQ admission and scheduler progress are not established by
this mechanism. M2 proofs, M3 park-state changes and M4 bandwidth work remain
outside this patch.
