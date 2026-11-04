# kdriver — Design

## Unified IRQ ownership

Host services now acquire DevIrq with an Arc-held provider instead of building
an extra release closure. Existing static-provider consumers use the same
guard and IrqOp/IrqResource definitions. IRQ descriptors in legacy device
discovery are os-api resources; no local device-res IRQ shape remains.
Guard acquisition does not change binding identity, cleanup ownership or
hardware stop requirements. Static-provider mapping/admission policy remains
unchanged; the stricter NativeIrq policy is not imposed on old consumers here.

## Host service composition

`src/host/mod.rs` exposes domain service constructors. Private `services.rs`
composes capabilities, extensions and management locks; `management.rs`
implements the sleepable device-management lock. Identity, descriptors and
cleanup storage remain model-owned. This is framework integration, not a
separate services crate or a new provider registry.

The provider crate lives under `drivers/adapters/xkernel/provider` and does
not depend on this module or driver-model. NativeIrq and its lifecycle tests
now live there; host services only compose it. Native parts of `host_dma.rs`
remain to be consolidated without losing per-device DMA constraints.

MMIO acquisition in both device models delegates to the single host
MmioOp implementation. Model host_mmio only translates os-api errors to
RuntimeError; it no longer allocates a release hook or implements native
map/unmap itself. Device-managed cleanup retains the same IoMap guard.

## Discovery statistics during model coexistence

EnumerationContext stages model RNG registrations and legacy descriptors, then
dispatches both paths outside discovery locks. Its `legacy_registered_count`,
`legacy_unclaimed_count` and `legacy_unclaimed` describe only legacy kdevice.
`probe_pending` returns only legacy descriptor activations. Requeued and failed
descriptors also enter the legacy unclaimed list; it is not a pure no-match count.
Manager logs explicitly exclude model devices and distinguish descriptor
activations from the nonnegative increase in legacy active records, which may
also reflect other registration/removal activity. Neither is a cross-model total.
Model registration success is not counted as activation. Model probe failures
are diagnosed by driver-model; staged conversion/registration failures keep their
existing logs and do not fall back to legacy registration.

## Native IRQ notification-tail acceptance

The serial SMP tests in `../../adapters/xkernel/provider/ut-tests/native_irq_smp.rs` and
`ut-tests/virtio_rng_notify_tail.rs` use NativeIrq registration and synchronous
release against isolated, metadata-free software IRQ descriptors. KIRQ supplies
a test-only dispatch entry using its actual action snapshot and in-flight
accounting, without a hardware claim. The coordinator and removal task run on
CPU0; the gated hardirq callback runs on CPU1. An explicit handshake observes
the removal task blocked before opening the callback gate. All workers retire
before checking teardown observations.

The native case verifies resource retention, exact-action withdrawal, repeated
withdrawal and continued shared-peer dispatch. The concrete RNG case gates its
injected completion notification outside the request lock, after a controlled
request completes. Closing reads can therefore proceed, but queue destruction
must wait for the full callback to return. The completed request has already
been unmapped; it must not be cancelled or unmapped a second time.

These are software integration tests, not controller delivery, EOI timing,
physical DMA shutdown or MSI ordering evidence. Execution results and the
temporary negative-control protocol are recorded in the driver-model IRQ
acceptance documents at repository root.

## Scoped production MMIO RNG model binding

The compatibility inventory and remaining acceptance gaps are tracked in
`docs/ai-collaboration/driver-model/r1-08-acceptance-audit.md` at repository root.
Pool-backed random nodes are not owned by an individual RNG binding. Existing
Input/DRM projections remain on their current contracts; migrating RNG is not
permission to retire those facilities or invent a per-RNG filesystem ABI.

The opt-in `model_mmio_lifecycle` regression uses the existing global-domain
unbind/bind operations and hwrng consumer handles against a single real model
RNG. Enable it only with `XK_TEST_MODEL_MMIO_LIFECYCLE=1` on the modern-MMIO
QEMU fixture, without competing device-management operations. It preserves the
registered device, restores a live binding before checking destructive-phase
observations, and keeps a fallback restoration guard for early returns.
Ordinary test configurations skip this hardware-mutating acceptance case.
Serial test execution excludes other test cases, not the kernel entropy worker;
the case does not establish that a request was pending at withdrawal or certify
IRQ notification-tail synchronization.

### Opt-in withheld-response pending read fixture

`ut-tests/model_mmio_pending.rs` provides
`real_model_mmio_rng_pending_unbind_releases_reader`, separate from the sampling
lifecycle case. Reproduction requires an aarch64 QEMU modern-MMIO configuration
with the RNG driver enabled, the VirtIO entropy consumer disabled, exactly one
matching model device/publication, and no competing consumers or management
operations. Build with `XK_TEST_MODEL_MMIO_PENDING=1` and leave
`XK_TEST_MODEL_MMIO_LIFECYCLE` unset. Serial execution excludes other tests, not
kernel workers; disabling the entropy consumer is a separate prerequisite.

The host harness `scripts/tests/run_model_rng_pending.py` runs the kdriver-only
suite with a controlled `rng-egd` backend withholding all response bytes from
boot. With the prepared configuration and a private regular `disk.img` in this
worktree, its entry point is
`python3 scripts/tests/run_model_rng_pending.py --log-dir /tmp/pending-01`.
The harness supplies the pending and boot-expectation opt-ins; ordinary boot
checks do not sample the RNG. The pending fixture is the sole nonempty reader
of the real RNG in this configuration.

The guest observes Started + Blocked + unfinished before printing the exact raw
line `MODEL_MMIO_PENDING: BLOCKED` through `unittest::ktest_println!`. It then
unbinds through the existing global domain, retains registration, waits for the
reader to exit, and restores the binding only after successful cleanup. It
checks the reader's BadState result and unchanged buffer, the unclaimed interval,
replacement publication identity, and rejection of a retained old handle without
buffer mutation. There is no post-rebind nonempty read while responses remain
withheld. `MODEL_MMIO_PENDING: CLOSED` is printed only after cleanup, restoration
and every assertion succeed.

Host acceptance requires both exact markers in order, the named test passing,
the suite all-pass result, positive EGD request count and zero responses. Guest
Blocked state alone is insufficient; socket and console buffering also preclude
cross-FD chronology claims. These are fixture responsibilities and acceptance
criteria, not a recorded validation result or proof of late-response/IRQ-tail
safety.

### Opt-in dual-device isolation fixture

`model_mmio_isolation` is a serial hardware acceptance case enabled only by
`XK_TEST_MODEL_MMIO_ISOLATION=1`. It requires exactly two modern-MMIO model RNGs,
independent host RNG backends, no VirtIO entropy worker, and neither the lifecycle
nor pending opt-in. The two registered devices must have distinct identities,
paths and non-overlapping MMIO ranges. No consumer metadata API is added for
testing: handles are associated behaviorally by observing exactly one original
publication close when a selected device is unbound, while the other stays live.

Each device is unbound and rebound in turn. While one is Unclaimed, the peer
must retain its Bound registration and exact publication and perform guarded
reads totalling more than the driver's 256-byte request/cache bound. This forces
new device I/O rather than accepting only data cached before peer teardown.
Rebinding must retain the selected device identity, introduce a
new publication, and permit a fresh read while the old handle remains closed.
Restoration precedes destructive-phase assertions, with a fallback guard for
early returns. This tests symmetric lifecycle isolation, not concurrent reader
races, pending cancellation, entropy quality or IRQ-tail synchronization.

Run the kdriver-only suite with the pending/lifecycle flags unset and
`XK_EXPECT_MODEL_MMIO_RNG=1 XK_TEST_MODEL_MMIO_ISOLATION=1`, adding
`QEMU_ARGS='-global virtio-mmio.force-legacy=false -object rng-random,id=host_rng1 -device virtio-rng-device,rng=host_rng1'`.
Use the prepared aarch64 MMIO configuration, a private worktree disk,
`XKMAKE_ARGS='--no-net --no-vsock'`, and an outer failure watchdog. Ordinary runs
ignore this test; enabled runs reject the wrong device count before mutation.

### Dedicated one-shot unregistration fixture

