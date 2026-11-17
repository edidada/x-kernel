# Guest-initiated QEMU S3 validation — 2026-11-11

## Scope and reproducibility

The historical local implementation extended baseline
`2b6212936cb4cd3b313d494dd615837d13b74d8a` on
`feat/guest-suspend-resume`. The results in the original sections below refer
to that pre-submission tree. The code is now split into a dependency stack rooted
at !1342; fresh submission results are recorded separately below.
The tested backend is x86_64 QEMU q35 / SeaBIOS with ISA serial and VirtIO block.
The host is aarch64 Linux 5.4.18-147-generic, QEMU is 4.2.1
(Debian/Kylin 1:4.2-3kylin6.24k1.11), and the project compiler is Rust 1.95.0.
Execution uses TCG, configured NR_CPUS=4, and no QEMU instruction tracing.
The runner explicitly disables QEMU's default NIC; unsupported driver kinds
are not accepted as successful no-op PM participants.

```sh
S3_TEST_CPUS=4 S3_TEST_ROUNDS=12 scripts/pm/run_qemu_s3.sh
S3_TEST_CPUS=1 S3_TEST_ROUNDS=3 scripts/pm/run_qemu_s3.sh
S3_TEST_UNSUPPORTED=1 S3_TEST_ROUNDS=3 scripts/pm/run_qemu_s3.sh
```

See [the runner instructions](../../../scripts/pm/README.md). The script prepares
an explicit defconfig, normal kernel and disposable root disk. It controls
QEMU through QMP and requires matching sleep/wake events and guest assertions.
It never substitutes QMP stop/cont for platform sleep. The fixed 600-second
bound includes guest boot/run; build is logged separately.

## Executed production chain

```text
userspace reboot(SW_SUSPEND)
 -> syscall permission/command handling
 -> kpm::suspend_guest
 -> boot CPU affinity + explicit driver/AML/platform preflight
 -> registry task freeze + normal queue drain
 -> ordered real device suspend + PCI bus-master exclusion
 -> reset-aware Stop callbacks acknowledge every remote CPU
 -> local event pause + MONOTONIC freeze + RTC boundary
 -> FADT PM1a S3 write
 -> QMP SUSPEND
 -> host wake request -> firmware FACS trampoline
 -> original BSP stack + INIT-all + saved AP context restart/acknowledgment
 -> MONOTONIC rebase + RTC sleep injection + event reconstruction
 -> PCI/VirtIO block/console recovery
 -> remote barrier release/join + original IRQ masks
 -> queue admission + task thaw + original affinity
 -> syscall returns 0 and userspace assertions continue
```

AP wake-page stores are explicitly published before INIT/SIPI, including the
x2APIC ICR ordering case. The shared trampoline is retargeted only after the
previous AP acknowledges high-kernel restoration. Unclaimed APs are first
initialized together, rather than left running firmware while another CPU's
entry is reconstructed. Hardware restoration failure is fatal; no failing
recovery path silently releases producers into unusable hardware.

## Results

The final normal kernel Build ID used by both present-CPU topologies is:

`3307abb641a6e19522697bd7cb85406e1c625ef4e4a340f6dd6573d5985d5eeb`

| Scenario | Observed result |
| --- | --- |
| Four present CPUs, 12 consecutive S3 rounds | 12 SUSPEND, 12 WAKEUP, 12 guest cycle passes, no guest reset |
| One present CPU, 3 consecutive S3 rounds | 3 SUSPEND, 3 WAKEUP, 3 guest cycle passes, no guest reset |
| Firmware S3 disabled, four CPUs, 3 requests | EOPNOTSUPP on all requests; 0 SUSPEND/WAKEUP; tasks/disk usable |
| Relevant kernel fixtures, four CPUs | 400 passed, 0 failed, 1 ignored |
| Same fixture image, one present CPU | 389 passed, 0 failed, 12 ignored |
| VirtIO host tests | 61 passed, 0 failed; 6 documentation tests passed |
| ACPI borrowed-table host tests | 16 passed, 0 failed; 1 documentation example ignored |
| Make/Kconfig Clippy and pinned formatting | Passed |

The single ignored four-CPU fixture is an explicitly gated raw firmware-context
bring-up test. Real platform S3 is instead exercised by the normal production
image above, with actual devices, task freeze, CPU reset and userspace return.
One-CPU additional skips require multiple present CPUs.

