# X-Kernel Agent Notes

## Shared AI Skills

Tool-neutral shared skills live under `docs/ai/skills/`.
Any AI agent used in this repository should prefer those shared skills
over tool-specific copies when both exist.

Current shared skills:

- `docs/ai/skills/build-workflow/SKILL.md`:
  project configuration, build, run, clippy, and fmt workflow.
- `docs/ai/skills/module-docs/SKILL.md`:
  module documentation generation workflow for `design.md`,
  `security.md`, and required rustdoc coverage.
- `docs/ai/skills/code-guidelines/SKILL.md`:
  coding and code-review conventions for Rust kernel code.
- `docs/ai/skills/linux-driver-port/SKILL.md`:
  harness-backed workflow for porting a Linux C driver (platform/PCI) to
  x-kernel Rust: `ldp` CLI gates (lookup/gaps/scan/check-layout/golden/
  requirements), API mapping data, and gap reporting. Reference trees are
  vendored under `docs/ai/skills/linux-driver-port/data/references/`.
- `docs/ai/skills/linux-driver-port/examples/rk3588-pcie/bring-up-log.md`:
  **PCIe/USB 移植的当前状态与操作手册**。先读顶部第 0–4 节（自包含交接：
  设备机/脚本/串口纪律、已验证事实、CFG 寻址结论、下一步）。
- `docs/ai/skills/test-harness/SKILL.md`:
  external Starry Test Harness workflow for guest regression cases,
  suite registration, execution, and syscall-focused test design constraints.
- `docs/ai/skills/board-automation/SKILL.md`:
  real-board (RK3588) validation loop: `scripts/board-run.sh`, serial/relay
  renumbering after re-cabling, the unittest-config trap, and the
  "dump a working kernel and diff registers" technique for hardware hangs.
- `docs/ai/skills/problem-diagnosis/SKILL.md`:
  first-pass issue classification and localization workflow for build,
  boot, panic, hang, regression, and performance problems.
- `docs/ai/skills/performance-analysis/SKILL.md`:
  performance investigation workflow (workloads, baselines, reporting);
  lock_stat is under `references/lock-stat.md`; scheduler (schbench /
  EEVDF / IRQ / affinity) is under `references/scheduler/`.
- `docs/ai/skills/linux-mm-design-knowledge/SKILL.md`:
  Linux memory-management semantic knowledge base for X-Kernel MM design,
  covering address space, VMA, mmap, faults, anonymous memory, file-backed
  mmap, COW, brk, madvise, msync, and mlock.
- `docs/ai/skills/xkernel-mm-design-workflow/SKILL.md`:
  multi-agent design-first workflow for X-Kernel memory subsystem topics,
  coordinating Linux semantic analysis, X-Kernel adaptation, arbiter review,
  and implementation task splitting without code generation.
- `docs/ai/skills/pr-ci-review/SKILL.md`:
  Gitee PR lifecycle: creating/updating PRs, tracking the latest Jenkins CI
  status and diagnosing failures, and reading the latest review opinions from
  the Gitee PR comment area.

The complete skill index is `docs/ai/skills/README.md`.

The project's own automatic code review rules are located in `docs/ai/review/`.

Before starting the analysis, the review agent must read the rule file corresponding to the current stage from the PR source branch that has been checkouted.

## Project Overview

X-Kernel is a multi-architecture Rust OS/kernel project.

The device lifecycle narrative (enumerate → probe → publish → quiesce →
release) lives in `docs/device-life.md`; read it before changing any of the
four stages.

Major areas:

- `arch/`: architecture HAL and CPU support.
- `boot/`: boot protocol handoff, boot stubs, and loaders.
- `core/`: core runtime, syscall, tracing, and services.
- `mm/`: memory management and address-space abstractions.
- `drivers/`: the driver framework and concrete drivers.
  - `base/`: device model (`driver-base`) incl. the class machinery;
    `host/`: host service bridge (`driver-host`); `init/`: composition
    root with initcall levels (`driver-init`).
  - `devnode/`, `uevent-netlink/`: the device model's userland-node
    projection (`devnode` holds the registry, node identity and the
    udev-visible uevent payload format) and its netlink sink
    (`uevent-netlink` installs itself as the lifecycle publisher and pays
    for `knet`, so the node model stays host-free). The class layer lives in
    `driver-base::class` with family instantiation in `subsystems/*`. The two
    adapter directions — kernel capabilities -> framework (`drivers/host`) and
    framework -> kernel facilities (`uevent-netlink`) — are fixed by role, not
    by directory name.
  - `subsystems/`: per-class APIs and services
    (block/char/char-console/display/hwrng/input/net/rtc/vsock).
  - `drv/`: concrete drivers, one subsystem crate per family, one module
    per driver (`drv/virtio`, `drv/serial`, `drv/clk`, `drv/block`,
    `drv/rtc`). The
    shared virtio family library is `drv/virtio/devices` (package
    `virtio-devices`).
  - `bus/`, `firmware/`: PCI access; DT/ACPI/FDT parsing and the DICE
    handover reader (`firmware/dice`, package `dice-driver`).
  - `boot-console/`, `clocksource/`, `irqchip/`, `perf/`: kernel-integrated
    drivers outside the model, pinned there by boot order or `kirq`
    layering (`boot-console` is the printk sink + earlycon; its runtime
    identity lives in `subsystems/char-console`). `arch/x86-apic`
    holds the CPU interrupt backend. `rtc` is the template for migrating
    one into `subsystems/<class>` + `drv/<family>`.