`model_mmio_unregister` tests real registration removal, not another unbind.
It requires `XK_TEST_MODEL_MMIO_UNREGISTER=1` and the exact filter
`UNITTEST_CRATE=kdriver::platform_model::unregister_tests`. The opt-in rejects
a broader suite before mutation because a removed registration is permanently
offline. The dedicated guest intentionally ends with this boot device removed;
coverage persistence and guest shutdown do not require its RNG. There is no
manual registry restoration, fabricated replacement core or new test-only API.

Use the single-device aarch64 modern-MMIO configuration with VirtIO entropy
consumption disabled and the lifecycle, pending and isolation flags unset:
`XK_TEST_MODEL_MMIO_UNREGISTER=1 make unittest UNITTEST_CRATE=kdriver::platform_model::unregister_tests XKMAKE_ARGS='--no-net --no-vsock' QEMU_ARGS='-global virtio-mmio.force-legacy=false'`.
Use a private disk and an outer failure watchdog. The test samples more than the
current 256-byte cache bound before removing through the global domain. Retained
device references must keep readable identity/descriptor metadata but become
offline, disappear from domain snapshots, and reject bind/unbind. The retained
consumer must close and reject nonempty reads without buffer mutation. Repeated
remove must succeed without restoring publication or registration.

The fixed boot-DT platform owner deliberately retains its MMIO claim after
domain removal; another enumeration context must still recognize the range as
owned rather than hand it to the legacy path. This verifies domain unregister
under the existing boot ownership policy, not releasing the platform slot or
supporting physical hot-unplug/re-enumeration. Those require an owner-level
policy, not direct test mutation of the slot map.

### Production ownership

`PlatformRngRegistration` is the transitional registration owner for selected
boot-DT RNG nodes, not the general platform model or a transport driver.
`ensure_root_initialized()` may install services and register the root and RNG
driver once; it is not a side-effect-free bus accessor. The module remains
`platform_model` to preserve existing test-filter entry points during migration.

With `virtio-rng`, EnumerationContext::new always obtains the same
`production_owner()` from LazyInit<Arc<PlatformRngRegistration>>; managers have no separate
platform-owner field.
DeviceManager instances and public default bus-manager enumeration routes share
one PlatformRngRegistration, root bus and native-service installation; manager identity
is not hardware ownership. The source is the single immutable boot DT. HAL node
IDs are full-tree u32 indices, stable independently of matching but not across
boots, overlays or tree replacement. Staging copies the original compatible
strings into PlatformBusData rather than trusting matcher aliases.

The new path selects only aarch64, available direct `virtio,mmio` children of
the `linux,dummy-virt` root classified as modern RNGs. Availability includes
ancestors. Other supported devices retain legacy routing. Once selected,
resource conversion, root setup, registration or probe failure never falls
back to the legacy RNG publisher. Staging requires a complete MMIO extent,
supported GIC route/trigger and coherent identity DMA. Explicit QEMU policy
may fill only MissingWindow from the same tree's single RAM interval; explicit
windows and firmware errors retain precedence.

The platform backend holds the shared owner.discovery SpinNoPreempt lock across
ownership lookup, temporary read-only
classification, and reservation or legacy descriptor registration. This closes
the gap across separate managers, not just rescans of one manager. Known model
ranges are not mapped or read again; exact legacy descriptors are reused for
their probe/skip decision and overlapping ranges are skipped. A model candidate
becomes Slot::Reserved, keyed by boot node ID and rejecting overlapping ranges.
Its reservation survives the unlocked registration/probe interval. Dropping
failed staging removes its reservation; successful registration replaces it
with Slot::Registered and retains DeviceRef even if probe has not succeeded.
This fixed-tree ownership is not hotplug slot reclamation or a retry framework.

DeviceManager releases its bus-manager lock before probe_pending consumes owned
staging. The shared discovery lock and slot lock are also absent during model
registration/probe. Root setup is serialized by a sleepable mutex, installs
native services on global_domain, creates the parentless PlatformBus instance
`firmware-platform`, registers its scoped RNG driver and caches success/failure.
Device registration runs after the root mutex is released. Devices retain native
MMIO/DMA/ordinary IRQ services and per-device management locks. Legacy descriptor
counters are not a census of model activations.

`driver_registry/virtio/rng_model.rs` validates instance, context, DMA and modern
RNG identification before transport construction. RngBinding owns IoMap, the
concrete VirtIoRngDev with DeviceDmaBackend, and an explicit HwrngRegistration
published with the binding's immutable source identity. IRQ acquisition uses
the model resource factory. Constructor errors unmap only after the runtime's
rollback contract releases queues/transport; publication failure explicitly
stops the constructed runtime before returning an error.

Remove and Data Drop share idempotent shutdown: close reads, unregister/drain
publication, then finish_stop with yielding reset polls. Reset confirmation
precedes synchronous IRQ release and pending-request cancellation/DMA release.
Data Drop unmaps only after shutdown succeeds, including successful-but-rejected
probe cleanup. Old consumer handles retain an inert runtime, not a live transport
or the binding's MMIO ownership. Shutdown failure panics at the current
infallible remove/Drop boundary and retains the manually held mapping rather
than claiming quiescence; no recoverable teardown retry protocol is supplied.

This is production routing/binding code, not merely a fixture. It does not
establish final hardware remove/rebind, DMA/cache correctness or IRQ
synchronization acceptance. Controlled fixture and host ownership tests retain
their narrower evidence scope; whole R1-08 hardware acceptance is not claimed.

## Read-only VirtIO MMIO discovery

Platform enumeration first reuses an existing MMIO descriptor, queuing its ID
for the existing core probe/skip decision without rereading owned registers or
creating another descriptor. Overlapping, nonidentical ranges are skipped. The
shared production discovery lock serializes this path across bus managers when
virtio-rng is enabled; independent registration outside this owner is unsupported.
Legacy reuse uses its existing registry; model devices use the reservations
described above. A registered descriptor
reserves its MMIO range even while a probe is in progress or has not yet succeeded.

Previously unregistered slots are classified through virtio::classify_mmio_device,
which reads identification registers without constructing a transport. Discovery
must not run MmioTransport::Drop: its reset would alter a device owned by an
existing binding on rescan. Actual activation still creates the transport.
The temporary discovery mapping reference is released after classification,
including invalid and empty slots. Raw classification must not alias an active
transport's UniqueMmioPointer; it is a pre-ownership operation only.

This follows the separation visible in Linux drivers/virtio/virtio_mmio.c:
identification reads precede device registration and transport operation.
The scoped production path adds reservations and owned staging to this seam;
registration/probing runs outside discovery locks. Classification alone still
conveys no binding ownership.

## Native ordinary IRQ composition

`host::new_native_device_services` composes the existing native MMIO,
DMA and management-lock services with `driver_os_api_xkernel::NativeIrq`. Construction does
not install services, enumerate firmware, activate a bus or probe any device.
The `device-model` feature enables the direct kirq dependency independently of
block drivers. The scoped PlatformRngRegistration root now installs this composition;
constructing the bundle alone still performs no discovery.

Firmware routes go through kirq::try_map once per host-service request. Explicit
domains are preserved; only absent domains select GIC, PLIC or IO-APIC roots.
Other controller families without an explicit domain are unsupported. A returned
resource retains the actual virq, domain, controller, hwirq and trigger. Resolved
virqs use PlainVirq without firmware remapping. Registration, enable and release
convert the retained resource, never reconstruct a domain from its controller.

Plain-virq requests first query kirq::descriptor by the virq. Missing descriptors
and conflicts with a specified trigger are rejected before registration; an
Unknown trigger supplies no new configuration requirement. Successful requests
still pass PlainVirq, preserving stored hwirq/domain/trigger rather than using
the virq as a hardware number. The lookup is a preflight snapshot: the bus owner
must keep the resolved mapping stable through registration and release. Missing
MSI domains have no default-root fallback; MSI allocation remains unsupported.

Requests require sleepable context and adapt ordinary os-api IrqHandler callbacks to
kirq shared actions. The first action retains kirq's auto-enable policy; this
is not a disabled-registration API. The host guard captures the provider,
resource and returned action ID. Release removes that exact shared action and
synchronously drains it before returning, outside locks needed by callbacks.
Queued replay must be able to drain through kirq's boot-installed BH executor.
Core ConfigurationFailed maps to Io and UnsupportedFlow to Unsupported.
Fallible reverse-map owner preparation maps NoMemory to NoMemory; hardware
numbers outside the domain range map InvalidHwirq to InvalidResource. Mapping
and registration preserve these errors rather than manufacturing a resource/token.
Configuration failure publishes no new action token and preserves existing
shared owners, but may retain resolved metadata and leave hardware masked; it
does not guarantee hardware rollback. Successful retry restores delivery only
when logical enablement and disable depth allow.
There is no host action registry, threaded IRQ, pseudo-NMI or MSI allocation.

