# subsys-char-console — Security and reliability

## Trust model and external boundaries

The crate consumes `Publication<CharDeviceImpl>` values produced by char
drivers through the class registry. It performs no device I/O, touches no
MMIO or user pointers, and contains no `unsafe`. Its only external inputs
are class availability/removal callbacks and the driver's transferred
guards.

## Memory and thread safety

All state is behind a single `SpinNoPreempt`. Class callbacks and probe-time
registration may run from different contexts, so every callback re-checks
membership under the lock (`add` is idempotent by publication id). Ownership
rules:

- the adopted stdout publication lives in the service, never dropped under
  the lock (its drop re-enters the class teardown callbacks);
- `DevIrq` / `IoMap` guards are stored for their `Drop`; replacement drops
  the previous pair outside any class lock path;
- a stale handle can only be observed by consumers that already cloned it,
  and `stdout_console` only returns handles that are still available.

## Threats and failure handling

| Failure | Effect | Control |
| --- | --- | --- |
| Two UARTs claim the console | Ambiguous `/dev/console` | Only the publication handed to `register_stdout_console` is adopted; the char-availability bridge filters by the adopted id or driver name `console`, so auxiliary ports cannot replace the association |
| Rebind while a consumer holds the old handle | Stale console | Consumers check `is_available`; `stdout_console` gates on availability, and removal clears the association |
| Publication outliving its IRQ/mapping | Console reads that can never receive input | Guards are transferred into the same binding as the publication (`adopt_stdout_guards`), so both are replaced/dropped together |
| Removal callbacks re-entering | Deadlock | Withdrawal runs outside the console lock; `remove` never drops the publication under the lock |

## Known limitations

- No hot-unplug semantics beyond availability gating; the native console is
  permanent.
- No active/default console selection API.
- The console view trusts publication driver names for non-stdout
  membership (`console`); a driver claiming that name becomes console
  visible.

## Audit checklist

- Every path that replaces the stdout publication also replaces the guard
  binding.
- No `Publication` drop happens while `ConsoleSubsystem` is locked.
- Class callbacks stay idempotent by publication id.