# R1-09: generic publication retirement and acceptance

## Baseline and scope

Starting commit: `5e3815f1ef16b2c514f2cc862d9f4a9b298df94d`, incorporating
main `289e2276b`. This is the approved R1-09 retirement, not a new design or a
network lifecycle implementation. Approved requirements and design are unchanged.
The source under test is that baseline plus this retirement patch.

## Consumer inventory and decisions

The inventory searches Rust, Cargo manifests/lockfile, Kconfig and executable
scripts for `driver_class`, `driver-class`, `base/class`,
`DeviceAccess::publish` and `attach_legacy`, then inspects the callers:

| Surface | Actual remaining consumer | Decision |
|---|---|---|
| Generic Class/Registration/Publication/InUse and hooks | Generic class's own tests and three model integration tests; no production registration | Delete base/class and its workspace dependency/lock entry |
| DeviceAccess::publish | Model class-only test | Remove wrapper, not a supported production bridge |
| PublicationSource | Model BindingSource; hwrng registry and legacy source | Move read-only identity contract into model's device/source module; no new trait-only crate |
| hwrng attach_legacy | kdriver's legacy VirtIO RNG publisher, including PCI; controlled tests | Retain until the last legacy publisher migrates |
| contracts/kclass and DeviceNode | Input/DRM node/event projections and other legacy device publishers | Retain; this is not base/class |
| device-res and xkernel adapter | Unmigrated hardware paths | Retain; not dead code or an alias for os-api |

The dependency becomes hwrng → model → os-api. hwrng uses only the provenance
trait, not domain state or lifecycle management. model remains host-free and
does not depend on hwrng. BindingSource retains DeviceRef plus the original
driver's immutable identity, not ops/Data/hardware. Its online bit is device
registration state, not subsystem admission. No new Arc, lock, allocation or
finalizer is added to the running path by moving the trait.

The replacement model test still checks offline-before-remove and retained
metadata after unregister. Two tests specific to retired generic publication
semantics disappear; current broad totals are therefore two lower. Actual hwrng
independent registration, explicit drain, retained handle and rebind provenance
tests remain. Source queries and runtime permissions are not conflated.

## R01–R13 acceptance map

All paths below are repository-relative. Model and host tests run in the broad
suites; destructive real-device cases run in separate guests. Static reviews
are identified as reviews, not fabricated runtime tests.

| Requirement | Current acceptance entry |
|---|---|
| R01 registration/binding separation | model `ut-tests/lifecycle.rs`: bind/unbind/rebind/unregister and duplicate cleanup; real MMIO lifecycle/unregister |
| R02 authoritative facts | model domain identity/view tests and replacement source test; kdriver `hwrng_binding_unittest.rs` retains original driver identity across rebind |
| R03 rollback | model uncommitted probe cleanup; kdriver hwrng post-publication failure, native MMIO/DMA/IRQ acquisition rollback, actual RNG constructor failure |
| R04 deferred progress | model `ut-tests/probe_progress.rs`: before/after parking, dispatch handoff, late notification after unregister; host management/progress tests |
| R05 stale binding isolation | kdriver hwrng binding tests and real MMIO retained handles; hwrng old-handle registration tests |
| R06 safe in-flight exit | rng_drain, concrete RNG lifecycle/stop tests, NativeIrq and RNG SMP tail/peer tests; real pending read with zero backend responses |
| R07 instance isolation | per-instance DMA and hwrng registration tests; real dual RNG reads after each sibling unbind/rebind |
| R08 management/context | model lifecycle reentry rejection; kdriver management contention, IRQ-off/preempt/RCU rejection; drain preparation error before mutation |
| R09 portable boundary | source/manifest inventory, host MMIO/IRQ/DMA tests, unchanged concrete RNG contract-driven call chain; no new unsafe or lint exemption |
| R10 real path | production modern-MMIO boot assertion, real handle reads and lifecycle; entropy collector tests plus production source wiring (compositional, not a real-pool causal assertion) |
| R11 migration compatibility | exclusive MMIO ownership/classification tests; canonical PCI and block/other affected kdriver regression; legacy bridges retained |
| R12 interfaces | existing interface inventory in R1-08 audit; devfs/sysfs/uevent tests and real procfs active/unclaimed/active/absent views |
| R13 extension scope | approved Bus/services/subsystem boundaries reviewed; no speculative PM, dynamic module, hotplug or network mechanism added |

Software IRQ negative controls were run and restored in
[the IRQ-tail record](irq-tail-acceptance-2026-11-04.md), not repeated or
relabelled as new negative runs here. Current positive fixtures must still pass.
Independent vendor tests and cross-platform checkpoints retain their original
baselines unless explicitly rerun; no deleted class test substitutes for hwrng
subsystem acceptance.

## Validation