Provider native_irq UT exercise pure conversion and injected map/register/free
helper seams, including nonidentity numbering, explicit domains, callback results,
context rejection and release errors. They do not register real interrupts or
modify production root domains, and do not prove IRQ hardware or synchronization
acceptance. Production use of this composition does not complete hardware
lifecycle/IRQ acceptance or establish whole-ticket completion.

## Retained device DMA capability

`host_dma::new_device_dma` creates the OS-api DeviceDma capability from the
current DeviceContext. It retains the original DeviceRef and reads its descriptor
on each operation, without caching DmaInfo or consulting a replacement domain
service. HostDeviceServices delegates its DMA factory here. Missing configuration,
non-coherent devices and invalid masks are rejected. Coherent allocation retains
the existing page-rounded validation and original-token release path.

Streaming operations use kdma's coherent bounce pool. Translation checks the
bounce buffer's CPU physical address and entire requested length, not the
caller's buffer address or native bus token. Rejection consumes the unpublished
mapping through discard_dma_buffer without receive copy-back. Success transfers
the native token, original buffer address, length and direction into a private
StreamingMapping release hook. Drop invokes normal unmap once, preserving
directional copy-back; the original buffer remains caller-owned.

All streaming buffers must be initialized, including receive buffers. Before
publication, map_native_streaming copies the original bytes into receive bounce
storage; kdma already initializes the other directions from the original. This
matches Linux swiotlb's transparent bounce semantics: partial device writes
preserve unwritten bytes, and ordinary unmap without device writes preserves the
entire original buffer, including unpublished queue rollback. It does not add a
submitted-DMA cancellation protocol or change the requested DMA direction.

The host receive pre-copy is followed by a SeqCst fence before returning the
native mapping for publication, matching kdma's outbound post-copy ordering.
The earlier native mapping fence cannot order this later host copy; initialization
therefore does not rely on a consumer's subsequent queue barrier. This ordering
does not add cache maintenance or support for non-coherent devices.

Guards do not require the capability to remain alive. There is no second mapping
registry, retry policy or implicit hardware stop. Production activation belongs
to the scoped binding above. The
private publish_streaming helper accepts an acquired unpublished native mapping
for deterministic publication/rejection tests, not submitted-DMA cancellation.

## Model host MMIO mapping

`host_mmio::iomap` delegates to the shared host MmioOp implementation over memspace.
It accepts explicit bus-resolved regions as well as descriptor-indexed resources;
it does not invent a second device resource registry or authorization policy.
The returned IoMap owns one native mapping reference, not necessarily the entire
mapping: overlapping guards may share the native mapping, and pinned boot
mappings must survive guard Drop. Each successful acquisition retains the static
provider and its token without allocating a hook; unmap uses the original address,
including page offset. Native errors retain Invalid/NoMemory/Io distinctions;
release failures are logged because Drop cannot return an error.

The native backend reserves registry capacity before creating a mapping and
rejects overflow of page-rounded physical endpoints. Its internal page-table or
platform-preparation recovery is not replaced by the adapter. Guest tests map
but never access a synthetic test physical interval: two guards must share
one mapping, the first Drop preserves it, and the last removes its registry
entry and VMA. A model callback fixture also checks descriptor-indexed mapping
through bind/unbind/rebind and removal. This verifies host mapping ownership,
not real hardware register behavior.
The smoke test targets the ordinary QEMU configuration, not optional KVM MMIO
guard hypercalls. The scoped production root now installs this mapping service.

## New-model firmware DMA configuration

`firmware_dma::from_firmware` converts the DMA result carried by `FirmwareDevice`
into a portable `DmaInfo` for a new-model descriptor. Firmware errors remain
typed, missing range/coherency is explicit failure, and device masks are supplied
separately from the firmware bus window. It revalidates public range values at
this seam. No fallback, memory allocation, MMIO access or model registration
occurs here. This is an enumeration capability, not a concrete-driver dependency.
The scoped production handoff uses this converter plus explicit QEMU policy;
constrained allocation search remains unsupported by the native adapter.

## Purpose

`kdriver` is x-kernel's device-driver orchestration and host-integration
crate. It owns bus-backend management, driver registration and matching,
the full discover → bind → activate pipeline, and devres-lifecycle-based
resource management (mmio / irq / dma / time). All activated runtime
devices are ultimately published into the matching typed `kclass`
registries.

`kdriver` is allowed to touch X-Kernel host APIs, but only as a provider
implementation holder, bus discovery backend, or concrete driver glue.
Reusable concrete drivers must still depend only on driver subsystem
contracts.

The intended audience is developers adding new bus backends, new device
drivers, or changing device discovery/matching policy.

## Background

The core of the Linux device model separates bus, device, and driver: the
bus discovers devices, drivers declare the device characteristics they
support, and the kernel performs matching and binding in between.
`kdriver` lands that architecture as crate-level abstractions:

- bus backends are abstracted behind the `BusBackend` trait, with PCI and
  platform instances;
- device descriptors (`DeviceDesc`) are produced by bus discovery and do
  not directly create runtime objects;
- drivers declare matchers and probe callbacks through the `DeviceDriver`
  trait;
- `kdevice` provides the shared device core, owning the persistent
  topology of `BusInstance`, `DeviceObject`, and `DriverObject`;
- `kclass` provides the typed publication entry per category
  (net / block / display / input / vsock / char / 9p).

## Scope

```text
drivers/integration/kdriver/
├── Cargo.toml
├── docs/
│   ├── design.md
│   └── security.md
└── src/
    ├── lib.rs                   # crate entry, init_drivers, ownership summary API
    ├── manager.rs               # DeviceManager and the unified discovery pipeline
    ├── enumeration.rs           # EnumerationContext, descriptor buffer bus backends write
    ├── resource.rs              # provider holder and DriverResult wrappers
    ├── block_completion.rs      # block wait/signals host provider
    ├── block_completion_dispatch.rs  # block completion reclaimer (softirq batches)
    ├── block_irq.rs             # shared block IRQ request adapter
    ├── bus/
    │   ├── mod.rs               # bus module root
    │   ├── backend.rs           # BusBackend trait
    │   ├── manager.rs           # BusManager multi-backend management
    │   ├── pci_backend.rs       # PCI bus discovery
    │   ├── pci_support.rs       # PCI BAR allocation and device configuration
    │   ├── platform_backend.rs  # platform bus (firmware + static devices)
    │   └── local_id.rs          # bus-backend local id allocator
    ├── driver_registry/
    │   ├── mod.rs               # DriverRegistrar, ownership summary types
    │   ├── firmware_specs.rs    # firmware match specs for platform drivers
    │   ├── virtio/
    │   │   ├── mod.rs           # VirtIO driver descriptors and activation
    │   │   ├── ids.rs           # VirtIO device type codes and PCI id mapping
    │   │   └── glue.rs          # VirtIoHal implementation, kdma/iomap binding
    │   ├── block/               # block driver registration
    │   │   ├── mod.rs           #   ahci, bcm2835_sdhci, ramdisk, sdmmc registry
    │   │   ├── ahci.rs          #   AHCI platform driver glue
    │   │   ├── bcm2835_sdhci.rs #   BCM2835 SDHCI glue
    │   │   ├── ramdisk.rs       #   ramdisk glue
    │   │   └── sdmmc.rs         # sdmmc glue
    │   ├── char/
    │   │   ├── mod.rs           # char device driver registration (console)
    │   │   └── serial.rs        # serial/UART platform driver glue
    │   └── net/
    │       ├── mod.rs           # net device driver registration
    │       ├── fxmac.rs         # FXMAC platform driver glue
    │       ├── ixgbe.rs         # ixgbe PCI driver glue
    │       └── ixgbe_hal.rs     # ixgbe HAL adapter
    └── tests/                   # block host tests (physically grouped)
        ├── block_completion.rs
        ├── block_completion_dispatch.rs
        ├── block_irq.rs
        ├── block_requests.rs
        └── virtio_block_activation.rs
```

