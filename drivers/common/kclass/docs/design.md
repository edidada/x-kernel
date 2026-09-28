# kclass — Design

## Purpose

`kclass` is x-kernel's typed runtime device class layer. Built on top of
the `driver-core` device model, it provides type-safe publication, enumeration,
lookup, and availability-subscription interfaces for each device category
(net / block / char / hwrng / display / input / vsock / 9p).

After a successful probe, a driver publishes its runtime capability into
the matching class registry through `publish_<class>()`; subsystems (such
as `knet`, `fs_boot`, or the input subsystem) discover and use runtime
devices through `*_devices()` / `find_*_device()` /
`subscribe_*_available()`, without depending on probe order.

The intended audience is developers implementing device class adapters,
modifying class registry semantics, or adding new device categories.

## Out of Scope

- No framebuffer management: `/dev/fb0` emulation lives in `fbdevice`;
  kclass only delegates `DisplayDevice` calls (see the framebuffer path
  below).
- No IRQ handling: interrupt registration and dispatch belong to `kirq`
  and `device-res-xkernel`; kclass stores only the `irq` metadata value.
- No driver binding or probing: the discover → match → bind pipeline is
  owned by the `driver-core` domain; kclass consumes the already-bound
  device object at publish time.
- No device lifecycle or node-allocation ownership: hotplug state transitions,
  class indices, major/minor numbers, and `devname` templates belong to
  `driver-core`. kclass only supplies class-specific metadata at activation.
- No runtime dispatch policy between consumers: after publication the
  consumer holds `ClassDevice<T>` directly and kclass steps out of the
  call path.

## Background

The Linux kernel class mechanism (`/sys/class/`) provides a view of
devices organized by functional category, orthogonal to bus topology
(`/sys/bus/`). `kclass` fills the same role in x-kernel:

- **driver-core** owns bus attachment, lifecycle state, and driver binding;
- **kclass** adds the typed per-category view
  (net/block/char/display/input/vsock/9p) on top;
- Drivers call `publish_*()` after a successful probe to inject the
  runtime capability into the kclass registries.

This layering lets upper subsystems (network stack, filesystem) stay
agnostic of both the bus (PCI vs platform) and the probe order — they only
query the class registry for currently available devices or subscribe to
future availability notifications.

## Scope

```text
drivers/common/kclass/
├── Cargo.toml
├── docs/
│   ├── design.md
│   └── security.md
└── src/
    ├── lib.rs       # macro-driven class registries + device trait delegation + prelude
    └── generic.rs   # ClassDevice<T>, ClassRegistry<T> generic primitives
```

## Architecture

```text
                driver-compose (probe success)
                     │
                     │ publish_net / publish_block / publish_display / ...
                     v
    ┌────────────────────────────────────────┐
    │ kclass                                  │
    │                                         │
    │  ┌─────────────────────────────────┐    │
    │  │     ClassRegistry (per class)   │    │
    │  │  (class-level subscription)     │    │
    │  │  Activated → notify_class_avail │    │
    │  │              + register node     │    │
    │  │  Removed   → remove_class_device│    │
    │  │              + unregister node   │    │
    │  └─────────────┬───────────────────┘    │
    │                │                        │
    │  ┌─────────────┴───────────────────┐    │
    │  │  Per-class registries           │    │
    │  │  (macro-generated)              │    │
    │  │                                 │    │
    │  │  NET_DEVICES    (SpinNoPreempt) │    │
    │  │  BLOCK_DEVICES  (SpinNoPreempt) │    │
    │  │  CHAR_DEVICES   (SpinNoPreempt) │    │
    │  │  DISPLAY_DEVICES(SpinNoPreempt) │    │
    │  │  INPUT_DEVICES  (SpinNoPreempt) │    │
    │  │  VSOCK_DEVICES  (SpinNoPreempt) │    │
    │  │  VIRTIO_9P_DEVICES(SpinNoPreempt)│   │
    │  └─────────────┬───────────────────┘    │
    │                │                        │
    │  ┌─────────────┴───────────────────┐    │
    │  │  ClassDevice<T>                 │    │
    │  │   Arc<ClassDeviceInner<T>>      │    │
    │  │   ├─ parent: ClassParent        │    │
    │  │   ├─ runtime: T (Box<dyn Trait>)│    │
    │  │   ├─ name, device_kind, irq     │    │
    │  │   └─ metadata (input identity)  │    │
    │  └─────────────────────────────────┘    │
    │                                         │
    │  Delegation impls:                      │
    │   ClassDevice<NI> : NetDevice           │
    │   ClassDevice<CI> : CharDevice          │
    │   ClassDevice<DI> : DisplayDevice       │
    │   ClassDevice<II> : InputDevice         │
    │   ClassDevice<VI> : VsockDevice         │
    │   ClassDevice<9I> : Virtio9pDevice      │
    └──────────────────┬─────────────────────┘
                       │
                       │ query / subscribe
                       v
         knet / fs_boot / input subsystem / ...
```