Each successful round verifies MONOTONIC excludes sleep, BOOTTIME and REALTIME
include the RTC-estimated interval, a MONOTONIC timerfd remains pending, an
expired BOOTTIME timerfd is rescanned, nanosleep still receives clock events,
raw disk reads retain data, and file write/fsync succeeds. Three child processes
pinned to CPUs 1–3 check forward progress and nondecreasing clocks after every
round. Expected compensation tolerance reflects the CMOS clock's one-second
resolution; it is not subsecond wake-latency measurement.

After QEMU exits, an independent host debugfs read also verifies every byte of
the final 4096-byte `pm-proof` file against the last cycle's pattern on both
root disks. The runner includes this persistence check for subsequent runs.
This distinguishes successful file syscalls from a result held only in guest
memory. JSON result files record this additional check.

The relevant fixtures include actual system freezer/worker preparation,
remote CPU park/join, absent/malformed AML S3 packages, successful-prefix device
rollback followed by retry, and repeated descriptor transfer after transport
reset. They complement rather than replace the real firmware scenario.

## Evidence and failure history

Local evidence is retained under:

`/home/greatwall/桌面/xkernel/validation/guest-suspend-20261111/`

Final real-run subdirectories are `verified-final-four-cpu`, `verified-one-cpu`
and `verified-no-s3`; each contains build/serial logs, QMP events, result JSON
and its root disk. Relevant kernel results are
`s3-final-relevant-unittest.log` and `s3-final-one-cpu-unittest.log`.
Host/static results are `virtio-host-suite-final.log`,
`acpi-host-suite-final.log`, `s3-clippy-final.log` and `s3-fmt-check.log`.

Earlier untraced repeat runs exposed AP execution/reset failure during the
second wake. Tracing changed timing and initially hid the issue; those traced
passes were not accepted as completion. The final protocol first resets all
APs before reusing the shared entry, then restores them sequentially with an
acknowledgment barrier. Multiple clean-boot 12-round untraced runs passed after
this change, including the final run with temporary resume printing disabled.

An expanded run including the entire kdriver crate aborted in the existing
`test_remove_waits_for_inflight_probe` fixture: its bounded yield loop observed
Running rather than Blocked. The exact failing log is
`s3-final-four-cpu-unittest.log`. That test is outside the executed PM path;
the relevant-driver modules and all PM fixtures passed in the scoped run above.
An isolated whole-kdriver follow-up reproduced the same fixture failure in
`s3-kdriver-isolated-unittest.log`.

A separate checkout archived from the unchanged baseline commit reproduced the
same test/line and `Err(Running)` result using the same config and four-CPU TCG
profile. After the baseline build, all 3925 tracked blobs were verified against
that commit, with zero modified files. `baseline-provenance.json` and
`baseline-device-binding-unittest.log` retain the proof. The failure therefore
predates this implementation in this environment. A fully green whole-kdriver
suite is still not claimed, and the baseline fixture's root cause is not fixed
by this PM work.

## Limits of the conclusion

This proves the guest-initiated production S3 chain on the stated QEMU profile,
including real firmware sleep/reset and recovery-side task/time/block behavior.
It does not prove physical Phytium/ARM sleep, host-transparent pvclock repair,
CPU hotplug, arbitrary ACPI firmware `_PTS`/`_WAK`, network/GPU/vsock/RNG PM,
PCI bridge or MSI-X table reconstruction. Unsupported bound driver contracts
refuse before device suspension. There is no sysfs `/sys/power/state` interface
in this implementation; the tested guest entry is reboot(SW_SUSPEND).

## Submission stack validation — 2026-11-16

This section supersedes the historical results above for the submitted stack.
It is rooted at !1342 (`a8408c1d424ff7995f7f08479d8ac6b9fb994d0b`):
!1376 adds CPU rendezvous, !1377 adds reset-aware x86 platform state,
!1378 adds device recovery, and the final integration PR adds the production
syscall chain and executable QMP scenario. !1378 has since been squash-merged into the !1377 branch as
`c303aca583824b89b3ea0bb210ebe0d5c0e63a6d`, whose tree exactly matches
!1378 head `3db778873d34c08634dc8050444566e3e8ad469f`. The integration
PR therefore targets that platform branch. These are dependent patches, not
four independent changes against main.

The final runtime source tested was clean commit
`14dec88f51d6e46652dbf8e901279c046ef5c048`, tree
`02cb4de593ab40e85d9744e84bad7a5d05fd0804`. After testing the integration commit is reparented onto that identical
merged base and this report is added; runtime, config, guest probe and runner
contents are unchanged. The same kernel image supports one or four present CPUs.
Its Build ID is
`3a842728c1f764d92479aef39a63db6fa9fcebb6a081937fb069fd05f343ad3d`,
recorded in every scenario's serial log.