## Architecture

When `block` is enabled, `HostBlockWaitProvider` implements the block host
contract: allocation-free context validation before submission and waiter
preparation only for an incomplete bio. `init_drivers` installs it before
discovery. `kruntime/fs` explicitly enables `kdriver/block`, so filesystem
consumers including devfs loop get this service without a physical block driver.
HostSignals owns one permanent `kcompletion::Completion`.
`HostBlockWaiter` uses `ktask::completion::CompletionWait` to retain and
validate the preparing task. Scoped stack registration retains early signals
and rechecks completion after spurious wakes, without heap allocation or a
recoverable registration error. Only terminal waiting remains; virtio refills
its own pending queue. Explicit device-close drain prepares its waiter before
teardown. Concrete drivers depend only on the portable contract.

Each `prepare_block_wait` allocates two metadata heap objects: the boxed waiter
and HostSignals Arc. An already completed bio skips both. Main's BioWait still
prepares these objects lazily after submission under the infallible OOM policy;
this migration does not make the complete bio lifecycle allocation-free.
After preparation, waiting and notification allocate no wait resources.
The scheduler uses intrusive ready queues; default unfiltered scheduler tracing
uses preallocated capture storage. Arbitrary tracing callbacks/filters are
outside that guarantee.

```text
                        init_drivers()
                             │
                    discover_unified()
                             │
                    ┌────────┴────────┐
                    │   DeviceManager │
                    └────────┬────────┘
                             │
              ┌──────────────┼──────────────┐
              │              │              │
         BusManager    EnumerationContext  DriverRegistrar
              │              │              │
    ┌─────────┴─────────┐    │     ┌────────┴────────┐
    │                   │    │     │                 │
 PlatformBackend    PciBackend  │  virtio drivers   platform drivers
    │                   │       │  (net/blk/gpu/    (ramdisk/AHCI/
    │                   │       │   input/vsock/9p)  sdmmc/fxmac/…)
    │                   │       │
    └───────┬───────────┘       │
            │ probe             │
            ▼                   │
     ┌──────────────┐           │
     │  kdevice core │◄─────────┘
     └──────┬───────┘
            │ publish
            ▼
     ┌──────────────┐
     │ kclass registries │
     │ net / block  │
     │ display      │
     │ input / vsock│
     │ char / 9p    │
     └──────────────┘
```

| Component | Responsibility |
|------|------|
| `DeviceManager` | Holds `BusManager` + `DriverRegistrar`; orchestrates the full discover → match → bind → activate pipeline |
| `BusManager` | Manages coexisting backend instances; owns the `early_init` → `enumerate` → `rescan` → `quiesce` → `remove` lifecycle |
| `BusBackend` | Bus-backend trait; implementers write `DeviceDesc`s through `EnumerationContext` |
| `EnumerationContext` | Descriptor buffer: records descriptors produced during discovery, then runs the unified probe |
| `DriverRegistrar` | Driver registration facade: registers `DeviceDriver` impls into the `kdevice` driver core |
| `PlatformBackend` | Unified platform bus: firmware-described (device-tree / ACPI) plus compile-time-known static devices |
| `PciBackend` | PCI bus: ECAM/MmioCam enumeration, BAR allocation, host-bridge / PCI-to-PCI-bridge adoption |
| `driver-os-api-xkernel` | x-kernel resource provider implementation; bridges `memspace` (iomap), `kirq` (irq), `kdma` (dma) |
| VirtIO driver family | Each VirtIO device type yields two `DeviceDriver` descriptors (PCI/MMIO) sharing one activation path |
| Platform driver family | ramdisk, AHCI, bcm2835-sdhci, sdmmc, fxmac, and other platform device drivers |

The disabled AHCI integration only admits the SDK on an explicitly
hardware-coherent DMA host (currently x86_64). Its SDK `flush_dcache` callback
is an ordering barrier, not cache clean/invalidate. Non-coherent hosts are
rejected before SDK initialization; CPU alignment staging does not implement
that missing DMA/cache protocol. Production features/SDK/probe remain disabled.

## Block Device Completion Processing

`block_completion_dispatch` owns one `Arc<BlockIoReclaimer>` per device.
Probe creates it; IRQ calls its `mark_pending()`; task-context shutdown calls
its `stop_and_wait(&self, waiter)`. `BlockCompletionOperations` is implemented by the actual device and defines
how to reclaim completed requests, not host scheduling or request submission.

The reclaimer holds an intrusive pending_link, Weak device, ProcessingState
(phase, is_accepting, stop_notification), and a task-only stop mutex. One global
PENDING_DEVICES SpinNoIrq protects the per-CPU lists and all processing state.
Idle/Queued(cpu)/Running(cpu)/RunningAgain(cpu) are the processing phases. Detachment gives
the pinned CPU a finite batch; detached nodes remain Queued and cannot relink.
Callbacks and notifications run outside the global lock. RunningAgain requeues
only into the next batch. Enqueue and raise stay pinned across daemon wake.

Concurrent stop callers serialize on stop_lock through the terminal wait:
the single stop_notification cannot be overwritten. IRQ and softirq never take
this mutex, so it adds no lock acquisition to the I/O hot path. Stop takes a
fresh task-bound completion prepared before teardown; its sole lazy terminal
registration cannot fail after disabling processing. A later closer finds Idle
and returns.
The lock order is stop_lock -> PENDING_DEVICES, never the reverse.

Arc Drop does not stop processing. Activation must explicitly finish admitted
I/O, suppress device IRQ generation, release/synchronize its Irq, then call
stop_and_wait before destroying the transport. The owning CPU consumes disabled
queued entries without invoking the device. Stop waits for a running callback's
temporary device Arc to retire. Do not call stop from the device callback.
CPU unplug and forced callback cancellation remain out of scope.

Each device allocates one reclaimer Arc with an embedded stop mutex;
there is no per-mark allocation or data copy. Existing host scheduler wake costs
are unchanged. Request-local waiting remains separately owned in block_completion.

## Shared Block IRQ Lifecycle

### DMA And Execution Context

Ordinary block completion follows `complete_* -> pop_used -> Hal::unshare ->
driver_os_api::unmap_streaming -> kdma::unmap_dma_buffer`. The existing provider
removes its mapping, copies reads back and returns bounce storage to the TLSF
pool. Pool creation is on map, not unmap; pool recycling and indirect descriptor
deallocation do not sleep in the audited implementation. Coherent virtqueue
destruction may perform different work and remains in task-context close.

This permits the Block softirq backend with the current provider. A provider
that needs sleepable reclamation must supply a task-context backend satisfying
the same block contracts. Neither hardirq nor softirq is sleepable.

No new block data copy is introduced, but the existing DMA bounce copies remain.
Reclaim cost depends on bytes as well as request count; finite batches and the
softirq outer-loop reschedule check do not bound a single pass in microseconds.
The existing scheduler's EEVDF enqueue can allocate, so allocation-free block
bookkeeping is not a claim of allocation-free end-to-end wakeup.

### Registration And Removal

`request_block_irq` returns the existing `driver_os_api::DevIrq` directly. Its closure
holds a Weak device IRQ handler and the device's reclaimer Arc. It acknowledges
only that device, marks pending only for a claimed event, and preserves the
complete IrqEvent. An expired device returns NOT_HANDLED.

Each disk requests its own shared action through XKernelResourceProvider.
kirq owns fanout, capacity, compatibility checks and callback synchronization.
Provider errors propagate: no block bucket, capacity workaround, local callback
gate, extra registration owner, retry or polling fallback. In particular the
current four-action capacity can make setup fail; changing it belongs to kirq.

Dropping Irq in sleepable task context releases that action token. The existing
provider delegates to kirq, which waits for in-flight dispatches; currently this
is line-wide synchronization and can wait for a neighboring callback too.
The block adapter never masks the shared line. Activation must retain a strong
device reference through IRQ release and reclaimer stop. Callback snapshots
never own the activation object or its synchronous destructor.

