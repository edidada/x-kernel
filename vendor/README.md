# Vendored Dependencies

Vendored packages retain their upstream licenses, manifests and formatting.
They are excluded from the kernel workspace, not treated as first-party
workspace members. Kernel consumers still pass the normal build and lint gates.
Do not apply kernel copyright headers or repository-wide refactors to them.

## arm-gic-driver (Locally Maintained Dependency)

- Published version: 0.17.13
- Upstream: https://github.com/rcore-os/tgoskits
- Package path: `drivers/intc/arm-gic-driver`
- Source revision: `8fdfcc9e7b57002d4ec3c48ccceb5acc5f24ed83`
- Archive SHA-256: `52210fc25fa222377dfabf2ffd2f07a751debe75d35c173ac14def04525541a5`

The import preserves the published archive byte-for-byte. The manifest declares
MIT, while its README declares Mulan PSL v2 and links a missing package LICENSE;
the upstream repository root at that revision carries Apache-2.0. These
inconsistent declarations are preserved, not resolved or relicensed here.
This is the maintained kernel dependency, not a temporary prerequisite awaiting
an upstream merge. Keep local patches separate from the recorded import, review
updates against that baseline, and run the regression gate below on changes.
An upstream release may replace a local patch only after equivalent behavior
and tests have been verified; upstream merging is not an acceptance condition.

License attribution remains unresolved independently of that maintenance policy.
The conflicting declarations cannot be settled by choosing a license locally.
Preserve them pending authoritative clarification; do not label distribution
attribution as settled. The source evidence, rechecked on 2026-11-03, is:

