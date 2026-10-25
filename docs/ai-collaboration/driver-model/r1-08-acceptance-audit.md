# R1-08 acceptance audit

Audit starting point: `aa4305341`. This is an evidence reconciliation, not a
declaration that R1-08 or the deferred IRQ safety gate is complete. Historical
"pending" statements in implementation-status describe their own slice; later
slice results supersede them.

## Requirement evidence

| Acceptance item | Evidence | Current decision |
|---|---|---|
| Real modern-MMIO discovery, exclusive new/legacy ownership | Slices 14/15; boot assertion and non-destructive classification tests | Covered for the scoped direct QEMU Arm virt nodes |
| DMA facts from firmware/platform policy | Slices 1–7; saved QEMU DT, original-node provenance, bounded RAM window and coherency tests | Partial: document the transport/device basis for production `u64::MAX` mask; it is not permission to use all physical addresses |
| Missing/malformed/unrepresentable DMA inputs | `of`, HAL topology and kdriver firmware-DMA tests; slice 7 regression | Covered for the supported representable window policy |
| Descriptor authority, host bounds, per-instance acquisition/release | Slices 5–13 plus real two-device slice 18 | Covered for coherent identity-DMA QEMU scope; no IOMMU/non-coherent claim |
| Provider installation before probe and failure rollback | Native composition and controlled failure/rollback tests, slices 8–15 | Software contract coverage present; native shared-IRQ/notification-tail gate remains open |
| Real sampling, pending unbind, old-handle rebind and domain unregister | Slices 15–19 | Covered individually; withheld-response test is not late-response cancellation proof |
| Two instances and stale binding separation | Slice 18, 257-byte reads force fresh I/O in each phase | Sequential isolation covered; not a concurrent IRQ-tail stress claim |
| Existing nodes, attributes and events | Inventory below; devfs/sysfs/uevent/procfs tests | Scoped compatibility evidence; real procfs observation across lifecycle still to be added |
| Legacy PCI and unaffected paths | Repeated canonical PCI regressions; expanded projection suite in current slice | Keep current-run results separate from historical evidence |
| Documentation and reproducibility | Saved configurations/commands/logs; per-slice tracked status | Ongoing until remaining gates close |

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

The procfs model-row formatting tests verify refreshing/omitting rows, and real
model lifecycle tests verify the underlying registration states. Neither alone
is an end-to-end read of those proc files during real unbind/remove. Keep that
acceptance gap explicit. The legacy-only driver-list view is also a documented
diagnostic limitation, not silently counted as model-driver introspection.

## Next work

1. Close the production DMA-mask rationale against the actual transport/platform
   contract without weakening the bounded-window validation.
2. Add real procfs lifecycle observations: unbind retains the device row with no
   bound driver; domain unregister removes the row. Pool-node presence/readability
   and unrelated projections must remain independent of hwrng binding state.
3. Complete the deferred shared-IRQ/notification-tail gate when that prerequisite
   is ready. No mock, ordinary lifecycle run or withholding backend replaces it.
4. Only then reconcile final R1-08 checkboxes and unblock R1-09 retirement and
   full requirements-mapped acceptance. No old projection facility is retired now.