IRQ adapter tests exercise per-device provider calls, token release, neighbor retention,
claimed/unclaimed dispatch, exact event propagation, expired targets and provider
failure. The mock has synchronous dispatch only; it does not prove concurrent
kernel release. That synchronization is covered by kirq's own regression tests.
driver_registry/virtio/block.rs owns DeviceId activation, rollback, publish-last
and hardware removal on top of the virtio request lifetime.
tests/block_requests.rs injects the real wait provider into that fake, blocks 24
callers across two CPUs, then acknowledges simulated IRQs and drives this
reclaimer through Block softirq. It does not publish a production IRQ disk.
Production PCI/MMIO block activation uses try_new_irq and the real host provider.
Driver initialization also installs that provider for block-core BioWait; disk
publication uses alloc_mq and the common Request data entry point.
Missing/failed IRQ leaves no published disk. One BlockActivation owns the strong
device, optional native Irq, reclaimer and optional Gendisk throughout setup and
close. A task-only map stores an owned FnOnce close action to erase heterogeneous
transport types; IRQ and request paths never consult that map. A local activation's Drop also handles
partial setup failure. Registry and devres cleanup ownership precede publication.

VirtioDriver::remove takes the unique close action before bus teardown; device
core's begin_removing serializes remove, and its later devres cleanup repeats
the now-idempotent lookup. Close prepares fresh task-bound drain/stop waiters,
marks the device Closing, removes owned block lookup, waits for admitted calls,
suppresses device interrupts, drops native Irq, stops the reclaimer, then
destroys the transport. No registry/class spinlock spans a wait. Retained disks
can query cached geometry but new I/O returns Io. Physical tests are grouped in
src/tests/virtio_block_activation.rs and included beneath their owning module.

Block host tests are physically grouped in src/tests/. Each unit test file is
included under its owning module with #[path], preserving private access and
the existing test namespace without a test-only directory per runtime module.

## Initialization Paths

Initialization is split into three deliberately separate paths:

### 1. Platform early init (`early_driver_init`)

Runs before the generic driver model and brings up the subsystems that must
work earliest: timer, IRQ, and the boot console. This stage does not go
through bus enumeration — the platform HAL starts it directly.

### 2. The descriptor-first path

This is the main path, driven by `init_drivers()` → `discover_unified()`:

1. **Bus enumeration**: `PlatformBackend` and `PciBackend` each enumerate
   devices, writing `DeviceDesc`s into the `kdevice` shared core through
   `EnumerationContext::register_device`.
2. **Driver matching**: `EnumerationContext::probe_pending()` runs
   `kdevice::probe_device_desc` for every descriptor, matching against
   registered `DeviceDriver`s by `bus_type` + `matcher`.
3. **Binding and activation**: on a match, `DeviceDriver::probe_device` is
   called; the driver performs hardware initialization and publishes the
   runtime device into the `kclass` registries.
4. **Unmatched devices**: descriptors with no matching driver land in the
   `unclaimed` list — logged, but never blocking boot.

### 3. The adoption path

Reserved for devices that already run from early init but lack runtime
device-model objects (such as the boot console).
`kdevice::adopt_active_device` registers the running device into the device
tree so later sysfs/device query paths can discover it.

## State Machine

### Device core states

Each `DeviceRecord` in the legacy `kdevice` pipeline moves through the states
below. This is separate from `driver_model::DeviceDomain`, whose unregister
operation deletes the entry; see the R1-01 management section below.

```text
                    ┌──────────┐
                    │Discovered│  ← bus enumeration create
                    └────┬─────┘
                         │ driver matched
                         ▼
                    ┌──────────┐
                    │ Matched  │
                    └────┬─────┘
                         │ bind
                         ▼
                    ┌──────────┐
                    │  Bound   │
                    └────┬─────┘
                         │ activate (probe success)
                         ▼
                    ┌──────────┐
                    │  Active  │  ← runtime device published to kclass
                    └────┬─────┘
                         │ remove
                         ▼
                    ┌──────────┐
                    │ Removing │
                    └────┬─────┘
                         │ cleanup done
                         ▼
                    ┌──────────┐
                    │ Removed  │
                    └──────────┘
```

| From | To | Trigger |
|----|----|----------|
| — | Discovered | bus backend `enumerate` calls `register_device` |
| Discovered | Matched | `probe_device_desc` finds a matching driver |
| Matched | Bound | binding succeeds; device-driver relation established |
| Bound | Active | `DeviceDriver::probe_device` returns `Ok` |
| Matched/Bound/Active | Removing | `remove_device_managed` is called |
| Removing | Removed | devres cleanup completes and the driver remove callback ran |

Unmatched devices stay `Discovered` but enter the `unclaimed` list and
never get a `DeviceObject`.

### Bus backend lifecycle

```text
BusManager::register(backend)
  │
  ▼
Registered ──► early_init() ──► enumerate() ──► probe_pending()
                   │                 │
                   │                 ├─ Activated (→ kclass publish)
                   │                 └─ Unclaimed (log + skip)
                   │
              rescan() ◄── hotplug / rescan
                   │
              quiesce() ──► suspend events (shutdown / suspend)
                   │
              remove() ──► bus teardown (orderly shutdown)
```

## Flows

### PCI bus enumeration

1. Pick ECAM or MmioCam access per `pci_cam_kind()`.
2. Open `PciBus` and obtain configuration-space access.
3. Adopt the host bridge: create a `pci-host` device on the platform bus as
   the root of the PCI device tree.
4. **Pass 1**: walk all BDFs on bus 0..bus_end, dispatching on `HeaderType`:
   - `Standard` → endpoint list;
   - `PciPciBridge` → bridge list, reading secondary/subordinate bus
     numbers;
   - others → skipped with a log entry.
5. **Pass 2**: call `adopt_active_device` per PCI-to-PCI bridge to create
   the `pci-bridge` device object, plus the corresponding secondary
   `BusInstance`.
6. **Pass 3**: walk the endpoint list; for each device:
   - call `configure_pci_device_if_needed` to assign unassigned BARs and
     enable the command register;
   - read BAR info and build the `ResourceSet` (MMIO / IO port / legacy
     INTx IRQ);
   - detect VirtIO-over-PCI devices and attach `TransportInfo::Virtio`;
   - register through `EnumerationContext::register_device_with_parent`,
     with the parent pointing at the host bridge or parent bridge.

### Platform bus enumeration

1. **Firmware stage**: walk firmware-described device nodes (DT compatible
   / ACPI).
   - For each node, query the registered `FirmwareMatchSpec`s (AHCI,
     sdmmc, fxmac, ...) for compatible matching.
   - VirtIO MMIO is special-cased: the `virtio,mmio` compatible is
     detected and the MMIO region mapped to probe the VirtIO device type.
   - Build the `ResourceSet` (MMIO + IRQ) and register through the
     `EnumerationContext`.
2. **UART/serial nodes**: UART nodes in the DT (including the stdout
   console) are enumerated here. The stdout UART is reused by the serial
   driver through `take_early_port` (same hardware, no re-map); other
   UARTs map their own ports and publish as independent char devices.
3. **Static device stage**: register compile-time-known platform devices
   (ramdisk is always registered; AHCI/sdmmc/bcm2835-sdhci only when no
   firmware description exists).

   > The ramdisk storage backend is controlled by `KFEAT_DRIVER_RAMDISK_STATIC`:
   > when off it is 16 MiB of zeroed heap memory (driver validation only); when
   > on it is zero-copy backed by a filesystem image embedded at build time (path
   > from the Makefile variable `RAMDISK_IMG`, format from `RAMDISK_IMG_FS`,
   > default ext4, an empty image generated by `make ramdisk_img`), so it can be
   > mounted as a real read-write root filesystem. See
   > `drivers/contracts/block/src/ramdisk_image.rs`.

### Driver matching and activation

1. `EnumerationContext::probe_pending` takes all pending descriptors.
2. For each descriptor it calls `kdevice::probe_device_desc`:
   - filter drivers by `bus_type`;
   - call `DeviceDriver::matcher().matches(identity)` to test the match;
   - on a match, run bind → activate.
3. Successfully activated devices have their runtime objects published to
   the matching `kclass` registries.
4. Unmatched (`Unclaimed`) or requeue-requested (`Requeue`) descriptors
   enter the `unclaimed` list.

### VirtIO device activation

VirtIO drivers share one activation entry across the PCI and MMIO
transports:

1. **PCI path**: extract the BDF from `DeviceLocation::Pci`, reopen
   `PciBus`, and run `probe_pci_device`, confirming the transport's
   reported `DeviceKind` matches the driver's declaration.
