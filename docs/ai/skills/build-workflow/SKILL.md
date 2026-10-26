# Build Workflow

Use this skill to configure, build, and run X-Kernel through the repository
Makefile and XKMake.

## Architecture

```text
Makefile -> xkmake -> xconfig + Cargo + image tools + QEMU
```

- `xconfig` owns Kconfig evaluation and generated configuration.
- `xkmake` owns build, bundle, rustdoc, and QEMU orchestration.
- Cargo owns Rust compilation and incremental builds.

During an XKMake build, xconfig parses and evaluates Kconfig once. All
generated configuration files consume the resulting resolved snapshot; build
stages must not call the standalone `gen-const` or `gen-cargo` CLI paths.
XKMake also renders the target linker script from the repository-root
`linker.lds.S` and that snapshot before Cargo runs. It must preserve the
existing file and mtime when the rendered content is unchanged so consecutive
builds remain incremental.

Do not use a bare `cargo check -p <crate>` as the main kernel validation path.

## Prerequisites

- the Rust toolchain and targets from `rust-toolchain.toml`;
- `cargo-binutils` and `rust-objcopy`;
- the target architecture's musl cross toolchain (`<arch>-linux-musl-gcc`) for
  user apps, TEE tests and harness guest cases; the kernel build itself does
  not compile C;
- the matching QEMU system emulator for `make run`;
- `cargo-shear 1.13.2` and `licensure 0.8.1` for repository hygiene checks;
- for `make dylint`: `cargo-dylint` and `dylint-link` at `DYLINT_VERSION` in
  `xtask/xkmake/src/hygiene.rs`, and the lint toolchain with the components
  listed in `tools/lints/rust-toolchain.toml` plus the kernel target of each
  platform you scan (`aarch64-unknown-none-softfloat`, `x86_64-unknown-none`);
  see `tools/lints/README.md`;
- an ext4 `disk.img` when the configured virtio block device is enabled.
- for unit-test coverage: `debugfs`, `rust-profdata`, and `rust-cov`;
- for x86_64 boot media: a GNU toolchain for the i386 LinuxBoot setup code
  (the native `gcc`, `ld` and `objcopy` on x86_64 hosts, `x86_64-linux-gnu-*`
  on other hosts), `mkfs.fat`, mtools (`mmd` and `mcopy`), and OVMF/EDK2
  firmware for UEFI execution.

The CI builder image provides all of the above except the project toolchain,
which CI installs at setup. Its Dockerfiles and the local build commands are
in `scripts/ci/docker/`; the image version CI uses is the one referenced in
`Jenkinsfile`. Run commands in that image to reproduce a CI-only failure.

Install the pinned repository hygiene tools with:

```bash
make install-tools
```

This delegates to XKMake so the installer and version checks share the same
pinned version constants. Cargo installs these executables under
`~/.cargo/bin` by default; that directory must be present in `PATH` for
`make clippy` and the standalone hygiene targets.

`make dylint` scans the kernel selected by the current `.config` with the
repository Dylint lints (see `docs/code-guidelines/`). Lints only see code
that the configuration compiles; CI scans the aarch64 and x86_64
`qemu_defconfig` configurations. `make dylint_test`
runs the lint's UI tests. The first run builds the Dylint driver and can take
a few minutes; later runs are incremental. Install the pinned tools with
`make install-tools` after adding the nightly components and target above.

## Configuration

Start from an explicit platform defconfig:

```bash
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
```

Supported configuration commands:

```bash
make defconfig
make oldconfig
make olddefconfig
make menuconfig
make saveconfig
make savedefconfig
```

`make build` also expands an existing seed `.config` automatically. It does
not silently select a platform when `.config` is missing.

## Build

```bash
make build
```

The canonical output is a bundle:

```text
target/xkmake/<platform>/<profile>/
├── bundle.toml
├── kernel.elf
└── kernel.bin
```

x86_64 bundles additionally contain both supported boot media:

```text
kernel.bzimg
kernel.uefi.img
```

Successful builds also refresh compatibility copies in the repository root:

```text
xkernel_<platform>.elf
xkernel_<platform>.bin
xkernel_<platform>.bzimg      # x86_64 only
xkernel_<platform>.uefi.img   # x86_64 only
```

The bundle remains the canonical output; Jenkins and existing repository tools
use the root-level names for artifact handoff compatibility.