| Component | Responsibility |
|------|------|
| `ClassDevice<T>` | Typed runtime device handle; wraps a detached `ClassParent` + trait object and delegates trait method calls |
| `ClassDeviceInner<T>` | `ClassDevice` internal shared state: detached parent (id, driver name, location), runtime, name, kind, irq, metadata |
| `ClassRegistry<T>` | Typed registry: publish (unique publication), devices (enumerate available), find (lookup by id), subscribe (availability notification), remove (remove by id) |
| `class_notify` / `class_remove` | Shared functions every class uses for availability notification (with devnode projection for input/display) and removal (unsubscribe + unpublish) |
| `ClassRuntimeMetadata::device_node_class` | Supplies input capability metadata or the DRM primary class to the devnode projection; other runtime classes return no node request |
| `class_api!` macro | Instantiates each class's typed registry static and the stable-name forwarding functions; the notify/remove logic is shared hand-written code, not macro-generated |
| `ClassDeviceMetadata` | Optional class-specific metadata (currently only the input class carries physical_location / unique_id) |
| `prelude` module | Centralized re-export of all public types for publishers such as `drv/*` modules |
| Trait delegation impls | Implement the operation traits (e.g. `NetDevice`) for categories consumed through class handles, delegating to the inner runtime; the net class also forwards `NetRxScheduler` attach/detach, while block I/O goes only through the block core canonical `BlockDevice` |

## State Machine

### ClassDevice lifecycle

```text
                    driver probe()
                         │
                         │ publish_<class>(parent, runtime)
                         v
                    ┌──────────┐
                    │Published │  ← ClassDevice created, held by registry;
                    │(pending) │     parent.state != Active
                    └────┬─────┘
                         │ class availability
                         ▼
                    ┌──────────┐
                    │Available │  ← parent.state == Active;
                    │(active)  │     DeviceNode assigned, then callbacks fired
                    └────┬─────┘
                         │ class removal
                         ▼
                    ┌──────────┐
                    │ Removed  │  ← swap_remove from the registry
                    └──────────┘
```

| From | To | Trigger |
|----|----|----------|
| — | Published | Driver calls `publish_<class>(parent, runtime)` |
| Published | Available | `devnode` dispatches `Activated`; `notify_class_available` registers any class node and then invokes subscriber callbacks |
| Published | Available (immediate) | `parent.state()` is already `Active` at publish time (desc-adoption path) |
| Available | Removed | `devnode` dispatches the `Removed` event; `remove_class_device` removes the entry |
| Published | — | Publishing the same `DeviceId` again returns `AlreadyExists` |

### Registry publish semantics

`ClassRegistry::publish` rejects duplicate identity, as Linux device
registration does. Hotplug re-registration must first remove the old
object through the `Removed` lifecycle and then publish a new object; a
resident runtime is never silently replaced.

## Flows

### Publish flow

1. The driver creates the runtime device inside `DriverOps::probe`
   (e.g. `VirtIoNet::try_new`).
2. It calls `publish_<class>(parent, runtime)` (macro-generated).
3. `name()`, `device_kind()`, and `irq()` are extracted from the runtime.
4. The `device_kind` is checked against the registry type;
   a mismatch returns `InvalidInput`.