- `platforms/`: platform glue and defconfigs.
- `process/`, `task/`, `fs/`, `io/`, `net/`, and `tee/`: subsystem crates.
- `util/`: shared utility crates such as `klazy`.

## Current Refactor Context

Recent work removed the old catch-all `kcore` crate and tightened subsystem
ownership boundaries.

- Move reusable utility crates out of `core/` into `util/`.
- Keep address-space logic inside `mm/memspace`.
- Keep kernel user-memory access glue in `core/kuaccess`.
- Keep user program loading and exec image setup in `process/kexec`.
- Define crates around cohesive ownership domains: data, state, invariants,
  lifecycle, and execution-context assumptions that naturally belong together.
  Keep code as a `mod` when it is only one crate's implementation detail; split
  it into a crate when multiple peer crates depend on the same owned boundary.
- Avoid reintroducing broad catch-all crates for responsibilities owned by
  `memspace`, `kuaccess`, or `kexec`.
- Prefer shared driver, HAL, and memory-management layers over platform-local
  forks.
- Treat `kirq` as X-Kernel's common interrupt core. Generic IRQ descriptor,
  mapping, registration, dispatch, context, softirq, workerqueue, and threaded
  IRQ semantics belong in `kirq`, not in `khal` or irqchip drivers.
- Keep irqchip drivers below `kirq`: they implement concrete controller
  behavior such as configure, mask/unmask, claim, complete, priority, and IPI
  delivery through `kirq::IntrManagerIf`.
- The driver framework is single-host: cross-host portability is not a goal.
  What is still required is that `driver-base` names no device family, that
  class/subsystem *contracts* stay host-free so the model runs on mocks, and
  that drivers acquire resources only through `Device` and publish providers
  only through `Device::provide::<C>`. The framework exposes kernel IRQ
  capability to drivers by adapting provider IRQ resources to `kirq` in
  `driver-host` (the `DeviceServices` bridge over `kirq`/`kdma`/`memspace`);
  `kirq` must not depend on driver-subsystem crates. Process-global wiring
  (installed services, resource provider, provider registries) has exactly one
  home: `driver_host::Host`.
- Treat `docs/ai/driver-subsystem-boundaries.md` as a hard constraint for
  driver work. It states the rules (L0-a family-agnostic core, L0-b
  host-free contracts, L1 single resource acquisition path) and the exception
  register (L2). `bus/`, `firmware/`, `drivers/host`,
  `drivers/init`, and `drivers/uevent-netlink` are host-side trees by role
  and are out of scope for L0/L1. `kirq`'s type vocabulary (`IrqTrigger`,
  `IrqController`, `IrqDomainId`) is common and may be used anywhere; its
  registration API may not be called from `drv/**`.
- Keep refactoring commits separate from feature or behavior commits.

## Build And Validation

Always prepare `.config` from a platform defconfig before build, run, or
QEMU-based unit-test commands.

```bash
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
```

**RECURRING TRAP (bit us 3+ times — never repeat):** after a board
deploy (`scripts/deploy-rk3588.sh --build` seeds the rk3588 defconfig),
`.config` stays on the BOARD machine. `make unittest` then boots QEMU
with board console addresses and hangs SILENTLY — no output, no error.
The `unittest` target now has a `config-guard-qemu` prerequisite that
fails fast, but check `.config` first whenever a QEMU run looks stuck:

```bash
grep MACHINE_AARCH64_QEMU .config   # must be =y for any make run/unittest
cp platforms/kplat-aarch64/qemu_defconfig .config && make defconfig  # restore
```

Board builds (deploy) and QEMU tests alternate constantly in the
rk3588 bring-up; switching `.config` is part of both flows, not an
afterthought.

Common commands:

```bash
make build
make clippy
make run
make UNITTEST=y run
make unittest
cargo +nightly-2026-03-08 fmt --all
```

