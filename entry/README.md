# XHyper Entry

The kernel image is an AArch64 EL2/VHE runtime substrate. Its entry crate only
links the allocator, memory-management, interrupt, timer, SMP, console, and RR
scheduler foundations required by `kruntime`.

After primary and secondary CPU initialization, `HypervisorRuntimeEntry` prints
the build metadata and starts a minimal serial shell. The shell supports only
`echo` and `exit`. POSIX processes, syscalls, user executable loading,
filesystems, networking, TEE services, and the `kvmm` VM/vCPU business layer
are not linked into this image.

EL2 host mode is unconditional. There is no `vmm` feature switch: firmware must
enter the image at EL2 and QEMU is launched with virtualization enabled.

With the `unittest` feature, the entry runs registered kernel tests after
printing the same build metadata, reports a machine-readable final status, and
then shuts the platform down.
