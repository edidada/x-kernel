# Guest system suspend/resume validation

## Architecture matrix and truthful support status

The guest probe and host runner support `x86_64`, `aarch64`, and `riscv64`
Linux syscall ABIs. This is test portability, not three implemented platform
sleep backends. The production `kpm::suspend_guest` entry currently implements
real system suspend only on x86 q35/SeaBIOS; the other targets return
`EOPNOTSUPP` before freezing. A positive ARM64 or RISC-V scenario therefore
fails today. Do not advertise a green three-architecture resume matrix until
those platform implementations and their firmware have actually run it.

```sh
# Strict matrix: all three architectures, one/four CPUs, three rounds each.
# Includes positive resume and negative refusal; x86 also tests VGA refusal.
# Missing QEMU, build failure or unavailable backend fails, rather than skips.
python3 scripts/pm/run_qemu_suspend_matrix.py --output /tmp/pm-matrix

# Select an individual architecture and expected result explicitly.
S3_TEST_ARCH=aarch64 S3_TEST_UNSUPPORTED=1 S3_TEST_ROUNDS=3 scripts/pm/run_qemu_s3.sh
S3_TEST_ARCH=riscv64 S3_TEST_UNSUPPORTED=1 S3_TEST_ROUNDS=3 scripts/pm/run_qemu_s3.sh
python3 scripts/pm/run_qemu_suspend_matrix.py --arches x86_64 --scenario resume --rounds 12
```

The matrix runs sequentially because each build replaces `.config`. Use an
isolated worktree. Output directories must be empty; each case has independent
logs, disk, provenance and verdicts. `matrix-result.json` and
`matrix-junit.xml` are updated after every case. A partial matrix always has
`complete=false` and `passed=false`. A refusal-only matrix can pass its negative
assertions but always has `all_architectures_resumed=false`.

Firmware/CPU overrides can be supplied explicitly as `S3_TEST_QEMU_ARGS` for
an individual run, or `PM_QEMU_ARGS_AARCH64` / `PM_QEMU_ARGS_RISCV64` for the
matrix (for example `-bios /path/to/fw_jump.elf`). The final QEMU command is
recorded. The runner does not patch firmware, invent a sleep backend or use
QMP stop/cont as a replacement. The installed emulator must implement the
selected CPU model; XKMake normally selects Cortex-A76 on ARM64 TCG. For an
older emulator, an explicit `-cpu cortex-a72` override changes the tested CPU
profile and must be retained in the report.

Every positive cycle must show the production task/queue freeze, device
suspend, exact remote CPU acknowledgment count, platform entry, successful
sleep accounting, and complete CPU/device/task recovery, in order between
the userspace request and return. The seed enables INFO logs for this audit.
Exact request/pass cycle IDs, alternating QMP SUSPEND/WAKEUP, the matching guest
architecture, independent host clock checks, no reset/failure/QMP error, and
the final persisted file are all required. Duplicate, missing or misordered
events cannot pass merely because their total counts match.

Negative cycles require each requested `EOPNOTSUPP`, continued remote process
progress, timerfd delivery, nanosleep, unchanged raw block contents, and new
file write/fsync plus a host-side persistence check. They must have zero
platform sleep/wake events and no production freeze/entry stages. These tests
validate safe rejection; they do not exercise device restoration or a firmware
resume on unsupported platforms.

Run all host tool regressions with:

```sh
python3 -m unittest discover -s scripts/pm -p 'test_*.py'
```

The protocol/matrix fixtures reject partial chains, duplicate cycle markers,
wrong CPU counts, reordered stages, stop/cont substitutes, unpaired sleep
events, resets, QMP errors, missing clock observations, and refusals presented
as positive resumes. Controlled host fixtures are never evidence of real
platform suspend.

## Real x86 ACPI S3

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
