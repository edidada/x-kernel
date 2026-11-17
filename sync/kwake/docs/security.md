# Wake protocol safety and reliability

## Trust boundaries

Sources and the linked host are trusted kernel code. This crate consumes no
user pointers, firmware, MMIO, DMA, network data or hardware registers.
WakeDelivery is an unsafe callback contract: independently owned dependencies,
IRQ-safe nonblocking execution, no unwinding/task switch, and host lock ordering.
Independent registrations can invoke one target concurrently; callbacks must
synchronize their mutable state.

## Ownership and unsafe boundaries

WakeTarget retains the caller's existing Arc<dyn WakeDelivery> without a new
allocation. Cloning preserves callback ownership; final release obeys the
callback's allocator/destructor constraints. Each WakeRegistration owns a
private KBox<WakeEntry> with a distinct
intrusive link. The registration cannot be cloned; WakeSet::push consumes it.
Push, append, delivery and Drop therefore operate on exclusively owned list
entries, with no aliased queue node or global queued flag.
WakeAdapter in src/adapter.rs supplies KBox ownership conversion to the existing
intrusive collection. Its unsafe PointerOps implementation transfers the sole
owner with KBox::into_raw and reconstructs it exactly once with KBox::from_raw,
using the same allocation, type and provenance. It must never recover a standard
Box pointer as KBox or vice versa. The unsafe Adapter implementation projects
the private links field and subtracts the same offset to recover the containing
live entry. KBox keeps its allocation stable until the collection unlinks and
returns/destroys its owner. Private consuming APIs prevent duplicate insertion;
no manual Send/Sync implementation is needed.

WakeDelivery in src/lib.rs requires the host implementation to retain all
callback dependencies independently of source/stack storage. DeliveryCtx's
unsafe constructor in that file requires the host to establish and preserve
admission. No local runtime check establishes absence of conflicting locks.
The host's synchronized handoff publishes source state when crossing CPUs;
the queue mechanism does not supply an implicit memory barrier for unrelated
source predicates.

## Failure modes

| Failure | Effect | Control and residual limit |
| --- | --- | --- |
| Registration selected into two sets | Duplicate ownership or list corruption | Non-cloneable owned registration, consuming push and private links. |
| Notification depends on another set | Lost or delayed progress | Every set owns distinct registration nodes; targets carry no queue membership. |
| Callback references retired storage | Use after return | Unsafe callback contract requires independently retained dependencies. |
| Delivery under conflicting locks | Deadlock | Host admission contract; no kernel adapter is installed yet. |
| Callback state races concurrent delivery | Corruption | Send + Sync callback contract and explicit concurrent-delivery obligation. |
| Dropped or forgotten set | Lost progress | Must-use result and eventual delivery obligation; no automatic callback on Drop. |
| Invalid allocator/destructor context | Context violation | Preparation and teardown obey host resource constraints. |

Selection and append allocate or free no storage. Preparation fallibly allocates
one KBox node per registration; delivery/cancellation frees it. Failed node
allocation publishes no registration and retains the original target. Target
construction retains an existing Arc without allocation. Node allocation flags
do not constrain callback storage or its eventual destructor. No user payload
is logged.
Kernel IRQ admission, scheduler progress and source-specific wait/cancel races
remain obligations of future adapters.