2. **MMIO path**: extract the physical address and size from
   `DeviceLocation::Mmio`, `iomap_mmio`, then run `probe_mmio_device`.
3. **Dispatch**: `dispatch_virtio_try_new` dispatches on `DeviceKind`:
   - `DeviceKind::Net` → `VirtIoNet::try_new` → `kclass::publish_net`
   - `DeviceKind::Block` → `block::activate` → IRQ setup → `kclass::publish_block`
   - `DeviceKind::Display` → `VirtIoGpu::try_new` → `kclass::publish_display`
   - `DeviceKind::Input` → local `virtio/input.rs` activation → devres IRQ registration → PCI INTx enable when applicable → `kclass::publish_input`
   - `DeviceKind::Vsock` → `VirtIoSocket::try_new` → `kclass::publish_vsock`
   - `DeviceKind::Fs9p` → `VirtIo9p::try_new` → `kclass::publish_virtio_9p`

### Resource management (devres)

`driver-os-api-xkernel` provides x-kernel's implementation type for the
`driver-os-api` provider contract; the driver-facing resource API is exposed
to drivers by `kdriver::resource` as `DeviceResourceExt`.
`kdriver::resource` holds the static `XKernelResourceProvider` and passes
it explicitly to `kdevice::device::managed::devm_*_with_provider()`; on release the RAII
handles return to the same provider that created them:

- **`device.devm_iomap`**: maps MMIO through `memspace::iomap_device`;
  automatically `iounmap`s on probe failure or device remove.
- **`device.devm_request_irq`**: adapts the devres IRQ handler into a
  `kirq` shared action registered on the `kirq` IRQ core action list.
  VirtIO input uses this path before class publication; a missing IRQ or failed
  registration aborts probe rather than publishing a polling-only device.
  The PCI path keeps INTx masked during queue construction and handler
  registration, adds a later cleanup that masks the source before IRQ release,
  and only then unmasks INTx and publishes the class device.
  `virtio/input.rs` owns this lifecycle and uses the framework's
  `DeviceResourceExt::devm_request_irq` helper. `glue.rs` only adapts the
  VirtIO HAL and other class constructors; it no longer selects an input IRQ
  provider or binds input devres.
- **`device.devm_request_threaded_irq` /
  `device.devm_request_threaded_irq_default`**: bind devres primary/thread
  handlers to the provider contract. The current mainline
  `driver-os-api-xkernel` provider does not yet cover the threaded IRQ
  methods, so they return `DriverError::Unsupported`; a later kirq
  threadirq branch will supply the xkernel implementation.
  `driver-os-api-xkernel` is the adaptation layer between devres IRQ
  resources and the kernel IRQ core, translating `driver_os_api`
  trigger/controller/event/handler values into `kirq`'s own types; `kirq`
  does not depend back on devres.
  Future IRQ core capability extensions belong in `kirq`; the driver
  framework exposes kernel IRQ capability to drivers only through this
  adapter.
- **`device.devm_alloc_coherent`**: allocates a coherent DMA buffer via
  `kdma::allocate_dma_memory`; release calls
  `kdma::deallocate_dma_memory`.
- **`resource_provider().monotonic_time`**: only X-Kernel glue passes
  `driver_os_api::TimeOp` into reusable drivers; concrete drivers must not
  call `khal::time` directly.

Release runs in reverse acquisition order (LIFO) so resource dependencies
are never inverted.

### IRQ dispatch mechanism

`driver-os-api-xkernel::XKernelResourceProvider` keeps no local IRQ line
state. It converts `driver_os_api::IrqResource` into `kirq::IrqSpec`, wraps
`driver_os_api::IrqHandler` as `kirq::IrqHandler`, and registers with the IRQ
core per API:

1. `request_irq` calls `kirq::try_register_shared()`; `kirq` allocates a
   line-local `IrqActionToken` and stores the action identity; the provider
   returns `driver_os_api::IrqHandlerToken::SharedAction(id)` to devres.
2. `request_threaded_irq` / `request_threaded_irq_default` currently reuse
   the `driver_os_api::IrqOp` default implementations, returning
   `Unsupported` until a kirq threadirq provider override lands.
3. When an interrupt arrives, `kirq` snapshots the actions from
   `IrqDescRuntimeState`, invokes each handler in order, and passes the
   resolved `virq` as the handler argument. Each wrapper then converts the
   devres handler's `driver_os_api::IrqEvent` into `kirq::IrqEvent`.
4. After the whole line's fanout, `kirq` aggregates the handled result.
   VirtIO input reports `NOT_HANDLED` for an empty device ISR and calls the
   input layer's notifier only for an event-queue interrupt. The input layer
   drains events in deferred work and wakes each evdev client's waiters when
   its queue receives an event; it does not register a physical IRQ waiter.
5. `release_irq` calls `kirq::free_irq_action()` for shared hardirq
   tokens, removing only the action belonging to this devres handler.

Registration and release paths may allocate; the IRQ dispatch path does
not allocate from the heap.

## Concurrency Model

- `DeviceManager::bus_mgr` uses `SpinNoPreempt`: bus enumeration, rescan,
  quiesce, and remove run in process context with mutual exclusion.
- `EnumerationContext` has no interior locking: it is a single-threaded
  bridge between bus backends and probe, filled only inside the `bus_mgr`
  lock.
- IRQ action fanout, threaded wake/ONESHOT, token teardown, and
  `in_flight` synchronization are owned by `kirq`; `driver-os-api-xkernel`
  only stores the mapping from devres tokens to `kirq` release calls, and
  `kdriver` keeps no IRQ core state.
- `PCI_BAR_ALLOCATOR` uses `SpinNoPreempt`: BAR allocation happens only in
  process context (enumeration or probe).
- The `kdevice` shared core's internal locks are managed by the `kdevice`
  crate itself; `kdriver` never holds them directly.

## Design Decisions

### Descriptor-first instead of creating device objects directly

**Choice**: bus enumeration only produces `DeviceDesc` descriptors; it does
not directly create `DeviceObject`s.

**Trade-off**: one extra intermediate representation and a batched probe
step, in exchange for:

- unmatched descriptors never pay `DeviceObject` memory or devres costs;
- all descriptors are collected before probing, making unmatched-device
  logging and diagnosis a single summary;
- descriptor deduplication (e.g. ACPI and DT describing the same device)
  can be implemented before probe.

**Rejected alternative**: the Linux `device_register` model — register at
enumeration. It creates device objects during discovery, simplifying the
code path, but couples registration to matching in time (drivers must be
loaded before device registration). With all x-kernel drivers built in,
descriptor-first is simpler.

### Bus backends as a trait instead of compile-time branches

**Choice**: PCI and platform buses are separate implementations of the
`BusBackend` trait, registered in `default_bus_manager()`.

**Trade-off**: the minor dynamic-dispatch cost of `Box<dyn BusBackend>`, in
exchange for:

- one kernel image supports PCI and platform devices simultaneously — no
  compile-time either/or;
- a new bus type (USB, I2C, ...) is one `BusBackend` implementation plus
  registration, with no `BusManager` core changes;
- backend lifecycle management (init/enumerate/rescan/quiesce/remove) is
  unified in `BusManager`, removing per-backend scheduling boilerplate.

**Rejected alternative**: `cfg` branches hard-coding one bus type inside
`BusManager`. No dispatch overhead, but no multi-bus support, and every new
bus edits `BusManager` internals.

### PCI BARs configured once at enumeration

**Choice**: the PCI backend completes BAR allocation and command-register
configuration during enumeration.

**Trade-off**: slightly longer enumeration (the endpoint list is walked
twice — collect, then configure), in exchange for a pure-read activation
path:

- `configure_pci_device_if_needed` degrades to a no-op at activation since
  BARs are already non-zero;
- the activation path never re-derives the firmware MMIO window
  (`pci_bar_allocation_range`);
- a configuration failure skips the device at enumeration, so no
  unconfigured `DeviceDesc` is ever created.

**Rejected alternative**: deferring configuration — allocate BARs at
driver probe. It avoids useless work for unprobed devices but complicates
probe (which would hold the PCI bus lock and BAR allocator lock) and
requires rolling back a registered `DeviceObject` mid-probe on failure.