- [Package manifest (MIT)](https://github.com/rcore-os/tgoskits/blob/8fdfcc9e7b57002d4ec3c48ccceb5acc5f24ed83/drivers/intc/arm-gic-driver/Cargo.toml).
- [Package README (Mulan PSL v2)](https://github.com/rcore-os/tgoskits/blob/8fdfcc9e7b57002d4ec3c48ccceb5acc5f24ed83/drivers/intc/arm-gic-driver/README.md).
- [Repository-root license (Apache-2.0)](https://github.com/rcore-os/tgoskits/blob/8fdfcc9e7b57002d4ec3c48ccceb5acc5f24ed83/LICENSE).
- The package-local `LICENSE` is absent from the imported archive; its raw
  source URL at the same revision returns HTTP 404. The root license alone does
  not resolve the contradictory package-specific statements.

The upstream working clone is local-only and is not a nested Git submodule in
the kernel repository.

The local patch exposes `v3::Gic::wait_for_distributor_rwp()` and the typed
`DistributorRwpTimeout` error. It reuses the existing 10,000-poll bounded wait
without changing initialization or register-write policy. RWP only tracks
architecturally specified Distributor writes; success is neither a general
configuration barrier nor an active-interrupt/handler drain guarantee.

Run `make check_gic` on a native AArch64 Linux host; the AArch64 Build Artifact
CI leg runs this gate. On an x86_64 host with AArch64 musl Rust std, the cross
linker and QEMU user emulator installed, use:

```sh
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-gnu-gcc \
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER=qemu-aarch64 \
make check_gic GIC_TEST_TARGET=aarch64-unknown-linux-musl
```

The register fixture verifies stuck-RWP failure, recovery after clearing RWP,
and preservation of CTLR. It does not emulate hardware write propagation.

## UART Dependencies

The UART imports preserve the published archives byte-for-byte, separately
from local patches. They remain outside the kernel workspace and retain their
upstream licenses. Rebase local changes onto a new published version and remove
the overrides once upstream supplies the required masked initialization API.

| Package | Upstream revision | Archive SHA-256 | License |
| --- | --- | --- | --- |
| [arm_pl011 0.1.1](https://github.com/arceos-org/arm_pl011) | d4e6caedb617ab9774f452f2b7658e77ad4c6c97 | e32b9dcf7e8b53378fa5eff69bc4a9430d69ffea312170c1ce2b1eaec5ee18b9 | GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0 |
| [uart_16550 0.4.0](https://github.com/rust-osdev/uart_16550) | 9a46f42061a1455c3f5fe206e613ca9071e63dfd | 94d293f51425981fdb1b766beae254dbb711a17e8c4b549dc69b9b7ee0d478d5 | MIT |

Local patches add `init_masked` and `set_receive_interrupt` to PL011 and the
I/O-port 16550 backend. Existing `init` still ends with RX notification enabled.
The masked path never enables RX notification; activation does not clear the
FIFO. The 16550 setup selects IER (clears DLAB) before masking it. Register write
sequences are shared with host fixtures; no kernel dependency is introduced.
The uart_16550 MMIO backend is unchanged and unused by console-driver.

Run `make check_uart` on an x86_64 Linux host. CI executes it alongside
`check_x2apic` in the native x86_64 Build Artifact stage. Kernel console tests
also check masked construction and non-destructive activation through the real
SerialPort adapters. Host fixtures do not prove delivery of physical UART IRQs.

## x2apic

- Published version: 0.5.1
- Upstream: https://github.com/kzhao0986/x2apic-rs
- Source revision: b8f989f781f44ba9a2e504c55dba2f2d526590f5
- Archive: x2apic-0.5.1.crate
- Archive SHA-256: e64dd21b207e918377ce79266b54517c75262031948f06324b6ef8473d3294cc
- License: MIT OR Apache-2.0; original license files are retained.

The import commit preserves the published archive byte-for-byte. Local changes
belong in separate commits. When updating, compare against that baseline,
reapply only required patches, and rerun dependency tests plus x86 kernel
build, Clippy and xAPIC/x2APIC boot tests. Remove the patch override once an
upstream release supplies the required behavior.

### Local Patch

`LocalApic::enable_masked()` first masks LVTERR with a valid error vector,
then prepares a stopped one-shot timer and masks both LINT pins without
changing their routing. `enable_error()` activates error reporting separately
after the host publishes its APIC handle and has a ready fixed-vector handler.
Neither operation alters `enable()` or
introduce any kernel dependency. The register-write observer exists only in
host tests and checks the entire write sequence, not just its final state.
This is not a drain of firmware-pending interrupts or a reset of other LVTs.

Run the dependency regression on an x86_64 Linux host:

```sh
make check_x2apic
```

Do not replace this with only a successful kernel boot: boot alone cannot
detect a temporary unmask followed by a later mask.

CI runs this target in the normal x86_64 Build Artifact stage on a native
x86_64 agent. Other architectures do not execute this architecture-specific
host binary. The standalone lockfile pins x86_64 to 0.15.4, matching the kernel;
this is a local change separate from the pristine archive import.
Kernel unittest boot checks additionally verify the caller's prepared hardware
state before activation; host tests alone cannot detect the host choosing the
wrong initialization API.

`LocalApic::acknowledge_error()` supplies the separate APIC error protocol:
refresh ESR (subject to the legacy maximum-LVT guard), read status, and EOI
once. The host register-access tests verify this order and the legacy branch;
they do not emulate the hardware mechanism that latches physical error bits.

## kvirtiodrivers

- Base version: 0.13.5
- Upstream: https://github.com/727Hsj/kvirtiodrivers.git
- Main import revision: ab99294a6860704142d3b18a9a4884349adceede (`kmain`).
- R1 instance-DMA/reset patch baseline: 5464361169822b3dbd3956017083bf8bbf7a7c2e.
- Reconciliation retains the main import's GPU/EDID, vsock counter handling,
  queue notification barrier and ordered transport initialization updates.
- License: MIT; upstream LICENSE and source notices are retained.
- Imported scope: Cargo manifest, source tree, README and LICENSE, plus local
  instance-DMA changes, documentation, external unit tests and a standalone
  test lockfile. Upstream CI and standalone OS examples are not imported.

The root Cargo patch selects this in-tree snapshot for the existing exact
0.13.5 dependency. It does not depend on an absolute path, a registry-cache
edit, or an unpublished external Git commit. The kernel uses its root lockfile;
the library's own lockfile pins its standalone host tests.
Its local rustfmt and Clippy configurations preserve upstream defaults instead
of inheriting the kernel's formatting and lint-parameter policies. These do
not disable lint checks; the library keeps its own crate-level lint settings.

Local R1-06 changes add a per-device DMA backend for allocation, ring layouts,
buffer/indirect mappings and RNG, retaining static Hal compatibility. See
`kvirtiodrivers/docs/instance-dma.md` for ownership, rollback and safety limits.
R1-07 also adds transport-owned reset confirmation and explicit RNG cancellation.
This is not completed new-model integration or hardware IRQ-tail acceptance.

During this temporary vendoring period, further kernel-delivered library
changes are made here. The external source checkout is a provenance/patch
reference, not a second automatically synchronized source of truth. Keep
library changes separable for upstream submission; do not introduce kernel
crate dependencies into the library. When upstream supplies the changes,
switch to a fixed release/revision, remove the patch and rerun library plus
kernel regression tests. Do not replace this snapshot with an unreviewed copy.

Standalone validation from this directory (using the kernel's pinned toolchain):

```sh
cargo +nightly-2026-03-08 test --manifest-path kvirtiodrivers/Cargo.toml --locked --all-features
cargo +nightly-2026-03-08 test --manifest-path kvirtiodrivers/Cargo.toml --locked --no-default-features
```

Kernel integration must also pass the normal configured Makefile build/test
flow; standalone host tests are not hardware or full-driver acceptance.
