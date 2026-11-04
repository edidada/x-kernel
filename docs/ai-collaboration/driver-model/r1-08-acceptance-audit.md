# R1-08 acceptance audit

Audit starting point: `aa4305341`; current reconciliation baseline:
`00b4f0fc0`, incorporating main `289e2276b`. The current-source commands, results
and scoped decision live in [R1-07/08 closeout](r1-07-08-closeout-2026-11-04.md).
Historical "pending" statements in implementation-status describe their own
slice; later slice results supersede them.

## Requirement evidence

| Acceptance item | Evidence | Current decision |
|---|---|---|
| Real modern-MMIO discovery, exclusive new/legacy ownership | Slices 14/15; boot assertion and non-destructive classification tests | Covered for the scoped direct QEMU Arm virt nodes |
| DMA facts from firmware/platform policy | Slices 1–7; saved QEMU DT, original-node provenance, bounded RAM window and coherency tests; [scoped mask basis](qemu-mmio-dma-mask.md) | Source rationale covered for the selected QEMU modern-MMIO path; 64-bit width is not permission to use all physical addresses or proof of above-4-GiB execution |
| Missing/malformed/unrepresentable DMA inputs | `of`, HAL topology and kdriver firmware-DMA tests; slice 7 regression | Covered for the supported representable window policy |
| Descriptor authority, host bounds, per-instance acquisition/release | Slices 5–13 plus real two-device slice 18 | Covered for coherent identity-DMA QEMU scope; no IOMMU/non-coherent claim |
| Provider installation before probe and failure rollback | Native composition and controlled failure/rollback tests, slices 8–15; [native and concrete RNG SMP tail acceptance](irq-tail-acceptance-2026-11-04.md) | Software gate covered, including shared peer survival and failing bypass negative controls; not physical IRQ/MSI ordering proof |
| Real sampling, pending unbind, old-handle rebind and domain unregister | Slices 15–19 | Covered individually; withheld-response test is not late-response cancellation proof |
| Two instances and stale binding separation | Slice 18, 257-byte reads force fresh I/O in each phase | Sequential isolation covered; not a concurrent IRQ-tail stress claim |
| Existing nodes, attributes and events | Inventory below; devfs/sysfs/uevent tests; dedicated real procfs lifecycle run | Three model device views verified across unbind/rebind/unregister; driver list remains legacy-only |
| Legacy PCI and unaffected paths | Repeated canonical PCI regressions; expanded projection suite in current slice | Keep current-run results separate from historical evidence |
| Documentation and reproducibility | Saved configurations/commands/logs; per-slice tracked status and current-source closeout | Results and limits reconciled in the linked closeout record |

## Source compatibility baseline

Source baselines are `491cb6e8d` (pre-R1 redesign branch) and `28e271c3d`
(immediately before production MMIO selection). They are source inspections,
not newly booted old binaries. Current PCI versus current modern-MMIO runs are
compatibility controls, not a claim to have replayed either historical build.

| Surface | Before production migration | Current implementation / expectation |
|---|---|---|
| `/dev/random`, `/dev/urandom` | Static pool-backed character nodes, device numbers 1:8 and 1:9 | Same root entries and entropy-pool operations; RNG binding does not own or remove them |
| RNG-specific device node and sysfs class attributes | No RNG DeviceNode kind; no `/dev/hwrng` or `class/hwrng` publication through this path | No new per-RNG filesystem ABI is invented |
| RNG userspace add/remove uevents | No RNG DeviceNode lifecycle publisher path | Unbind/remove does not synthesize device-node uevents for a previously absent projection |
| Input/DRM projections | `kclass` registers DeviceNodes; sysfs and kdevice-uevent project them | Existing paths remain; they must not be deleted with obsolete generic driver-class code |
| Internal legacy lifecycle events | Descriptor probe emits Matched/Bound/Published/Activated for legacy objects | Not userspace uevents. Model devices use domain state and hwrng availability/removal; no duplicate legacy object is created just to replay internal events |
| `/proc/devices`, `/proc/bus/devices`, `/proc/bus/topology` | Legacy numeric-object rows | Production migration appends distinct `model:path` rows from current domain snapshots; legacy IDs are not reused |
| `/proc/bus/drivers` | Legacy driver registry and counters | Still legacy-only; model driver enumeration/counters are not currently exported here |

Sources inspected:

- `drivers/subsystems/hwrng/src/class.rs` at `491cb6e8d`: `Class::new(())`,
  not a filesystem/uevent hook; the entropy consumer subscribes to availability.
- `drivers/integration/kdriver/src/driver_registry/virtio/mod.rs`: legacy RNG
  publishes through `attach_legacy`; the production model uses `rng_model.rs`.
- `drivers/contracts/kdevice/src/device_node.rs`: Input and Drm node kinds only.
- `drivers/contracts/kclass/src/lib.rs`: Input/DRM node activation/removal bridge.
- `drivers/integration/kdevice-uevent/src/lib.rs`: userspace events originate
  from DeviceNodeLifecycleEvent, not every internal DeviceEvent.
- `fs/filesystems/devfs/src/root.rs` and `src/nodes/random.rs`: static pool nodes.
- `fs/filesystems/sysfs/src/lib.rs`: DeviceNode and named-cdev projections;
  anonymous pool-node cdev registration is not a new hwrng class.
- `fs/filesystems/procfs/src/device_nodes/root.rs`: model rows in three device
  views, but not in the driver-registry view.

The model-row formatter tests existed in source but were not collected in the
previous default/MMIO regression logs: `kfeat` weakly forwarded the feature to
its unselected optional procfs dependency, while `fs_boot` brought procfs in
through another edge. Those earlier logs are not model projection evidence.
Explicitly enabling `procfs/device-model` from `driver_virtio_rng` fixes the
production feature composition, including RNG-enabled PCI configurations.

The dedicated `procfs::model_mmio_tests` run now passes its one real-device
test: each of `/proc/devices`, `/proc/bus/devices` and `/proc/bus/topology` is
reopened through VFS and read to EOF in four phases. Unbind keeps the same
online registration with an unclaimed/no-driver row; rebind restores its active
row; terminal domain unregister removes it. Evidence is
`r1-08-procfs/ut-procfs.log`; the initial no-tests-found failure is retained as
`ut-procfs-missing-feature.log`. This is a production feature fix plus acceptance
tests, not a test-only change. The legacy-only driver-list view remains a
diagnostic limitation, not model-driver introspection. Pool-node readability
is separate evidence, not inferred from this fixture.

## Next work

Scope update (2026-11-04): the separately delegated
[PCI virtio-net lifecycle follow-up](network-lifecycle-handoff.md) is now owned
by this workstream. It is additional work, not a completed R1-08 requirement;
"only IRQ confirmation remains" must not describe the expanded overall scope.

The deferred software IRQ gate is delivered in `00b4f0fc0`, not inferred from
the independent KIRQ worktree or its uncommitted audit. Both NativeIrq and the
concrete RNG fixture preserve a shared peer after draining the old action.
Ordinary lifecycle and withheld-response runs remain separate evidence.

Next is R1-09 consumer inventory, compatibility-layer retirement where safe,
and full requirements-mapped acceptance. No old projection facility is retired
by this reconciliation; PCI network shutdown still needs its own design and
implementation under the linked handoff.
