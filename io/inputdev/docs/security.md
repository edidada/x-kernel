# inputdev — Security and reliability

## Trust boundaries

This analysis covers the whole crate, which currently consists of
`src/lib.rs`. No source module or test-only path is excluded.

## Trust model

The crate receives class handles and removal ids from trusted kernel driver
infrastructure. `devnode` and `kclass` create and validate those handles and
ids; `inputdev` trusts that contract and does not independently revalidate
their provenance or internal identity. Its local validation is limited to
deduplicating registration by id and removing the matching lifecycle entry.
It has no user-memory, MMIO, DMA, network, or filesystem boundary and contains
no unsafe code.>>>>>>> 9f18a37dd (docs: purge old-framework references from every module's docs)

## Invariants and thread safety

- One driver-core id has at most one endpoint and one active poller.
- The hard-IRQ notifier only schedules work; driver reads and listener calls
  happen in task context.
- No endpoint lock is held while reading the driver or invoking a listener.
- A partial input frame contains at most 256 events. Device-wide key state is
  updated only for codes within the key bitmap.
- A detached endpoint cannot accept new listeners or distribute new frames.
- Removal stops the notifier before waiting for the poller to finish.

A timestamp pair is sampled before taking the endpoint state lock. The
underlying timekeeper releases its correlation lock before endpoint or listener
locks are acquired. Once captured, neither later wall-clock changes nor a slow
listener can alter either timestamp. Synthesized loss records copy the report's
pair, so resynchronization does not silently change clock domains.

## Threat analysis

| ID | Threat | Impact | Trigger | Control |
|----|--------|--------|---------|---------|
| T-01 | Device floods events | Worker starvation | The device continuously completes descriptors | The work callback reads at most 64 events per round and requeues after two rounds. |
| T-02 | Device omits `SYN_REPORT` | Unbounded frame memory | Malformed input stream | A 256-event limit discards until the next report and emits `SYN_DROPPED`. |
| T-03 | Device reports an out-of-range key code | Bitmap corruption | Hardware-origin `EV_KEY` code exceeds `KEY_COUNT` | Check the code before changing the bitmap. |
| T-04 | Slow or absent file reader holds the hardware queue | Input stalls for every reader | Events remain in the VirtIO used ring | The endpoint drains the driver independently of file readers; each listener bounds its own queue. |
| T-05 | Removal races a scheduled event read | Freed transport or stale callback | Device removal while work is pending | Detach removes the notifier, shuts down and drains the work handle, then detaches listeners. It runs on removal notification and does not ensure that the worker stops before driver-resource cleanup. |

## Failure handling and limitations

Work-handle preparation or notifier attachment errors prevent endpoint publication and
are logged. An event read error ends the current poll round; a later device
notification may retry it. The input layer keeps key state after a client
queue overflows so userspace can query current state during resynchronization.
The input layer does not interpret unknown event categories or implement
evdev grab policy.