Each bundle ELF contains an allocated `.note.xkernel.build-info` note and a
SHA-256 `.note.gnu.build-id`. Inspect them with:

```bash
readelf -n target/xkmake/<platform>/<profile>/kernel.elf
```

XKMake always invokes Cargo, allowing Cargo to validate source freshness. It
reuses the image-processing result when the resolved configuration, build
inputs, Cargo ELF, and bundle manifest still match.

Normal `build` reuse is a trusted local-cache fast path. It validates the
atomically published manifest, artifact names, sizes, mtimes, and build inputs
without hashing the complete ELF. `run --no-build` is an artifact-consumption
boundary and recomputes the loadable-image SHA-256 before accepting the bundle.

The embedded `build_time` is the UTC time at which XKMake actually regenerates
the bundle. Automatic wall-clock time does not participate in cache matching:
an unchanged build reuses the existing ELF, Build ID, and build time. Explicit
`KBUILD_BUILD_TIME` and `SOURCE_DATE_EPOCH` values are build inputs and can
force regeneration when they differ from the bundled value.

Direct tool invocation is available for diagnostics:

```bash
cargo xkmake build -v
cargo xkmake build --dry-run -v
```

## Documentation

Generate workspace rustdoc with the resolved Kconfig feature set:

```bash
make doc
make doc XKMAKE_ARGS='--open'
```

Enforce rustdoc coverage on public APIs in addition to broken-link checks:

```bash
make doc_check_missing
```

XKMake consumes the same one-pass resolved configuration snapshot and Rust
target used by the build flow. Documentation stages must not recover features
by parsing `.cargo/.xconfig.toml` or document kernel crates for the host target.

## Run

```bash
make run
```

XKMake supports QEMU boot for:

- `kplat-aarch64`;
- `kplat-riscv64`;
- `kplat-loongarch64`;
- `kplat-x86_64`.

x86_64 defaults to LinuxBoot. Select the UEFI image with either form:

```bash
make run UEFI=y
make run XKMAKE_ARGS='--boot uefi'
```

XKMake locates common OVMF/EDK2 installations automatically. Use
`--ovmf-code` and `--ovmf-vars-template`, or the corresponding
`OVMF_CODE`/`OVMF_VARS_TEMPLATE` Make variables, for custom firmware paths.

Pass options through `XKMAKE_ARGS`:

```bash
make run XKMAKE_ARGS='--memory 2g --smp 2'
make run XKMAKE_ARGS='--no-net --no-block'
make run XKMAKE_ARGS='--disk-image images/rootfs.img'
make run XKMAKE_ARGS='--no-vsock'
make run ACCEL=n
make run QEMU_ARGS='-d guest_errors'
make run QEMU_ARGS='-no-reboot'
```

`QEMU_ARGS` forwards arbitrary extra arguments straight to the QEMU invocation
(they are appended after `--`), covering ad-hoc devices or debug flags that
don't have a dedicated Make variable.

## Symbolize

Raw backtrace addresses in a panic/exception log are resolved against the
unstripped `kernel.debug.elf` (always produced by the build) on the host:

```bash
make run > panic.log          # capture a failing run
make symbolize LOG=panic.log  # resolve addresses to function/file/line
```

The tool accepts the stable kernel frame format (`0: 0xffff8000...`) and
`func+0xoff/0xsize` annotated lines, and uses `llvm-addr2line` (falling back
to `addr2line`). Use `XKMAKE_ARGS='--elf <path>'`, `--offset <delta>`, or
`--tool <binary>` for custom setups; without `LOG=` it reads stdin.

`make run` / `make justrun` also symbolicate automatically: QEMU output is
mirrored to `bundle/qemu.log` (alongside the live terminal output), and when
the run ends with backtrace frames the addresses are resolved right away.
Disable with `XKMAKE_ARGS='--no-symbolize'`.

### Changing the log level without rebuilding

The `log` crate here has no compile-time level cap (`log` is a plain `0.4`
dependency, no `max_level_*` feature), so every level is compiled in and only
filtered at runtime. Three layers, most specific first, all using the same
vocabulary (`0`/`off`, `1`/`error`, `2`/`warn`, `3`/`info`, `4`/`debug`,
`5`/`trace`):