5. `class_metadata()` is extracted from the runtime (only the input class
   returns meaningful metadata).
6. `ClassDevice::try_new_with_class_metadata` is constructed:
   - the parent must have `driver_name()` and `driver_id()` (i.e. a bound
     driver), otherwise `BadState` is returned.
7. The registry publishes under the `SpinNoPreempt` lock:
   - if the same id already exists → `AlreadyExists`, and the
     class-specific publish already performed is undone;
   - otherwise → push.
8. If the device is already `Active`, `notify_class_available` runs
   synchronously. Input and display classes first register a `DeviceNode` in
   `devnode`, then notify class subscribers.

### Device enumeration and lookup

1. `*_devices()`: take the registry lock, iterate all entries, filter by
   `is_available()` (parent.state == Active), and return clones.
2. `find_*_device(id)`: take the registry lock and find the entry matching
   the id and `is_available()`.
3. Both operations return clones of `ClassDevice<T>` (`Arc`-shared), so
   callers can safely use them across the lock boundary.

### Availability subscription

1. A subsystem calls `subscribe_<class>_available(callback)` during init.
2. The callback is stored as
   `ClassAvailabilityCallback<T> = Arc<dyn Fn(ClassDevice<T>) + Send + Sync>`.
3. When a device becomes `Active`, `notify_class_available` invokes all
   node and class-subscriber callbacks outside the class-registry lock.
4. A subscriber may use the device immediately or cache the reference
   inside the callback.
5. Callbacks run outside the lock so reentrancy inside subscriber code
   cannot deadlock the registry.

### Removal semantics

There is no legacy event bridge. Removal flows through the composition
root's remove notifier: `driver-compose` calls
`kclass::remove_all_detached(owner)` between `remove()` and `drop(Data)`,
so publications are withdrawn while the hardware is still mapped:

1. `class_remove(registry, id)`: remove from the registry, snapshot the
   removal-subscriber list inside the lock, run the callbacks outside it.
2. Withdraw the devnode publication (`devnode::withdraw`).
3. Run class-specific unpublish cleanup (`ClassRuntimeMetadata::
   unpublish_class` — block deletes the gendisk, etc.).

Device removal may also be driven by a family activation's `close()`
(virtio-mmio): `unpublish_<class>(id)` is the family-facing entry; both
paths are idempotent.

### Device trait delegation

`ClassDevice<T>` implements the operation trait for categories whose
consumers hold class handles, delegating to the inner runtime. The block
class is the lifecycle publication entry point; I/O consumers obtain the
canonical `BlockDevice` from the block core, so the full set of block
operations is deliberately not re-implemented for
`ClassDevice<BlockDeviceImpl>`.
`BlockDeviceImpl` uses `block::GendiskRef` to keep the disk alive through
canonical part0. Class publication and removal only register or release that
owning reference; device references keep the complete disk alive after class
removal.

```rust
impl NetDevice for ClassDevice<NetDeviceImpl> {
    fn can_tx(&self) -> bool {
        self.with(|device| device.can_tx())
    }
    // ...
}
```

The `with()` method borrows the runtime shared through
`&self.inner.runtime` without taking a lock. Concurrency inside the
runtime is the driver's own responsibility (usually interior mutability).

### Display device framebuffer path

The `DisplayDevice` trait exposes only the resolution
(`DisplayInfo { width, height }`) and the scanout resource interface
(`create_scanout_resource` / `destroy_scanout_resource` /
`present_scanout_resource`). The kclass class adapter is a pure
delegation for those methods and holds no framebuffer raw pointers or
direct memory mappings.

The `/dev/fb0` framebuffer compatibility layer lives in the `fbdevice`
crate: at `fb_init` it allocates a shadow buffer against the primary
display device, binds it as a host-visible 2D resource through
`create_scanout_resource`, and pushes it to the scanout on demand via
`fb_present` (no background refresh task: a continuous
`present_scanout_resource` would race a DRM compositor for the single
physical scanout and cause flicker, so only `/dev/fb0` writes and the
`FBIOPAN_DISPLAY` ioctl trigger a present). This "fbdev emulation over
scanout" model applies uniformly to any `DisplayDevice`, so drivers never
need to expose a directly-mapped framebuffer, and kclass needs no
framebuffer special-casing or unsafe boundary.