### VirtIO PCI/MMIO dual descriptors

**Choice**: each VirtIO device type yields two `DeviceDriver` descriptors
(PCI and MMIO), registered on their respective bus types.

**Trade-off**: twice the descriptor count (6 device types × 2 = 12), in
exchange for:

- the matcher (`VirtioTypeMatcher`) works on the VirtIO type code rather
  than PCI vendor/device ids or DT compatibles, so devices discovered via
  PCI and MMIO both match the same functional driver;
- the PCI vs MMIO activation differences are sealed inside
  `activate_virtio_pci` / `activate_virtio_mmio`; the upper
  `dispatch_virtio_try_new` is fully transport-agnostic.

**Rejected alternative**: one descriptor with runtime transport detection.
Fewer descriptors, but the matcher would compare bus type and VirtIO type
together, and `probe_device` would branch between two transport init
logics.

### IRQ handlers reach kirq through driver-os-api-xkernel

**Choice**: `driver-os-api-xkernel` is the `driver_os_api` provider adapting
driver devres IRQ requests onto `kirq`; `kirq` keeps the shared action
list per IRQ line and manages devres handler lifetimes via tokens.

**Trade-off**: shared IRQ fanout moved from `kdriver` down into `kirq`,
making IRQ core action state more complex; but:

- shared, oneshot, threaded IRQ, per-action stats, and free/synchronize
  semantics all concentrate in the IRQ core;
- device handlers carry their own context — no per-slot trampolines and no
  global IRQ count limit;
- tokens allow freeing one shared handler while other devices on the line
  keep receiving interrupts;
- fixed-length action snapshots keep the dispatch path allocation-free;
- interrupt release removes a device handler by token; after the last
  handler goes, `kirq` masks and tears down the line.

**Rejected alternative**: a preallocated static `IrqSlot` array with
generated trampolines. That would need slot identity bookkeeping and a
hard-coded total IRQ limit.

## Drop / Resource Release

- devres resources are released in LIFO order in the `DeviceObject` remove
  path.
- `PciBackend` holds no persistent resources needing drop-time release
  (`PciBus` is released when `enumerate` returns).
- `PlatformBackend` holds only a `LocalIdAlloc` (a stack `u16`), with no
  explicit release.
- Interrupt release removes the device handler by token; once the last
  handler goes, the `kirq` handler is deregistered.
- A VirtIO input action is registered before class publication. PCI input adds
  its INTx-mask cleanup after the action, so LIFO device removal masks the
  source first, then releases and synchronizes the action before the detached
  class object and its shared transport state are finally dropped.
- Shared DMA buffers are reclaimed by `kdma::deallocate_dma_memory` after
  the last handle drops.

## Feature Gating

```text
virtio ────────► virtio-blk ──► block  + virtio + virtio/block
                virtio-net ──► net    + virtio + virtio/net
                virtio-gpu ──► display + virtio + virtio/gpu
                virtio-input ► input  + virtio + virtio/input
                virtio-socket► vsock  + virtio + virtio/socket
                virtio-9p ───► virtio + virtio/virtio-9p + kclass/virtio-9p

console ───────► console-pl011 / console-ns16550-mmio / console-ns16550-ioport
ramdisk ───────► block + block/ramdisk
ahci ──────────► any_firmware_driver + block
bcm2835-sdhci ─► any_firmware_driver + block
sdmmc ─────────► any_firmware_driver + block
fxmac ─────────► any_firmware_driver + net
ixgbe ─────────► net (placeholder)
```

`any_firmware_driver` is an umbrella feature: true whenever any DT/ACPI
platform driver is enabled; `cfg(feature = "any_firmware_driver")`
replaces the verbose `any(ahci, bcm2835-sdhci, sdmmc, fxmac)` condition.

Platform location IDs use a fallible monotonic allocator. Exhaustion returns
`DriverError::NoMemory`; firmware discovery records it in its existing first-error
result, and static discovery propagates it before registering another device.

## Device management lock adapter (R1-01)

`host/management.rs` implements the portable `driver_model::DeviceManagementLock`
capability with one private `ksync::Mutex<()>` per factory call.
The optional `device-model` feature enables only `driver_model`, `ktask`, and
`ksync`; `hwrng` selects it. The factory validates sleepability before allocating.
Execution validates current context first, checks the safe mutex owner query for
same-task recursion, then invokes the borrowed callback once under an RAII guard.
The guard is released before success is returned; independent capabilities have
independent serialization domains.

The model now stores this capability in each `DeviceCore`, alongside an `Arc`
domain identity. `add_device` calls `DeviceServices::create_management_lock`
before publication and inserts the device inside `run_locked`. Each entry keeps
the host services bundle selected at registration; replacing domain defaults
does not redirect existing devices' probe or teardown services. The lifecycle
test host implements that factory with this adapter and installs it on a real
`DeviceDomain`.
Production service installation remains a composition-root concern; these tests
do not migrate the legacy discovery pipeline or attach real RNG hardware.

Model bind, unbind, unregister, and typed Data visits use the per-device lock.
Unbind retains registration in `Unclaimed`; bind retries that registration.
Unregister deletes only the specified entry, and repeated same-domain removal
succeeds after management-entry checks; foreign references are rejected.
Matching and Data visitors execute outside the domain spinlock. Probe dispatch
has no global pump flag, so different devices can execute concurrently; host
services must not assume domain-wide serialization.

External tests live in `ut-tests/device_management_unittest.rs`, mounted with
`path_mod_test!`. Basic tests run with `device-model`; `hwrng` additionally enables
the Completion contention test and the optional `krcu` dependency for RCU context
coverage. Contention uses a Completion start handshake, an observed blocked
contender, and a Completion finish handshake. Five-second monotonic deadlines
only act as failure watchdogs, not success performance thresholds; no sleep
duration establishes ordering. A separate two-task test completes work under
an independent lock while the first lock remains owned. The owner
waits only for the contender to block, never for lock-dependent work to finish.
Atomic flags use release/acquire for observations across tasks. Before assertions,
tests wait for task exit with a watchdog and join exited tasks, including when
earlier observations failed. No unbounded join is attempted if a regression
leaves a task stuck; that failure requires the enclosing test-run watchdog.

`ut-tests/device_management_lifecycle_unittest.rs` exercises the model lifecycle
with this installed host under `hwrng`. Its resource services return `Unsupported`,
and its extension/provider surfaces are empty; no MMIO, IRQ registration, or DMA
is required. A rejected factory call leaves no bus membership or pending probe.
IRQ-off, preempt-disabled, and RCU read contexts reject bind/unbind/remove without
changing callbacks, private-data lifetime, source online status, or registry state.
The fixture also checks successful unbind to `Unclaimed`, rebind, idempotent
unregister with an absent snapshot, and reentry rejection from a remove callback.

Two-task tests hold the first remove callback at a Completion-controlled gate.
A same-device contender must block until release and produce exactly one remove
and private-data drop; a different-device contender must complete while that
gate remains closed. Release happens before joins and assertions. Hook references
are cleared before reporting results, and callbacks never run under the fixture's
hook-storage mutex. Release/acquire observations and Completion establish ordering;
monotonic deadlines are failure watchdogs, not performance requirements.
The model performs no automatic parent/bridge recursion or retryable cascade.
The R1-02 same-device bind regression pauses registration inside probe, starts
an explicit binder on a second task, and observes that task blocked before
releasing probe. The binder then reports zero new activations: one probe, one
binding, and one subsequent remove/Data drop. This complements the model's
deterministic dequeued-candidate tests with the real sleepable host lock.
Buses own enumeration shutdown, downstream teardown and cross-device lock order.
`ut-tests/device_management_downstream_unittest.rs` supplies a test-only controller
whose binding owns an enumeration gate and a set of committed children. Shutdown
closes the gate and takes that set, then releases the gate before child removal;
the separate parent transaction/service lock remains available until all owned
children are removed. A same-parent device outside the owned set survives.
The concurrent case pauses a child probe, observes controller shutdown blocked
on the enumeration gate, then releases the probe and checks that its successful
commit is drained before parent stop and private-data drop. After parent rebind,
the old binding's gate still rejects late commits while the new binding accepts
children. Weak domain links avoid fixture ownership cycles. These cases validate
the ownership protocol with the real host adapter, not a production bus or hardware.
Legacy class integration is not migrated, and
Unbind keeps the device registered and online; publication drain remains the
subsystem's explicit responsibility, not an effect of device identity changes.