| Final scenario | Required and observed outcome |
| --- | --- |
| Four CPUs, 12 consecutive real S3 rounds | 12 SUSPEND / 12 WAKEUP / 12 guest passes, no reset, persisted disk pattern verified |
| One CPU, 3 consecutive real S3 rounds | 3 SUSPEND / 3 WAKEUP / 3 guest passes, no reset, persisted disk pattern verified |
| Firmware S3 disabled, four CPUs, 3 requests | All return EOPNOTSUPP; tasks and raw block reads continue; zero sleep/wake/reset events |
| Default VGA present with firmware S3 enabled, four CPUs, 3 requests | Discovery-descriptor preflight rejects all requests; tasks/block reads continue; zero sleep/wake/reset events |
| VirtIO host suite including delayed reset acknowledgment | 78 tests and 6 documentation tests pass |
| Normal commit hooks / configured Clippy for recovery changes | Passed |

Final evidence is retained outside the source worktree under
`/home/greatwall/桌面/xkernel/validation/guest-s3-pr-split/`.
The four scenario directories are `pr-final-four`, `pr-final-one`,
`pr-final-no-s3`, and `pr-final-vga-refusal`. Each contains build/serial logs,
QMP events, result JSON and its disposable disk. Host regression results are in
`pr3-delayed-reset-host.log`; normal commit checks are in
`pr3-recovery-fixup.log`. The real S3 runs use TCG without instruction tracing
and wait three host seconds between SUSPEND and system_wakeup.

The earlier stack-stage fixtures separately exercised park/join, CPU save/abort
and device-prefix rollback. Their reports/logs describe the corresponding
stage revisions; they are not being presented as full-suite reruns at the
final source revision. The historical whole-kdriver baseline fixture failure
above remains outside the scope of this implementation.

### Independent host timing and the old fixed-ceiling failure

The earlier `recovery-four` run failed its old fixed five-second BOOTTIME
compensation ceiling on round 11. `recovery-clock-diagnostics` then retained a
6.16-second compensation measurement on round 7 instead of hiding the values
behind a PASS marker. Neither failed scenario is counted as a successful run.

A three-second QMP wait is not the entire suspended interval: firmware/AP
reconstruction can extend it. The final probe logs all clock deltas before
checking them, and the host independently observes every request-to-return
interval. Both BOOTTIME and REALTIME increments must match that host interval
within two seconds (one-second CMOS quantization plus serial polling/sampling
skew), in addition to the guest's minimum sleep and MONOTONIC checks.
There is no unconditional enlarged upper bound. Every requested positive cycle
must have a matching host check in `guest-e2e-clock-checks.json`, or the final
JSON fails even if the guest prints PASS.

The intermediate `clock-host-proof` run observed a 5.13-second host interval and
5.60-second BOOTTIME increment in one round, demonstrating why the old fixed
ceiling was invalid. The clean final four- and one-CPU runs above both pass all
independent host comparisons. This tests second-resolution sleep accounting;
it is not a subsecond accuracy or wake-latency guarantee.

### Display-profile failure and refusal regression

A fresh default-VGA, four-CPU run of the split implementation reset on the
first wake before returning to userspace. SeaBIOS debug output ended during
VGA option-ROM resume before its OS resume-vector jump. Diagnostic instruction
traces are retained, but the underlying firmware/AP failure is not claimed to
be repaired. The supported production runner now explicitly uses `-vga none`.

PM additionally refuses discovered PCI display-class devices before freezing.
The first negative run exposed that scanning only runtime device records misses
unbound devices: these have discovery descriptors but no DeviceObject. That
failed run is retained in `final-vga-refusal`. The final check instead scans
`device_descs_snapshot`; the last negative scenario proves rejection with S3
still enabled in firmware. This relies on complete bus discovery and does not
claim recovery for devices omitted by failed enumeration.

### Recovery review fixes and remaining boundaries

VirtIO recovery now polls for reset acknowledgment instead of treating one
nonzero status read as terminal failure. A host fake holds status nonzero for
three reads and repeats resume three times, checking successful reactivation.
A permanently nonresponsive device can still prevent completion; this polling
is not a bounded recovery guarantee.

ISA serial PM retains the hardware RX-enable bit under its port lock before
masking it. Resume consumes that snapshot after restoring the fixed line setup;
allocated input storage does not imply IRQ setup succeeded. Calls without a
paired suspend or repeated suspend are rejected.

