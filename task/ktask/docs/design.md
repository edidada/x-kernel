# ktask design for XHyper

`ktask` owns kernel execution contexts and RR scheduling for the XHyper EL2
runtime. A task is either an internal kernel task or a per-CPU idle task.

Tasks have a kernel stack, architecture context, scheduler state, CPU affinity,
wait registrations, and optional preemption accounting. Scheduler-local trace
and lock-owner identifiers use the stable `TaskInner` address while the task is
alive.

There are no user tasks, PID/TID handles, process runtimes, user page-table
callbacks, or syscall lifecycle hooks. VM and vCPU objects belong to a future
hypervisor business layer and may use these kernel tasks as workers without
changing the scheduler's identity model.
