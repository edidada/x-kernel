# Real guest S3 validation

Run from the repository on a host with `qemu-system-x86_64`, the project Rust
and x86 LinuxBoot build tools, Python 3 and e2fsprogs:

```sh
S3_TEST_CPUS=4 S3_TEST_ROUNDS=12 scripts/pm/run_qemu_s3.sh
S3_TEST_CPUS=1 S3_TEST_ROUNDS=3 scripts/pm/run_qemu_s3.sh
S3_TEST_UNSUPPORTED=1 S3_TEST_ROUNDS=3 scripts/pm/run_qemu_s3.sh
S3_TEST_UNSUPPORTED=1 S3_TEST_VGA=1 S3_TEST_ROUNDS=3 scripts/pm/run_qemu_s3.sh
```

`S3_OUTPUT_DIR` selects an evidence directory (otherwise a fresh `/tmp` path).
The script replaces `.config` with the supplied x86 q35 S3 seed, builds the
normal production kernel and a standalone static `/bin/sh` probe, and creates
a disposable 256 MiB ext4 disk. Do not put existing data in that directory.
The configured hardware is four-capable CPUs, ISA serial and VirtIO block;
network, GPU, vsock and RNG devices do not yet advertise PM support.
The runner explicitly uses `-vga none`; even unbound PCI display devices
are rejected by PM preflight because firmware option-ROM recovery is unsupported.

The userspace probe invokes `reboot(SW_SUSPEND)`. QMP observes real `SUSPEND`,
waits three host seconds, then requests `system_wakeup`. QMP `stop`/`cont` is
never used as a substitute for platform S3. Each round verifies syscall return,
MONOTONIC exclusion, BOOTTIME/REALTIME compensation, both clock domains' timerfd
semantics, nanosleep delivery, raw block reads, file write/fsync, and forward
progress/no backward clocks on three processes pinned to distinct remote CPUs.
The one-CPU variant exercises the same entry without remote participants.

The first negative variant disables S3 in QEMU firmware. The VGA variant
leaves firmware S3 enabled and attaches the default VGA, testing display-profile
rejection before platform entry. Repeated requests must return
EOPNOTSUPP while tasks and block reads continue, with no QMP sleep/wake events.

After QEMU exits, host debugfs independently checks all 4096 persisted bytes of
`pm-proof` against the last cycle's pattern. The JSON result requires this check,
the userspace success marker, every requested cycle, matching QMP event counts,
a successful launcher and no guest reset. A kernel
boot, unsupported request, raw context test, unit fixture or QEMU exit code by
itself cannot pass this scenario. Serial/QMP logs and failure CPU registers are
retained. The runner has a 600-second bound. This platform/firmware scenario is
standalone because it must control host wakeup as well as guest syscalls; the
ordinary syscall harness cannot manufacture a firmware resume by itself.

The guest logs clock deltas before asserting them. The host independently
compares BOOTTIME and REALTIME deltas with its observed request-to-return wall
interval, allowing two seconds for the one-second RTC quantum, serial polling
and sampling skew. Firmware/AP reconstruction can extend the three-second QMP
sleep, so a fixed five-second compensation ceiling is not used. Every positive
round must pass the host comparison, retained in `guest-e2e-clock-checks.json`.

The runner announces the evidence path before building and again on exit.
QMP startup failures, malformed responses and guest timeouts retain a failed
result JSON with an error reason, clock checks, serial and QMP logs.
`S3_TEST_TIMEOUT_SECONDS` can shorten the default 600-second deadline for
runner failure-path verification; it never converts a timeout into a pass.

Run `python3 scripts/pm/test_qemu_s3.py` for controlled host-only failure
regressions (early launcher exit, QMP hang with group cleanup, and evidence
announcement on shell setup failure). These do not replace the real S3 tests.