| # | Layer | Scope | Needs |
| --- | --- | --- | --- |
| 1 | `echo 'warn drv_mmc=trace' > /proc/sys/kernel/printk` | live, until reboot | nothing — the node already exists |
| 2 | `/chosen/x-kernel,log-level = "debug";` | boot of that board | editing the DTB |
| 3 | `loglevel=5`, `quiet`, `debug` on the cmdline (`/chosen/bootargs`) | boot of that run | the boot loader |
| 4 | `LOG_LEVEL_*` Kconfig | build default | a rebuild |

Layer 1 is the one to reach for while debugging on hardware: it changes
verbosity mid-session (`cat /proc/sys/kernel/printk` reports the number) and
leaves the flashed image alone. It cannot help with **boot-time** output
(driver probes run before userland and there is no log ring buffer: what the
console filter drops is gone), which is what layers 2–4 are for. The RK3588
defconfig therefore boots at `LOG_LEVEL_DEBUG` during bring-up.

Values are validated; an invalid one is reported and ignored rather than
silently applied. Note the value order is X-Kernel's (higher = more verbose),
which is *not* Linux `console_loglevel` compatible.

A layer-1 write takes a full spec, so logging can be *retargeted* mid-session
instead of raised globally — the boot default on RK3588 is
`warn drv_mmc=debug subsys_mmc=debug fs_boot=debug`, which keeps IPI and
syscall chatter off the console while the storage stack talks.

### `dmesg`: the console filter does not lose records

The kernel keeps a 64 KiB log ring buffer. Every record that gets past the
`log` ceiling is stored there **before** the console filter runs, so a quiet
console and a complete log coexist — the filter decides what is *shown*, the
buffer records what *happened*. Read it with the usual tool:

```bash
dmesg                 # everything captured, oldest first
dmesg | tail -40
dmesg | grep -i 'dwmmc\|mmcblk'
dmesg -r | grep -E '^<(3|4)>' # level filter: 1=error 2=warn 3=info 4=debug 5=trace
dmesg -c              # read and clear
```

Lines carry X-Kernel's level number as a `^<N>` prefix. It is the same value
domain as `/proc/sys/kernel/printk` (1 = error … 5 = trace), *not* Linux's
inverted console levels; note that tools which parse `<N>` (busybox `dmesg`
included) strip the prefix by default, hence `-r` above when filtering on it. The ceiling of the active spec is also the
capture level: with `warn drv_mmc=debug` the buffer holds debug from every
module but stops before `trace`; add a `target=trace` rule (at boot, or live
through `/proc/sys/kernel/printk`) to capture trace as well.

### Symbolizing logs from real hardware

Board logs work the same way. The kernel runs at its link-time virtual
address (`kimage_vaddr` is fixed per architecture; only the physical load
address varies), so raw backtrace addresses captured from a serial console
match the build's `kernel.debug.elf` directly:

```bash
minicom -C board.log    # capture U-Boot + kernel serial output
# resolve with the matching build artifact (use --elf when it is not the
# current bundle's kernel.debug.elf):
make symbolize LOG=board.log XKMAKE_ARGS='--elf xkernel_aarch64-rk3588.debug.elf'
```

Keep the build's `xkernel_<arch>-<machine>.debug.elf` (published next to
the boot image) together with the flashed image. `--offset` is only needed
if the kernel gains relocation/KASLR, or when mixing logs with a different
build; a fully-unresolved pass prints a diagnostic pointing at both causes.

Address detection is two-tier: strict kernel frame lines
(`<index>: 0x<addr>`) first, then — only when the log carries panic markers
(`Backtrace:` / `panicked` / `kernel panic`) — a loose scan for high-half
kernel addresses anywhere in a line. The loose tier tolerates serial tools
that inject timestamp/ANSI prefixes or logs captured with truncated
headers; user-space addresses (`0x7f...`) are excluded.

Important defaults:

- memory: `1g`;
- SMP: configured `NR_CPUS`;
- disk image: `disk.img`;
- virtio block and network: enabled when compiled into the kernel;
- vsock: automatically attached when the kernel driver and selected QEMU
  device model both support it; otherwise XKMake warns and continues;
- acceleration: `KFEAT_VMM=y` forces QEMU TCG even when `ACCEL=y`, because
  CI hosts usually expose KVM only to the outer QEMU and do not support nested
  virtualization;
- display: serial/nographic unless `--graphic` is supplied;
- guest IP: `10.0.2.15`;
- gateway: `10.0.2.2`.

Guest IP and gateway are currently build inputs because the network stack uses
compile-time environment values:

```bash
make build XKMAKE_ARGS='--guest-ip 10.0.2.20 --gateway 10.0.2.2'
```

