# kruntime security for XHyper

## Trust boundary

Boot information and platform descriptions are validated before they are used
to configure memory and CPU topology. The runtime executes at EL2/VHE and does
not accept syscall or userspace pointer inputs.

## Invariants

- The scheduler and interrupt subsystem are initialized before kernel worker
  tasks become runnable.
- `INITED_CPUS` uses release/acquire ordering as the global runtime-ready
  barrier.
- Only the primary bootstrap task invokes `HypervisorRuntimeEntry`.
- Secondary CPUs enter scheduler idle after local initialization.
- The boot task remains parked after publishing the bootstrap task.
- No process, PID namespace, credential, signal, or user address-space state is
  created during runtime initialization.
