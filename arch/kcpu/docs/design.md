# kcpu design for XHyper

The XHyper configuration uses `kcpu` only for privileged AArch64 CPU state,
trap-frame handling, exception vectors, and kernel task context switching at
EL2/VHE.

The AArch64 path deliberately has no EL0 entry API, syscall restart state, or
Linux exception-table fixups. `init_trap` installs the vector table and clears
the low-address translation root. Synchronous faults not claimed by a kernel
trap handler are fatal.

Architecture-specific userspace modules retained for non-AArch64 source targets
are outside the XHyper build graph and must not be introduced into the AArch64
runtime dependency graph.
