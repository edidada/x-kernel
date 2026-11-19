# Final-user address-space teardown benchmark

The guest probe in `../benches/guest/exit_teardown.c` measures the parent-visible
latency from releasing an already prepared child to `waitpid` returning. Child
creation, mapping creation and initial faults finish before the start timestamp.
Pipe wakeup, scheduling, file-descriptor teardown and reaping remain in the
interval, so this is not a timer of `MmSpace::clear_for_exit` alone.

Each workload runs four warmups followed by 24 samples. The probe reports the
upper median, approximate 90th percentile (the 22nd sorted sample), minimum and
maximum, in microseconds:

- One private anonymous 1 GiB `MAP_NORESERVE` mapping with 64 resident pages,
  evenly spaced 16 MiB apart.
- 256 separate private anonymous 1 MiB `MAP_NORESERVE` VMAs, with one resident
  page in each.

Build a static aarch64 guest binary on the host:

```sh
aarch64-linux-gnu-gcc -O2 -Wall -Wextra -Werror -static \
  -o exit-teardown-bench mm/memspace/benches/guest/exit_teardown.c
```

Inject it into an independent rootfs copy, then run `/exit-teardown-bench` from
the guest shell. Compare baseline and patched kernels using the same rootfs,
platform defconfig, guest memory, CPU count, emulator and acceleration mode.
Run comparisons without concurrent kernel builds. These measurements do not
replace correctness tests for shared ownership, fork, file mappings or failed
TLB invalidation.

## 2026-11-19 aarch64 QEMU comparison

Baseline: upstream `main` at `e2f2bb47548ae53fc5ae9670dcfb8d247a3711c1`.
Patched: the local `fix/mm-exit-teardown` implementation. Both use
`platforms/kplat-aarch64/qemu_defconfig`, normal release builds, QEMU TCG
(`cortex-a76`, `virt,gic-version=3`), 4 vCPUs, 1 GiB RAM, and the same independent
musl rootfs image. No kernel compilation ran during these measurements. Two
baseline/patched pairs booted fresh guests, each with 4 warmups and 24 samples.
Values below are microseconds.

| Workload | Round | Baseline median | Patched median | Baseline p90 | Patched p90 |
| --- | --- | ---: | ---: | ---: | ---: |
| Sparse 1 GiB / 64 resident pages | 1 | 7012 | 6734 | 7208 | 6941 |
| Sparse 1 GiB / 64 resident pages | 2 | 6917 | 6807 | 7297 | 7187 |
| 256 VMAs / one resident page each | 1 | 19516 | 15110 | 20150 | 15361 |
| 256 VMAs / one resident page each | 2 | 19372 | 15356 | 19947 | 15870 |

The multi-VMA median decreased by 21–23% in these two pairs. The sparse single
VMA changed by about 2–4%, within a narrow absolute interval: upstream already
skips absent page-table subtrees. The patch's main observed benefit here is
consolidating traversal and invalidation across VMA boundaries. These are TCG
observations, not bare-metal predictions or a proof of long-run leak freedom.
