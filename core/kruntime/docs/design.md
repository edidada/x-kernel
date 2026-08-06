# kruntime design for XHyper

`kruntime` coordinates AArch64 EL2/VHE bring-up. It initializes memory,
interrupts, timers, drivers, SMP state, and the RR scheduler, then invokes
`HypervisorRuntimeEntry` after every discovered CPU reaches the runtime-ready
barrier.

The primary CPU activates one kernel bootstrap task pinned to its run queue.
That task starts secondary CPUs, completes late initialization, and hands off to
the hypervisor runtime policy layer. The original boot task remains parked.
Secondary CPUs register their run queues and enter their idle tasks.

There is no process-init phase, PID 1, user executable, or userspace runtime
handoff.
