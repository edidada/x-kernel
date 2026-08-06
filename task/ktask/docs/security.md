# ktask security for XHyper

## Invariants

- Runnable tasks execute only kernel-context entry functions.
- Context mutation is owned by scheduler-controlled switch paths.
- A task address is used as an identifier only while its owning reference is
  alive; it is not an externally stable or reusable ABI identifier.
- Run-queue publication occurs only after task construction is complete.
- CPU affinity and SMP running-state updates remain synchronized by the run
  queue and task-local locks.
- No task callback changes a user page-table root during context switching.

The scheduler does not model processes, credentials, namespaces, signals, file
descriptors, or any userspace trust boundary.