## Hardware entropy completion provider

`rng_completion.rs` is the X-Kernel implementation of the driver-facing
`RngCompletion` capability. `wait_until` prepares the current task to sleep,
registers a scoped stack node with `kcompletion::WaitQueue`, checks the driver's readiness
predicate, and parks if the request is still pending.
`notify` wakes registered tasks so they can check readiness again. The
provider owns wait registrations; the concrete driver owns its request and DMA
buffer. Entropy owns one reusable collection work across source changes,
issuing reads serially and retaining demand when a provider disappears.

The `hwrng` feature compiles this provider for injection into concrete drivers.

R1-03 adds `new_rng_drain`, a distinct reliable `RngDrain` with its own WaitQueue.
Construction rejects non-sleepable context before publication. Unregister calls
CompletionWait::prepare before invoking the subsystem predicate; after that point
it waits without a fallible registration, timeout or signal exit. Final operation
release notifies outside admission locks. No kpoll/block_on path is introduced.
The legacy VirtIO RNG boot publisher constructs the drain before creating the
device, then intentionally drops the credential: the subsystem registry retains
the runtime for kernel lifetime. This does not implement hardware removal.
The legacy RNG factory validates sleepable context before initialization and
supplies `ktask::yield_now` as the reset-poll interval for IRQ-registration
rollback. No scheduler dependency is added to the concrete driver. The factory
returns the concrete Arc so the publisher can explicitly close/finish the device
if registration fails, rather than losing the teardown interface through early
trait-object erasure. Successful legacy publication remains boot-only; it does
not become a new-model binding or acquire an unbind/remove callback.
External tests cover atomic/RCU rejection, early notification, detached nodes,
and an in-flight read that must finish before unregister returns or emits removed.

## R1-04 binding publication acceptance

R1-07 adds `virtio_rng_lifecycle_unittest.rs` beside the earlier model-only
fixture. A test DriverOps binds the actual VirtIoRngDev and publishes through
the real registry with DeviceAccess's binding source. Real host completion and
drain providers are injected. A spawned consumer blocks on a pending request
while probe holds management serialization; remove or an injected error after
publication closes/wakes that reader, unregisters/drains, then invokes the
actual reset/IRQ/cancellation helper before Data is dropped. The test checks
that old handles preserve provenance but cannot read, and that queue/request
allocations are released while those handles still exist. Releasing the final
consumer in IRQ-disabled context causes no additional IRQ release.

Only the transport, identity-mapped DMA HAL and synchronous IRQ provider are
controlled test implementations. This is an integration test of model callbacks
with the real driver, not a production MMIO probe or proof of real IRQ-core drain.
Production firmware discovery, instance resource adaptation and remove/rebind
wiring belong to R1-08 and are not supplied by this fixture.

`ut-tests/hwrng_binding_unittest.rs` connects a model test driver to a private
hwrng registry using the real management lock and drain provider. Data owns
the unique registration, a strong runtime reference and a simulated resource
guard. Failure at resource acquisition, before publication and after publication
is checked independently; rollback stops the runtime, unregisters when published,
then releases resources before returning an error. The framework does not call
remove for these failed probes. Successful binding removal follows the same order.

Tests cover A-to-B replacement, same-driver rebind, sibling independence,
illegal-context rejection before remove, and metadata/old handles surviving
driver ops destruction. Final consumer drops in IRQ-disabled context perform
no repeated simulated resource cleanup. This is not real IRQ/DMA teardown proof.
The model's existing synthetic generation-mismatch test separately verifies
remove-before-Data-drop for a successful but rejected probe; no public injection
API is added merely to reach that serialized defensive path from kdriver.

## Per-device coherent DMA adapter

`host_dma::alloc_coherent` is a host service implementation helper, not a new
driver-facing contract or allocator. It consumes the model's borrowed
`DeviceContext`, reads `descriptor().dma` on each call and allocates through
kdma. It does not cache descriptor facts, infer firmware defaults, or use the
legacy global bus address as the device's physical address. Translation starts
with `khal::mem::v2p` of the allocated CPU address.

The complete page-rounded allocation must fit the resolved window and device
mask; the translated bus base must satisfy requested alignment. Rejection drops
an unpublished native owner, returning the original allocation to kdma's cleanup
path. Successful buffers, including padding, are zeroed before publication.
The non-Clone `CoherentMem` guard receives the original CPU/native-bus/layout
release token in a private hook; its device-visible bus address is independent
of that token. Ownership transfers once, without another global allocation map.
The private native owner carries its release operation until transfer. Tests
substitute only this operation to observe the original CPU/bus/layout token and
assert one release on rejection or guard Drop; production uses kdma directly.
There is no global fault-injection state or public alternate allocator interface.

Absent configuration and non-coherent devices return `Unsupported`. This slice
does not claim a cache-maintenance protocol for non-coherent devices, streaming
DMA, IOMMU support, or constrained allocation search. If kdma's selected region
cannot satisfy the device view, allocation returns `NoMemory` after cleanup.
Metadata allocation retains the kernel's existing infallible Arc policy.

Guest tests exercise actual page allocation/mapping/free, independent translated
views, zeroed padding, invalid inputs and full-range rejection. A model callback
fixture delegates its DMA service to this helper and checks two descriptor views
across bind/unbind/rebind. It does not hand the addresses to real hardware.
The scoped production root installs this provider and routes eligible MMIO RNGs
to the model; these helper tests alone do not establish hardware acceptance.

### Explicit QEMU Arm virt DMA policy

The production owner supplies `QEMU_MODERN_MMIO_DMA_MASK` (64 address bits),
not a mask inferred from the RAM interval. Modern queue register pairs and
buffer descriptors retain all 64 bits; the pinned QEMU source uses its system
memory address space for this direct-MMIO RNG. The evidence and scope caveats
are in `docs/ai-collaboration/driver-model/qemu-mmio-dma-mask.md` at repository
root. This is not an unrestricted identity window or a generic VirtIO default.

`from_firmware_with_qemu_virt_mmio_policy` is a separate enumeration operation,
not a default in `from_firmware`. Only MissingWindow permits fallback: an
available direct root child with actual `virtio,mmio` compatibility under
`linux,dummy-virt`, on aarch64, can use the same tree's single root RAM interval
as a bounded identity window. Explicit windows and all other errors win.
The caller supplies the device mask; firmware must still specify coherency.
The native allocator remains responsible for free/reserved-memory ownership.

This policy follows QEMU Arm virt's direct virtio-mmio system-memory DMA path;
it is not a general DT rule for missing dma-ranges. No support is inferred for
nested buses, other boards, multiple RAM banks or an IOMMU path. Boot discovery
tests exercise the policy without demonstrating real RNG DMA. The production
enumerator now applies it before registering a scoped model descriptor.

### Model host service composition

`host::new_device_services` composes the native MMIO/DMA helpers and
management-lock factory into the existing DeviceServices contract. Construction
does not change the global domain or enumerate hardware. The caller supplies an
owned OS-api IrqOp capability; the host aggregate keeps no copied device facts.
The domain must receive these services before registering its devices.

Firmware IRQ routes are resolved by the supplied provider, never treated as
virtual IRQ numbers. Already resolved virtual IRQs bypass mapping. A successful
request creates one DevIrq guard capturing the exact returned resource, action
token and provider. Drop releases that registration without reconstructing its
route or consulting a replaceable global service. The provider owns mapping
lifetime and enable/mask semantics; the adapter does not mask shared lines.
The current IrqOp contract has no diagnostic name parameter, so model request
names are not forwarded. No new IRQ registration semantics are introduced here.

Clock acquisition and provider publication explicitly return Unsupported; each
service aggregate owns an independent extension registry. Existing lifecycle UT
services delegate to this composition, retaining only factory-call counting and
an unavailable IRQ test provider. New controlled-provider tests cover translated
and resolved IRQs, failed mapping/request/probe, and unbind/rebind release counts.
They validate adaptation, not real IRQ detach/drain or hardware quiescence.
The scoped production root now supplies provider installation and exclusive
new/legacy enumeration routing. Real hardware lifecycle and IRQ acceptance
remain separate from these controlled-provider tests.