These results establish the guest-initiated production chain only for the
stated headless x86 q35/SeaBIOS serial/VirtIO-block profile. They do not add
physical Phytium/ARM PM, repair its pvclock issue, support CPU hotplug or arbitrary
ACPI methods, or implement network/GPU/vsock/RNG, PCI bridge or MSI-X table
recovery. IRQ route mutation, device discovery/removal and external producers
must remain excluded under the documented system transaction contract. Remote
CPU timeout still joins already queued Stop work before reclaiming its storage.
CI and AI review are tracked separately on each PR; local validation is not a
claim that all remote CI jobs or reviews have passed.

## Standard-distribution multi-architecture probe — 2026-11-17

The test runner now has independent x86_64, AArch64 and RV64 Linux syscall
entry/ELF startup implementations. The normal production kernel is used;
there is no test-only platform suspend provider. The matrix requires both
one- and four-present-CPU runs, exact cycle identities, production PM-stage
ordering, alternating hardware SUSPEND/WAKEUP events, host clock comparisons
and persisted disk contents. QMP errors, kernel panics, resets and incomplete
matrices fail. Negative scenarios separately require repeated EOPNOTSUPP,
remote process progress, timer delivery, raw reads and file/fsync persistence.

The user constrained this validation to standard distribution QEMU/firmware.
On this tree, `kpm::suspend_guest` still has only an x86 platform backend.
ARM64 and RISC-V positive scenarios must therefore remain failed; their
negative successes do not establish system-sleep or recovery support.

Observed development-tree results, retained under
`/home/greatwall/桌面/xkernel/validation/guest-pm-multiarch/`:

| Profile | Actual result |
| --- | --- |
| x86 q35/SeaBIOS, distribution QEMU 4.2.1 | One CPU / 3 real S3 rounds and four CPUs / 12 real S3 rounds pass all production-stage, timing and persistence checks |
| x86 matrix, one/four CPUs | Positive resume, firmware-S3-disabled refusal and VGA refusal exercised separately |
| ARM64, distribution QEMU 4.2.1 Cortex-A72 | One/four CPUs / 3 refusals pass; a real positive request fails with EOPNOTSUPP |
| ARM64, Debian QEMU 7.2.22 Cortex-A76 | One/four CPUs / 3 refusals pass; both positive scenarios fail with EOPNOTSUPP |
| RV64, Debian QEMU 7.2.22 + distribution OpenSBI | One/four CPUs / 3 refusals pass; both positive scenarios fail with EOPNOTSUPP |
| RV64, original distribution QEMU 4.2.1 | Kernel boot fails because its virt DT lacks the Goldfish RTC; this is retained as a failed attempt, not a skipped success |
| Host protocol/matrix/cleanup regressions | 17 pass; controlled fixtures do not count as real hardware sleep |

Debian QEMU and its runtime dependencies were extracted under `/tmp`, without
system installation or source modification. Package SHA256 values were checked
against the distribution package index; the download manifest is retained in
the evidence directory. The RTC remains enabled on the successful RV64 boot.
The old ARM emulator needs an explicit Cortex-A72 override; the newer standard
emulator uses the normal Cortex-A76 profile. Per-case provenance records the
source revision/status, probe input hashes, kernel ELF hash, resolved kernel
configuration, emulator identity and full launch command. Results made before
commit retain dirty-tree provenance rather than claiming a clean-head run.

The required full matrix is deliberately not green: missing ARM64/RISC-V
system-suspend backends remain visible as positive-case failures. Finishing
those capabilities requires platform/firmware entry, wake context, CPU,
interrupt, device and time restoration work beyond portable test code. No
CPU wait, QMP stop/cont, unsupported return or normal guest shutdown is counted
as a successful system suspend/resume.

### Parameter-validation follow-up

The standalone guest now validates CPU topology and parses cycle counts without
panic, including when a caller invokes rustc directly and bypasses the shell's
validation. The actual parser is compiled into a native host regression: two
Rust tests cover accepted bounds and malformed, overflowing, signed, empty and
out-of-range input. The host suite now has 18 Python tests. An intentionally
compiled `S3_TEST_ROUNDS=not-a-number` guest was booted in real x86 QEMU and
printed `E2E_FAIL: S3_TEST_ROUNDS must be decimal 1..100`, without panic and
before any suspend request. Its failed run is retained as `invalid-rounds`;
it is not counted as a positive sleep scenario.