## Hardware entropy class

The `hwrng` feature adds `HwrngDeviceImpl = Arc<dyn HwrngDevice + Send + Sync>`
and the standard class publication and discovery APIs.
`ClassDevice<HwrngDeviceImpl>` forwards `read` and the terminal `is_failed`
query without holding the registry lock. Only the read operation may sleep. The host must prepare completion delivery
before publication; kclass does not implement waiting or entropy trust policy.

## Concurrency Model

- Each class registry is protected by `SpinNoPreempt<ClassRegistry<T>>`:
  publish / devices / find / subscribe / remove complete inside the lock.
- `notify_class_available` uses the "find device inside the lock, call
  callbacks outside" pattern:
  - find the device and clone the subscriber list under the lock;
  - walk subscribers and run callbacks outside the lock.
  This prevents reentrancy inside subscriber callbacks from deadlocking
  the registry.
- Class registry statics initialize through `LazyInit` (thread-safe, once).
- `ClassDevice<T>` is shared via `Arc`; `Clone` only bumps the reference
  count.
- Runtime (trait object) concurrency safety is guaranteed by the
  `Send + Sync` bounds of `Box<dyn Trait + Send + Sync>` plus each
  driver's interior locking.

## Design Decisions

### Macro-driven instead of hand-written per class

**Choice**: a small `class_api!` macro instantiates each class's typed
registry static and the stable-name forwarding functions; the publish
validation, notify (with devnode projection), and removal bodies are
shared hand-written functions (`class_notify`, `class_remove`), not
macro-expanded.

**Trade-off**: one macro to learn, in exchange for:

- the notify/remove logic exists exactly once and is plainly readable;
- adding a new class is one line at the `class_api!` call site;
- lock scoping and callback ordering are auditable without expanding
  anything.

**Rejected alternative**: the original 12-parameter `class_registry!`
generating notify/remove per class — duplicated logic invisible to IDE
navigation; also hand-writing every class in full (duplication).

### Device trait delegation instead of exposing the trait object

**Choice**: implement each operation trait for `ClassDevice<T>`,
delegating through `with()`.

**Trade-off**: each class needs 5-10 delegating methods, in exchange for:

- upper subsystems use `ClassDevice<NetDeviceImpl>` exactly as they would
  use `&dyn NetDevice`;
- `ClassDevice` can insert common logic around delegated calls (stats,
  logging, permission checks) without touching driver implementations;
- Runtime lifetime management is encapsulated; consumers never
  inspect device state.

**Rejected alternative**: expose `fn runtime(&self) -> &T` and let callers
operate directly. That simplifies delegation code but breaks
encapsulation — callers could bypass device state checks.

### Removal through the composition root

**Choice**: there is no lazily initialized event bridge. Publications are
withdrawn by the composition root's remove notifier
(`kclass::remove_all_detached(owner)`), which `driver-compose` invokes
between `remove()` and `drop(Data)` — while the hardware is still mapped.

**Trade-off**: one explicit call site instead of transparent event
plumbing, in exchange for:

- a single, documented teardown order (quiesce -> withdraw publications ->
  release IRQ -> drop transport -> unmap) that no callback ordering can
  violate;
- no hidden global subscriptions that fire on access paths.

**Rejected alternative**: the earlier `ensure_event_bridge()` design
(subscribing to device-model events on first registry access) — it
coupled kclass to the device model's event vocabulary and hid the
teardown order inside lazy initialization.

## Drop / Resource Release

- Dropping a `ClassDevice` only decrements the `Arc` count; it performs no
  device operations.
- Registry removal is a `Vec::swap_remove` of the `ClassDevice` entry.
- Resource guards (MMIO mappings, IRQs, DMA buffers) are owned by the
  driver's `Data` in the device model, not by kclass; they release when
  the model drops `Data` after publication withdrawal.
- The runtime trait object is dropped by its `Box<T>` when
  `ClassDeviceInner` drops.