Evidence directory: `.scratch/driver-model-r1/evidence/r1-09/`.
Serial SMP4 AArch64 QEMU guests use the private regular disk, no network/vsock
or performance-exclusive reservation. Start with the canonical platform
defconfig and `make defconfig`; save config and source patch with results.

| Current-source run | Result | Log |
|---|---|---|
| Canonical PCI broad regression | 434 passed, 0 failed, 6 ignored, 440 total | `pci.log` |
| PCI rerun after final manifest/lock cleanup | 434 passed, 0 failed, 6 ignored, 440 total | `pci-final.log` |
| Worker-enabled modern MMIO broad regression | 435 passed, 0 failed, 5 ignored, 440 total | `mmio.log` |
| Real single-device unbind/rebind | 1 passed, 0 failed/ignored | `lifecycle.log` |
| Real pending-read removal, kdriver suite | 95 passed, 0 failed, 3 ignored | `pending/make.log` |
| Real two-device isolation | 1 passed, 0 failed/ignored | `isolation.log` |
| Real terminal unregister | 1 passed, 0 failed/ignored | `unregister.log` |
| Real terminal procfs lifecycle | 1 passed, 0 failed/ignored | `procfs.log` |
| Focused pinned rustfmt check | exit 0 | `fmt.log` |
| Modern-MMIO clippy with UT | exit 0 | `clippy-ut-mmio.log` |
| Modern-MMIO production clippy | exit 0 | `clippy-mmio.log` |
| Canonical PCI production clippy | exit 0 | `clippy-pci.log` |
| Canonical x86_64 build | exit 0, not a guest runtime run | `x86-build.log`, `x86.config` |

Every completed guest persists coverage. Pending metadata records success and
two EGD requests / 256 requested bytes / zero response bytes, no protocol error,
and ordered BLOCKED then CLOSED markers. Ignored destructive cases in broad
runs are not counted as passes; dedicated guests supply their evidence.
The positive NativeIrq and concrete RNG SMP notification-tail tests, plus the
rewritten provenance test and existing rebind provenance test, pass in PCI.
No current negative-control run is claimed.

The first lint dependency gate found model's now-unused direct `kerrno`
dependency, formerly required only by generic publish. It is removed rather than
suppressed; the initial failure remains in `clippy-ut-mmio-unused-dep.log`.
The following stale-lock rejection is preserved in
`clippy-ut-mmio-stale-lock.log`; the lockfile now matches the cleaned manifest.
The initial runtime tests precede this manifest-only removal; their Rust source
is unchanged. All listed build/lint runs follow the lockfile correction.
The exact-final-manifest PCI runtime rerun also passes and persists coverage.
This is distinct from the initial PCI run, not an overwritten evidence label.

The five MMIO opt-in fixtures and broad suite use the commands documented in
[R1-07/08 closeout](r1-07-08-closeout-2026-11-04.md), with a fresh evidence
directory. The broad crate filter is unchanged. Each destructive terminal
fixture gets a fresh guest; entropy worker is disabled for those tests only.

Lint commands are `make clippy UNITTEST=y` and `make clippy` under the saved
MMIO config, then `make clippy` under canonical AArch64 PCI. x86 uses
`cp platforms/kplat-x86_64/qemu_defconfig .config; make defconfig; make build`.
Canonical AArch64 PCI is restored afterwards. Commands have a 600-second
external failure watchdog. Focused formatting uses
`rustup run nightly-2026-03-08 rustfmt --check --edition 2024 --config skip_children=true`
on the six modified Rust files. Existing dependency future-incompatibility and
unused-optional-dependency advisories are not new failures or suppressions.

## Decision and review

R1-09 is complete within the approved R1 scope on this local branch. R1-01–09
are now implemented and accepted for that scope, not merged upstream. The
additional network handoff and the explicit non-claims below remain outside
this completion statement. Local ticket checkboxes mirror this tracked record.

Independent Standards and Spec reviews found no remaining source or scope
blockers. The stale generic-publication documentation found in review was
corrected, and the unused dependency and matching lock entry were removed,
without lint suppression. Final recorded results follow the actual commands,
not planned runs. `git diff --check` and focused rustfmt pass. The approved
design SHA-256 remains
`bdc322d03950d8395f23e73daa8624a3ad27ec69bd4d75219c758db6d2f86249`.

## Boundaries carried forward

No physical GIC/EOI or MSI ordering proof, late-response DMA injection, IOMMU,
noncoherent DMA, above-4-GiB execution, physical hotplug or measured entropy pool
credit is claimed. The [PCI network handoff](network-lifecycle-handoff.md)
remains additional work, not completed by generic publication retirement.
Existing procfs driver enumeration remains legacy-only, as documented in the
[interface audit](r1-08-acceptance-audit.md). No old Input/DRM projection is removed.