## Repository Utilities

The thin Makefile retains repository-level utility targets that do not own
kernel configuration or QEMU policy:

```bash
make rootfs
make uapps
make rootfs-uapps
make teefs
make disk_img
make ramdisk_img
make install-tools
make check_deps
make check_header
make check_lints
make check_boundaries
make doc
make doc_check_missing
make fmt
make clean
make distclean
```

`make check_deps` runs the pinned `cargo-shear 1.13.2` analyzer through
`xkmake hygiene deps`. Use `make deps` to apply its unused-dependency fixes.
The check covers the root, `xtask`, `tee_apps`, and `uapps/hello` Cargo
workspaces. Cargo-shear warnings remain advisory; dependency errors fail the
command.

`make check_header` runs the pinned `licensure 0.8.1` analyzer through
`xkmake hygiene header`. Use `make header` to add or update the configured
three-line header. XKMake passes tracked and non-ignored untracked Rust source
files to licensure; deleted worktree paths and non-Rust files are not checked.

`make check_test_layout` is a manual source-layout check. An `include!` call
alone is not evidence of test code: production modules also include generated
configuration and bindings. Recognized test-module markers are still checked
when their module body uses `include!`. The checker does not expand macros or
resolve dynamically generated include contents; passing it does not prove
that such contents contain no misplaced tests. Existing textual marker checks
are not a complete Rust syntax analysis.

`make check_lints` runs `xkmake hygiene lints`. Every root workspace member,
including implicit path-dependency members, must declare
`[lints] workspace = true`; there is no opt-out list. `make clippy`,
`make dylint`, and `make dylint_test` run it before Cargo.

`make check_boundaries` runs `scripts/ci/check_driver_boundaries.sh`, the
mechanical half of `docs/ai/driver-subsystem-boundaries.md`: rule L0-a
(`driver-base` names no device family), L0-b (contract crates carry no direct
host-kernel dependency), L1 (no `drv/**`/`subsystems/**`/`devnode/**` crate
fetches a provider singleton or imports `driver_base::devres::host_api`, and no `drv/**`
crate calls a `kirq` registration API), and the integrity of the E-01..E-04
exception register — an allow-list entry whose id is missing from that document,
or whose code no longer needs the exception, fails the check. It is pure grep
over the tree (no build), and `make clippy` and `make dylint` run it before
Cargo, so it also gates the pre-commit hook. A new host dependency under
`drv/**` must be added to that document's register *and* to the script's
allow-list; the script fails until both exist.

`make unittest` means a kernel unit-test build followed by QEMU execution. It
is equivalent to the former `make run UNITTEST=y` workflow:

```bash
make unittest
make unittest UNITTEST_CRATE=kvfs,kprocess
```

After a successful unit-test QEMU run, XKMake extracts
`/.llvm-cov/default.profraw` from the configured disk image and writes these
artifacts under `target/<rust-target>/<profile>/`:

```text
default.profraw
default.profdata
coverage.txt
coverage.info
coverage.xml
```

Extraction or report-generation failures fail the run. Old coverage artifacts
are removed before processing so they cannot be mistaken for current output.

`make justrun` runs an existing bundle without invoking Cargo.
`make debug` builds first, starts QEMU with the GDB stub paused, and attaches
the configured `GDB` command. `make disasm` consumes the bundle ELF.

`make ci-test` builds the kernel, clones or updates the Starry test harness
into the ignored `ci-test/` directory, and runs its complete `ci-test` suite
against the current X-Kernel tree. Override `CI_TEST_REPO`, `CI_TEST_BRANCH`,
`CI_TEST_ARCH`, `CI_TEST_CASES`, or `CI_TEST_JOBS` when narrowing or
relocating the run.

```bash
make ci-test
CI_TEST_ARCH=riscv64 CI_TEST_CASES=timerfd-semantics make ci-test
```

## Validation

For build-system changes, run:

```bash
cargo test --manifest-path xtask/Cargo.toml -p xconfig -p xkmake
cargo clippy --manifest-path xtask/Cargo.toml -p xkmake --no-deps -- -D warnings
make build
make run
```

Use a bounded QEMU run in automation and verify kernel boot output before
terminating it.

## Formatting

Use the repository-pinned formatter:

```bash
cargo +nightly-2026-03-08 fmt --all
```

When formatting only host tools during development, avoid unrelated formatting
churn in untouched xtask crates.
