# XHyper Entry

The kernel image is an AArch64 EL2/VHE runtime substrate. Its entry crate only
links the allocator, memory-management, interrupt, timer, SMP, console, and RR
scheduler foundations required by `kruntime`.

After primary and secondary CPU initialization, `SystemInitEntry` prints the
build metadata and parks the bootstrap task. The scheduler then runs the idle
tasks. POSIX processes, syscalls, user executable loading, filesystems,
networking, TEE services, and the `kvmm` VM/vCPU business layer are not linked
into this image.

EL2 host mode is unconditional. There is no `vmm` feature switch: firmware must
enter the image at EL2 and QEMU is launched with virtualization enabled.