`make clippy` also runs the tree hygiene checks (`check_deps`, `check_header`,
`check_lints`) and `check_boundaries`, the mechanical half of
`docs/ai/driver-subsystem-boundaries.md`. `make check_boundaries` runs that one
on its own; it is pure grep, needs no build, and fails if a `drv/**` crate
gains an unregistered host dependency.

Do not use bare `cargo check -p <crate>` as the main validation path.
The project relies on Kconfig-generated features and `.cargo/.xconfig.toml`,
so prefer the Makefile/Kconfig flow.

The current checked-in toolchain is described by `rust-toolchain.toml`.

## Coding Guidelines

Read `docs/ai/skills/code-guidelines/SKILL.md` before writing or
reviewing Rust kernel code.
Read `docs/code-guidelines/README.md` for the rule registry and unified
exception policy before writing or reviewing lint attributes. Backend levels
and coverage live in `docs/code-guidelines/lints.md`; rule requirements and
judgment criteria live in `docs/code-guidelines/CG-*.md`.

Key local rules:

- Use accurate, descriptive names and encode units where the type does not.
- Keep boolean names assertion-like, such as `is_*`, `has_*`, or `can_*`.
- Comments should explain why, not restate what the code does.
- Every `unsafe` block needs a preceding `SAFETY:` justification.
- Validate at subsystem boundaries and trust validated internal invariants.
- Use `?` for ordinary error propagation.
- Avoid casual atomics and linear scans on hot paths.
- Use workspace dependencies and existing local helper APIs where available.
- **Porting and debugging are source-driven, never memory-driven.** When a
  register field, bit position, descriptor layout, command sequence, or delay is
  in question, read the pinned reference implementation — the vendored tree
  `docs/ai/skills/linux-driver-port/data/references/` is the ONLY reference:
  no other source tree exists in this environment. Reading anything under
  `/workspace/*` outside this repository, or fetching sources from the
  network, is forbidden and counts as a failed run. Transcribe from the
  vendored file — quote the function and line in the commit message. Do not reconstruct a layout from
  recall: this port already lost rounds to a swapped TT field pair, a
  SuperSpeed hub descriptor requested with the USB 2.0 type, and a full-speed
  EP0 packet size copied from the low-speed row. Every one of them would have
  been correct on the first try from the source, and every one of them looked
  plausible until the hardware disagreed.
- When the hardware disagrees with the code, first re-read the reference's
  *sequence*, not just the field values: order (what runs before what, which
  reset happens between which steps, which command is retried and how often)
  explains failures that no field-level fix can.
- **Bit definitions are the easy half; mechanisms are the half that breaks.**
  Transcribing `#define`s is necessary but not sufficient — the reference's
  *structure and ordering* must be diffed function by function, and a structure
  the reference never builds must be treated as wrong until the source says
  otherwise. Concretely, before writing a controller driver path, read and
  quote the function that: builds the descriptor/queue structure, enqueues the
  transfer, decides completion, and tears it down. The EHCI bring-up is the
  cautionary example: every register bit matched the reference (the token field
  layout was copied verbatim and was correct) while seven mechanisms were
  invented — a queue head with the transfer in the reclamation-list head, a
  zero height `MULT` in a high-speed queue head, `0` instead of `LIST_END` for
  an empty transfer pointer, `next | LIST_END` setting the terminate bit on
  every queue element, one shared buffer pointer for all stages, completion
  polled on the overlay that starts inactive, and a 50 ms reset hold that was
  never held. Each looked reasonable; none survived the hardware. The rule:
  for anything structural, `sed` the reference function out first and diff our
  version against it field by field and step by step, and say in the commit
  message which function was read.

## Change Completeness

When making a module change, do not stop at code edits alone.
Agents should treat a change as complete only after checking the
surrounding review, documentation, and validation obligations.

Default workflow:

1. Implement the code change.
2. Review the patch against
   `docs/ai/skills/code-guidelines/SKILL.md`.
3. Check whether the change requires documentation updates:
   - crate-local `docs/design.md`
   - crate-local `docs/security.md`
   - rustdoc on touched public APIs
   - shared skills or top-level docs if workflow or policy changed
4. Update the required documentation before final validation.
5. Run the relevant build, lint, and test commands from
   `docs/ai/skills/build-workflow/SKILL.md`.

Documentation sync is usually required when a change affects:

- public API behavior or contracts;
- module architecture, file layout, or major type roles;
- execution-context assumptions;
- state machines, lifecycle, or cleanup behavior;
- `unsafe` boundaries or invariants;
- external inputs, trust boundaries, threat model, or failure handling;
- project workflow, commands, or agent-facing guidance.
