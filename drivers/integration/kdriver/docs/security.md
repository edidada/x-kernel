# kdriver — Security and Reliability Analysis

## Scoped production RNG ownership boundary

The explicitly enabled real-MMIO lifecycle test operates only on a fixture with
one matching model device and one matching hwrng publication, checked before
mutation. A driver name alone is not an instance selector when multiple devices
exist. The test uses the public unbind/bind contracts and retains registration;
it does not reset registers directly, replace services or unregister a device
behind its bus owner. Restoration failures must fail the test and be reported,
not be treated as successful cleanup. Passing ordinary unbind/rebind does not
prove a hardware request was pending or complete the deferred IRQ safety gate.

The separate `model_mmio_pending` opt-in fixture trusts a controlled host EGD
gate to withhold every response byte from boot. It requires the VirtIO entropy
consumer and lifecycle sampling opt-in to be disabled, a kdriver-only suite,
one matching device/publication, and no other real-RNG consumer or management
operation. A serial test annotation alone cannot enforce the kernel-worker
exclusion. Positive EGD requests and zero responses must corroborate the exact
ordered BLOCKED/CLOSED lines, named-test success and suite all-pass result;
neither a marker nor a blocked task alone is sufficient evidence. Independent
socket/console buffering does not establish cross-FD event ordering.

The reader owns its handle, buffer and shared observation state; task-context
results use SpinNoPreempt, not an IRQ-disabled lock. Observations are collected
before assertions, and failed readiness still attempts normal unbind cleanup.
Five-second deadlines are failure watchdogs, never evidence of quiescence.
Join occurs only after observing Exited. Rebinding requires successful unbind
and a zero reader exit code; otherwise the fixture fails without attempting to
reuse the binding. Dropping the observer's task reference neither cancels nor
force-frees a live task: scheduler/waiter ownership and the closure's owned
inputs remain. A hung unbind/reset or non-exiting reader requires the outer
guest timeout, not a forced resource-release shortcut.

CLOSED is emitted only after successful cleanup, restoration and assertions.
Restoration preserves the registered device and performs no new nonempty read;
this fixture therefore does not demonstrate rebound queue data delivery. The
withholding gate does not inject a late response or exercise an IRQ callback
tail race, and no IRQ-tail, cache/DMA hardware safety or whole-ticket acceptance
claim follows. Validation results must be recorded separately after the main
run; this documentation records only the fixture's contract and limitations.

The new path is restricted to fixed boot-DT, available direct QEMU Arm virt
modern MMIO RNG nodes. Availability includes ancestors; real compatible strings
and root provenance come from FirmwareDeviceView, not matcher aliases. Full-tree
node IDs are valid only within that single immutable snapshot. Firmware remains
trusted for board identity and physical ranges: header/numeric checks do not
authenticate hardware or authorize arbitrary MMIO. No overlay, replacement-tree
or general cross-bus resource ownership scheme is supplied here.

EnumerationContext::new uses the LazyInit<Arc<PlatformModel>> production owner;
separate DeviceManager/default bus-manager instances must not create independent
ownership tables or per-manager owner fields. The platform backend's shared
owner.discovery SpinNoPreempt lock spans ownership checks, temporary
classification and reservation or legacy registration. Reserved and registered
model ranges prevent remapping during pending probe, unbound and probe-failed
states. Legacy descriptors also prevent alias classification. A second virtual
mapping cannot bypass a live UniqueMmioPointer's exclusive-region contract.
Independent registration outside this composition root remains unsupported.
Model-selected conversion/registration/probe failure never authorizes legacy
fallback. Classification is pre-transport only, not a safe active-device rescan.

Unsafe RNG driver registration is scoped to one bus instance and must stay true
for future additions and rebinds: exclusive QEMU/no-IOMMU provenance, coherent
identity DMA, stable native providers and non-sleeping DMA callbacks under the
request lock/ordinary IRQ context. Probe checks instance, context, descriptor
and header facts but cannot infer all platform guarantees from the header.
Both ACCESS_PLATFORM modes rely on this scope; arbitrary translated/IOMMU
devices are not admitted by this binding. Later service replacement must not
violate the native-provider assumptions established at root initialization.

Bus-manager, shared discovery and slot locks are absent during device
registration/probe. Root initialization uses a sleepable mutex; device lifecycle
uses retained per-device management locks. Scheduler, allocator, mappings and
IRQ domains must already be ready. Native registration can enable delivery, so
callback-visible runtime state exists before request. Release must detach/drain
the exact retained IRQ token without callback-needed locks. This relies on the
external IRQ contract rather than proving real IRQ-core synchronization.

RngBinding retains IoMap in ManuallyDrop through construction and shutdown.
Cleanup closes reads, unregisters/drains publication, confirms reset, releases
and drains IRQ ownership, then cancels remaining requests and releases DMA before
unmapping. Data Drop also covers successful-but-rejected probes; publication
failure cleans up before returning an error. Old readers retain only an inert
runtime, not a transport capable of MMIO access. Reset polling has no
timeout-based reclamation shortcut. If shutdown cannot establish quiescence,
the infallible remove/Drop path panics and does not unmap: fail-closed, not
successful cleanup or recoverable retry support.

Production wiring does not certify real hardware DMA/cache behavior, physical
IRQ delivery/drain or final remove/rebind acceptance. Controlled fixtures retain
their narrower scope; no complete R1-08 hardware lifecycle or G-IRQ acceptance
is claimed here.

## Non-destructive MMIO discovery

Enumeration reuses descriptors for known MMIO ranges before mapping or reading
registers. This protects the unique-access contract of live transports; raw
volatile reads would not by themselves make aliasing a UniqueMmioPointer legal.
Overlapping descriptions are rejected. With virtio-rng enabled the shared
production owner serializes discovery through reservation/descriptor publication
across managers; this is not a general cross-bus physical-resource allocator.

For unregistered slots enumeration receives copied type/version information, not a temporary
transport with reset-on-drop behavior. Repeat classification cannot write status,
queue or feature registers. Firmware still supplies the trusted MMIO extent;
structural header validation does not authorize arbitrary physical addresses or
make a stale hardware mapping safe. Discovery releases only its own mapping
reference and does not claim another binding's transport or IRQ/DMA resources.
Unmap failures are logged and are not retried by releasing another reference.

## Native ordinary IRQ provider

NativeIrq requests reject non-sleepable context before registration. Callbacks
are ordinary non-sleeping hardirq callbacks, not threaded or pseudo-NMI handlers.
The driver must stop hardware before dropping its IRQ guard and must release in
sleepable task context without locks or resources required by the handler.
Admission checks are snapshots, not permission across later context changes.

Explicit firmware domains and returned action IDs are retained without a global
host lookup table. Resolved virqs bypass firmware mapping. Native release uses
the retained resource and action ID. Plain-virq acquisition rejects an absent
descriptor or a conflicting specified trigger; Unknown means unspecified and
does not override stored configuration. This prevents admission through kirq's
unknown-virq compatibility fallback in the stable-mapping case. Descriptor lookup
and registration are separate operations, so mapping ownership must exclude
concurrent removal or reconfiguration; no atomic lifetime pin is claimed.
Native release uses
kirq's exact-action synchronous removal; a missing action, invalid token kind or
core error is not treated as proof of quiescence. Because IrqOp::release_irq has
no error return, the release site panics rather than logging and permitting
driver resource reclamation. Its narrow CG-021 exception identifies a future
fallible teardown contract that must retain resources on failure; no such core
protocol or retry mechanism is introduced here. Correct release requires the
original resource/token exactly once, without external removal of that action.

Mapping depends on initialized IRQ domains, and teardown depends on the current
branch's scheduler-backed kirq synchronization contract. First shared registration
can enable delivery; callers must make callback-visible state ready beforehand.
The constructor does not activate hardware or install global services. Pure
helper UT establish adapter decisions only, not real IRQ draining, physical
interrupt routing or acceptance of the deferred IRQ work.

## Retained DMA capability

DeviceToDriver acquisition copies the initialized original bytes into native
bounce storage before publication. Partial device writes preserve the original
unwritten bytes rather than copying stale pool contents or zeros back. Ordinary
unmap without device writes leaves the original unchanged, including unpublished
queue rollback; it is not cancellation of submitted DMA.
Other directions retain the original-buffer initialization performed by kdma.
The private publication test seam does not overwrite simulated device writes.

The host DeviceDma implementation retains the original DeviceRef rather than a
copied DMA configuration. Each operation validates the authoritative descriptor;
missing, invalid or non-coherent views cannot fall back to unrestricted DMA.
Device removal does not revoke this capability or stop hardware. Drivers remain
responsible for admission, quiescence and releasing guards in a valid context.

Streaming callers must initialize their entire original buffer for every DMA
direction and keep it live and exclusive until guard cleanup. The host validates
the complete native bounce interval before publishing
its translated bus address. Rejected mappings are discarded without copy-back,
including receive mappings. The unique private release hook retains the original
native bus token, buffer address, length and direction, so a translated device
address is never passed to native unmap. Successful unmap performs the required
directional copy-back but never frees the caller's buffer. Mapping state remains
owned by kdma, whose synchronized global pool outlives individual capabilities.
This adds no hardware isolation or non-coherent support. Production activation
belongs to the scoped binding, not capability acquisition itself.

Capability construction uses normal probe/task context after native allocator
and mapping initialization. Acquisitions and release are non-sleeping and may run
under a request spinlock or in ordinary IRQ context, not NMI. They can acquire
kdma, page-table and allocator spinlocks, including lazy bounce-pool setup;
callers must respect that lock order and not hold those locks recursively.
The native heap/page allocators use IRQ-safe locking and heap growth directly
allocates pages rather than sleeping or performing reclaim waits. This does not
promise allocation-free operation or bounded contention/latency. Arc hooks and
native bookkeeping use the kernel's infallible metadata allocation policy rather
than a recoverable metadata-exhaustion protocol.

## Scope

This analysis covers the entire crate: `src/lib.rs`, `src/manager.rs`,
`src/enumeration.rs`, `src/resource.rs`, `src/block_completion*.rs`,
`src/block_irq.rs`, `src/bus/`, and `src/driver_registry/` (including
the VirtIO, block, char, and net driver glue), plus `src/tests/`, which
is test-only. Reusable concrete drivers behind the registry are audited
in their own crates; this document covers the host integration glue.


## Trust Model

```text
Firmware (DT / ACPI) / PCI configuration space
   |
   | Untrusted: physical addresses, IRQ numbers,
   |            compatible strings, PCI vendor:device IDs
   v
kdriver
   +-- Safe boundary
   |   +-- BusManager enumeration dispatch
   |   +-- DriverRegistrar registration
   |   +-- EnumerationContext buffering
   |   +-- Ownership summary API
   +-- Unsafe boundary
       +-- VirtIoHalImpl
       |   +-- dma_alloc/dealloc
       |   +-- mmio_phys_to_virt
       |   +-- share/unshare
       +-- AhciDriver / SdMmcDriver
       |   Probe (MMIO -> vaddr)
       +-- IxgbeHalImpl
       |   DMA and MMIO address translation
       +-- virtio::probe_mmio_device
           Raw MMIO register reads
   |
   | Validated mappings, IRQ handler closures, DMA buffers
   v
device-res-xkernel / kirq / khal / kdma / memspace / driver crates
```

- `kdriver` trusts `device-res-xkernel` to implement the `device_res` provider contract
  correctly. Driver-facing devres APIs are exposed through
  `kdriver::resource::DeviceResourceExt`.
  `kdriver::resource` explicitly passes `XKernelResourceProvider` and converts
  `ResError` into `DriverError`.
- `device-res-xkernel` trusts `kirq::try_register_shared()` to validate interrupt
  descriptors, manage action fanout, and provide teardown synchronization.
  `device_res` reserves a threaded-IRQ provider contract; the X-Kernel provider based
  on `main` currently still returns `Unsupported`.
- `kdriver` and `device-res-xkernel` trust `memspace::iomap_device` to reject mappings
  of invalid physical address ranges.
- `device-res-xkernel` trusts `kdma::allocate_dma_memory` and
  `kdma::deallocate_dma_memory` to handle matching, valid `(cpu_addr, bus_addr)` pairs.
- `kdriver` trusts `virtio::probe_mmio_device` to perform the required volatile-read
  safety checks before accessing MMIO registers.
- `kdriver` trusts each driver crate (`block::ahci`, `block::sdmmc`, and `net::ixgbe`)
  to define the safety preconditions of its `new(vaddr)` entry point correctly.
- External callers in the `kruntime` boot path trust that `init_drivers` runs after
  the platform's `early_driver_init`.

## External Boundaries and Attack Surface

The block wait provider is a safe-code host adapter, not a DMA owner. It
validates sleepable task context before source construction and checks task
identity when waiting. An already-completed request needs no task registration.
Otherwise a scoped stack node is registered under the same IRQ-safe lock
as the completion state. Registration cannot fail or allocate. Spurious wakes
recheck the permanent terminal state. There is no admission wait.
Signals retain no request/device pointer and may outlive the waiter.
Notifications run outside device/registry locks; the constrained scheduler
wake runs under the completion source lock, which also serializes node cleanup.
Arc/Box preparation retains the infallible OOM policy, including BioWait's lazy
post-submission preparation. Pending requests and DMA retirement remain owned
by the device queue independently of the caller-side waiter.

`kdriver` is a core kernel crate that processes hardware description data.
Its attack surface primarily consists of untrusted physical addresses, IRQ numbers,
and device identities supplied by firmware, together with runtime configuration
values in PCI BAR registers.

The module directly or indirectly interacts with these boundaries:

- **Firmware input:** DT compatible strings, ACPI HID/CID, MMIO physical addresses,
  IRQ line numbers, and firmware source types (`DeviceTree` / `ACPI`).
- **PCI configuration space:** Vendor:device IDs, class/subclass, BAR addresses and
  sizes, header types, bridge secondary/subordinate bus numbers, and legacy INTx routing.
- **VirtIO MMIO registers:** Discovery registers such as `MagicValue`, `Version`,
  `DeviceID`, and `VendorID`.
- **Driver probe paths:** MMIO regions mapped through `iomap_first_mmio` or
  `devm_iomap`, and DMA buffers allocated through `devm_alloc_coherent`.
- **Interrupt registration:** IRQ line numbers supplied by firmware or PCI INTx routing
  are converted into `kirq::IrqSpec` by `device-res-xkernel` and connected to `kirq`
  through shared hardirq actions.
- **Compile-time static configuration:** Fixed platform addresses such as
  `kbuild_config::AHCI_PADDR` and `kbuild_config::SDMMC_PADDR`.

This module does not directly dereference userspace pointers; device discovery runs
in kernel process context.
It does parse physical addresses from firmware tables and PCI configuration space,
so physical address validation and MMIO mapping safety are central concerns.

Threat analysis should cover:

- Whether invalid firmware addresses can make MMIO mappings overwrite critical kernel data.
- Whether zero addresses left after PCI BAR allocation can bypass checks and map page zero.
- Whether devres IRQ handlers and `kirq` shared-action tokens have matching lifetimes.
- Whether mismatched DMA allocation/free layouts can corrupt memory.
- Whether paired `share`/`unshare` operations in the VirtIO HAL withstand malicious device DMA.
- Whether devres release ordering during device removal can cause use-after-free.

## Unsafe Code Inventory

### Block IRQ Provider Delegation

block_irq adds no unsafe code and no IRQ membership registry. Each registered
closure owns only a Weak device and the reclaimer Arc. Activation must retain
the strong device until the returned Irq has been dropped and the reclaimer
has stopped. The X-Kernel provider releases only the device's action token and
delegates escaped-callback synchronization to kirq; block does not duplicate
that bookkeeping or mask a shared line. Current kirq synchronization waits for
line-wide in-flight dispatches, not only this device's callback.

Provider resource/capacity errors propagate without publication or fallback.
Platform-local ID exhaustion likewise returns `DriverError::NoMemory` through
the existing enumeration error path, preserving the counter instead of wrapping
or panicking on firmware-controlled device counts.
The closure is dropped on failed registration, releasing its reclaimer clone.
The adapter mock verifies forwarding and token ownership, not asynchronous
release; existing kirq tests cover in-flight dispatch synchronization.
Production activation owns the actual admitted-I/O and transport shutdown path.
BlockActivation remains task-owned and is never captured by an IRQ callback.
Its unique registry close action is taken outside the registry mutex and invoked
by driver.remove before bus teardown, or by failed-probe cleanup. Device-core
serializes removal with begin_removing; devres fallback repeats only after the
first close has returned. IRQ registration and class publication failures run
the same partial-owner destructor without publishing a polling-only disk.

Close prepares task-bound waiters before changing state, marks Closing and
withdraws its owned block lookup, then drains all admitted calls including FIFO
and preparation. IRQ/reclaimer stay live through that drain. Only then does close
suppress device interrupts, release/synchronize the native action, stop the
reclaimer and destroy the transport. Close cannot return a recoverable error
that permits device-core to free resources with DMA still live. Invalid context
is fatal; no forced reset or timeout is introduced. Retained Gendisk references
contain inert device state, not a transport whose final Drop could occur in IRQ.

### Block Device Completion State

`block_completion_dispatch` uses UnsafeCell<ProcessingState> to couple each stable BlockIoReclaimer's state to
the single PENDING_DEVICES lock, rather than adding independently ordered slot locks.
Every state access, list push and list pop holds that lock. Only the owning,
pinned CPU consumes its finite detached batch. Queued also means detached, so
remote requests cannot link the same device completion twice. List<Arc<BlockIoReclaimer>> retains
the allocation; the module never manufactures raw ownership pointers.

The unsafe Sync implementation relies on this lock discipline, stable Arc
ownership, and Send+Sync devices. The fields remain private to block_completion_dispatch.
Device upgrade/callback/drop happen outside PENDING_DEVICES, and drop precedes publishing
Idle or notifying stop. Stop signals are independently Arc-owned and invoked
outside the lock. Arc Drop does not stop producers; activation must explicitly
call stop_and_wait with a fresh prepared waiter before freeing device resources.
The task-only stop_lock serializes concurrent closers through the wait so the
single stop_notification cannot be overwritten. IRQ/softirq never acquire it.
The only nesting is stop_lock -> PENDING_DEVICES; callbacks never hold either.
The provider trusts finite non-sleeping callbacks and online CPU progress.
No user-controlled pointers or device DMA state enter this executor.

State mutation is centralized in private `update_state`, except the atomic
batch-pop/Running transition under the same lock. The closure cannot return a
borrow into the protected state. The host provider validates bio callers before
submission without allocation. A pending bio prepares its task-bound waiter
when waited on; the block layer serializes notification enrollment with result
publication. Device-close drain/stop still prepare their waiters before exposing
notification handles. Signals never retain a task, device or data pointer.

### 1. device-res-xkernel — DMA Allocation

Location: `drivers/adapters/xkernel/device-res/src/dma.rs`

```rust
let info = unsafe { kdma::allocate_dma_memory(layout) }.map_err(|_| ResError::NoMemory)?;
```

Invariants:

- `layout` is constructed through `Layout::from_size_align(spec.len, spec.align)`,
  which validates the allocation layout.
- The returned `DmaAllocation` is owned exclusively by devres and released through
  `free_coherent` when the `DeviceObject` is removed.
- `cpu_addr` and `bus_addr` describe the same physical memory.

Safety rationale:

- `alloc_coherent` validates the size/alignment through `Layout` before calling
  `allocate_dma_memory`.
- `DmaAllocation` has no public destruction path; release is routed through
  `free_coherent` in the devres callback.
- The safety contract of `kdma::allocate_dma_memory` requires a valid layout and
  exclusive caller ownership of the returned buffer.

Callers:

- Driver probe paths through `devm_alloc_coherent`.

### 2. device-res-xkernel — DMA Release

Location: `drivers/adapters/xkernel/device-res/src/dma.rs`

```rust
unsafe { kdma::deallocate_dma_memory(info, layout) };
```

Invariants:

- `info` and `layout` match the original `alloc_coherent` call.
- Each `DmaAllocation` is released once, through devres LIFO cleanup and exclusive ownership.
- Device DMA has stopped before release, as guaranteed by the driver's remove callback.

Safety rationale:

- `free_coherent` reconstructs the layout using the same `Layout::from_size_align`
  arguments as `alloc_coherent`.
- `DmaAllocation` does not implement `Clone`; devres holds the sole ownership.

Callers:

- The `free_coherent` method of the `device-res-xkernel` provider, triggered by
  `device_res::DmaAllocation` destruction or devres release.

### 3. VirtIoHalImpl — Unsafe Trait Implementation

Location: `src/driver_registry/virtio/glue.rs:139`

```rust
unsafe impl VirtIoHal for VirtIoHalImpl { ... }
```

Overall invariants:

- `dma_alloc` and `dma_dealloc` use matching `(paddr, vaddr, pages)` tuples.
- `mmio_phys_to_virt` is called only for physical addresses aligned to `PAGE_SIZE_4K`.
- `share` and `unshare` are paired with matching directions.
- `dma_alloc` zeroes newly allocated memory through `write_bytes(0)` so the device
  cannot read residual kernel data.

Individual operations:

#### 3a. `dma_alloc`

Location: `src/driver_registry/virtio/glue.rs:148,150`

- Constructs `Layout::from_size_align(pages * PAGE_SIZE_4K, PAGE_SIZE_4K)` before
  calling `allocate_dma_memory`.
- Zeroes the allocation through `write_bytes(0)`:
  `unsafe { core::ptr::write_bytes(dma_info.cpu_addr.as_ptr(), 0, size) }`.
- Returns `(0, NonNull::dangling())` on failure; the VirtIO transport detects and
  reports the error.

#### 3b. `dma_dealloc`

Location: `src/driver_registry/virtio/glue.rs:165,178`

- Uses the same `Layout` as allocation: `pages * PAGE_SIZE_4K` bytes aligned to `PAGE_SIZE_4K`.
- Reconstructs `DmaInfo` from `paddr` and `vaddr` before calling `deallocate_dma_memory`.

#### 3c. `mmio_phys_to_virt`

Location: `src/driver_registry/virtio/glue.rs:183`

```rust
unsafe fn mmio_phys_to_virt(paddr: PhysAddr, size: usize) -> NonNull<u8> {
    iomap_mmio(paddr as usize, size, "virtio-mmio-hal")
        .expect("failed to iomap virtio MMIO region")
}
```

- `iomap_mmio` calls `memspace::iomap_device` through the standard MMIO validation path.
- Mapping failure panics. This path runs only after the VirtIO transport has confirmed
  that the device exists, so failure indicates a platform configuration error.

#### 3d. `share`

Location: `src/driver_registry/virtio/glue.rs:190,195`

```rust
unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection, ...) -> PhysAddr {
    unsafe { kdma::map_dma_buffer(buffer, dma_direction(direction)) }
        .expect(...)
        .bus_addr.as_u64() as PhysAddr
}
```

- `buffer` is a valid buffer supplied to the VirtIO transport by `dma_alloc` or an upper layer.
- `direction` is correctly translated into `kdma::DmaDirection`.

#### 3e. `unshare`

Location: `src/driver_registry/virtio/glue.rs:203,209`

```rust
unsafe fn unshare(paddr: PhysAddr, buffer: NonNull<[u8]>, direction: BufferDirection, ...) {
    unsafe { kdma::unmap_dma_buffer(DmaBusAddress::new(paddr), buffer, dma_direction(direction)) };
}
```

- `paddr` and `buffer` come from the same `share` operation.
- The `virtio` transport guarantees paired calls.

Callers:

- The `virtio` transport layer: `VirtIoNetDev`, `VirtIoBlkDev`, `VirtIoGpuDev`,
  `VirtIoInputDev`, `VirtIoSocketDev`, and `VirtIo9pDev`.

### 4. VirtIO MMIO Discovery During Platform Enumeration

Location: `src/bus/platform_backend.rs:193`

```rust
(unsafe { virtio::probe_mmio_device(regs.as_ptr(), mmio.size) })
```

Invariants:

- `regs` comes from `iomap_mmio(mmio.base, mmio.size, "virtio-mmio-discovery")`,
  which validates the mapping.
- `mmio.size` matches the size used to create the mapping.
- Discovery reads only registers defined as read-only by the VirtIO specification,
  such as `MagicValue`, `Version`, and `DeviceID`.

Safety rationale:

- Success from `iomap_mmio` means that the physical address lies in a valid MMIO window
  and the mapping has been established.
- The safety precondition of `virtio::probe_mmio_device` requires `regs` to point to
  a valid, accessible MMIO region large enough for a complete VirtIO MMIO register frame.

Callers:

- `PlatformBackend::enumerate_firmware` through `virtio_mmio_registration`.

### 5. VirtIO MMIO Discovery During Driver Activation

Location: `src/driver_registry/virtio/mod.rs:194`

```rust
unsafe { virtio::probe_mmio_device(regs.as_ptr(), size) }.ok_or(DriverError::BadState)?;
```

Invariants:

- `regs` comes from `iomap_mmio(base, size, "virtio-mmio-transport")`.
- This path runs only for `DeviceLocation::Mmio` after the transport type has matched.
- `size` comes from `DeviceLocation::Mmio.size` and matches the value recorded during
  platform enumeration.

Safety rationale:

- The same as item 4.

Callers:

- `activate_virtio_mmio`, through `activate_virtio_device` and the VirtIO driver's `probe_device`.

### 6. AHCI Driver Probe

Location: `src/driver_registry/block/ahci.rs`

```rust
// Ordering only: not data-cache clean/invalidate.
karch::dma_read_barrier();

// Constructor first checks the HAL's explicit coherent-DMA capability.
let ahci = match unsafe {
    block::ahci::AhciDriver::<AhciHalImpl>::new(vaddr, resource::resource_provider())
} { ... };
```

Invariants:

- `vaddr` comes from `iomap_first_mmio(device, "ahci")`, with its lifetime managed by devres.
- The pointer returned by `iomap_first_mmio` remains valid while `device` is alive.
- `karch::dma_read_barrier()` only orders accesses; it does not establish cache
  coherency for data or command memory. The SDK builds PRDT entries directly
  from physical addresses and has no data-buffer cache-sync hook.
- `AhciHalImpl` implements the unsafe `AhciHal` admission contract with
  `DMA_COHERENT = cfg!(target_arch = "x86_64")`. All other hosts are rejected
  before SDK initialization. Non-coherent AHCI requires a separate SDK/HAL
  DMA/cache protocol and must not be enabled by merely setting the flag.

Safety rationale:

- The safety precondition of `AhciDriver::new` requires `vaddr` to point to a valid,
  exclusive AHCI HBA MMIO window.
- `iomap_first_mmio` establishes mapping validity through `devm_iomap` and
  `memspace::iomap_device`.

Callers:

- `AhciDriver::probe_device` (feature `ahci`).

### 7. SDMMC Driver Probe

Location: `src/driver_registry/block/sdmmc.rs:42`

```rust
let dev = unsafe { block::sdmmc::SdMmcDriver::new(vaddr) };
```

Invariants:

- `vaddr` comes from `iomap_first_mmio(device, "sdmmc")`.
- The safety precondition of `SdMmcDriver::new` requires `vaddr` to point to a valid
  SD/MMC controller register region.

Safety rationale:

- Follows the AHCI pattern: `iomap_first_mmio` establishes mapping validity and
  devres manages its lifetime.

Callers:

- `SdmmcDriver::probe_device` (feature `sdmmc`).

### 8. IxgbeHal — Unsafe Trait Implementation

Location: `src/driver_registry/net/ixgbe_hal.rs:14`

```rust
unsafe impl IxgbeHal for IxgbeHalImpl { ... }
```

Individual operations:

- `dma_alloc` (line 17): `Layout::from_size_align(size, 8)` followed by `allocate_dma_memory`.
- `dma_dealloc` (lines 23 and 29): Reconstructs `Layout`, then calls `deallocate_dma_memory`.
- `mmio_p2v` (line 33): Directly converts a physical address to a virtual address through
  `khal::mem::p2v`, assuming that the caller supplies a valid physical address.
- `mmio_v2p` (line 37): Performs the reverse conversion through `khal::mem::v2p`.

Invariants:

- DMA allocation/deallocation use matching `(paddr, vaddr, size)` tuples.
- `mmio_p2v` and `mmio_v2p` run only after the probe has confirmed physical address validity.

Safety rationale:

- The `ixgbe` feature is currently a placeholder and does not enable the downstream
  dependency, so the HAL implementation is not called.

Callers:

- The `ixgbe` driver crate (feature `ixgbe`, currently a placeholder).

### 9. PL011 Serial Driver Probe

Location: [`src/driver_registry/char/serial.rs:99`](../src/driver_registry/char/serial.rs#L99),
where the `SerialKind::Pl011` branch of `resolve_port` calls `SerialPort::new_mmio_pl011`.

Invariants:

- `vaddr` comes from `device.devm_iomap(mmio, "serial")`; `paddr` and `mmio.size`
  describe the same MMIO resource.
- The firmware resource must describe a real PL011 register window, aligned for
  32-bit accesses and covering all registers used during construction and I/O.
- The mapping must remain valid while the port is in use.
  Device removal must stop port accesses before releasing the devres mapping.
- New ports are initialized exclusively before `publish` exposes them.
  The stdout path reuses the existing instance through `take_early_port` without reinitialization.

Safety rationale:

- `devm_iomap` uses the `device-res-xkernel` MMIO provider and `memspace::iomap_device`
  to establish a valid mapping. Mapping errors propagate through `?` before the
  constructor is called.
- The device's devres owns the mapping and manages its lifetime, releasing it on
  probe failure or device removal. `SerialPort` does not own the mapping itself.
- `Pl011SerialDriver::probe_device` completes construction in `resolve_port` before
  exposing the port through `publish`, preserving exclusive initialization.

Callers:

- `Pl011SerialDriver::probe_device` (feature `serial-pl011`).

## Memory Safety Invariants

1. **MMIO virtual address lifetime:** The `NonNull<u8>` returned by `devm_iomap` is valid
   only while the `DeviceObject` is alive. Probe failure or device removal releases
   the mapping through `iounmap`.
2. **Exclusive DMA buffer ownership:** The `DmaAllocation` returned by
   `devm_alloc_coherent` is owned exclusively by devres and has no public clone/copy interface.
3. **Paired DMA allocation/free:** `alloc_coherent` and `free_coherent` reconstruct
   `Layout` from the same `DmaSpec`, preserving size and alignment.
4. **IRQ handler registration order:** A devres handler is wrapped as a `kirq` action,
   then atomically inserted into the descriptor's action list by `kirq`.
5. **IRQ handler release:** Shared hardirq actions are removed by token; regular
   threaded actions are released through their regular action.
   `kirq` masks the line and waits for in-flight hardirq snapshots and the IRQ thread to exit.
6. **No heap allocation during IRQ dispatch:** `kirq` performs action fanout through
   a fixed-size stack snapshot.
7. **VirtIO DMA zeroing:** `dma_alloc` clears new allocations through `write_bytes(0)`
   to prevent devices from reading residual kernel data.
8. **Paired VirtIO share/unshare:** `share` and `unshare` are paired with matching
   directions by the `virtio` transport.
9. **Rejection of zero PCI BAR addresses:** BARs that remain zero after allocation
   are skipped during enumeration and are not registered as valid resources.
10. **Firmware physical address validation:** MMIO addresses supplied by firmware
    pass through `memspace::iomap_device` validation before mapping.
11. **Device identity allowlist:** VirtIO PCI device IDs are translated through the
    `pci_device_id_to_virtio_type` allowlist. Unknown IDs are registered as generic
    PCI devices without binding a VirtIO driver.

## Thread Safety

| Type | Send Conditions | Sync Conditions |
|------|-----------------|-----------------|
| `DeviceManager` | Fields satisfy `Send` | `SpinNoPreempt<BusManager>` provides interior mutability. |
| `BusManager` | `Vec<(BusId, Box<dyn BusBackend>)>` satisfies `Send` | Shared access is protected by `SpinNoPreempt`. |
| `EnumerationContext` | `Vec<DeviceDesc>` satisfies `Send` | Does not implement `Sync`; used by one thread. |
| `DriverRegistrar` | Zero-sized type | Accesses shared state only through the global `kdevice` lock. |
| `device-res-xkernel::XKernelResourceProvider` | Zero-sized type | The provider has no internally mutable state; `kirq` protects IRQ action state. |
| `PCI_BAR_ALLOCATOR` | `SpinNoPreempt<Option<PciRangeAllocator>>` satisfies `Send` and `Sync` | `SpinNoPreempt` provides interior mutability. |
| `PlatformBackend` | The `LocalIdAlloc` field is `Copy` and satisfies `Send` | Does not implement `Sync`; access is serialized by the `BusManager` lock. |
| `PciBackend` | `Cam` satisfies `Send` | Does not implement `Sync`. |
| `VirtIoHalImpl` | Zero-sized type | Calls `kdma` and `iomap_mmio`, each responsible for its own thread safety. |

## Threat Analysis

| ID | Threat | Impact Level | Trigger | Mitigation |
|----|--------|--------------|---------|------------|
| T-01 | Invalid firmware physical addresses cause MMIO mappings to overwrite critical kernel data | High | DT/ACPI describes malicious addresses that `memspace::iomap_device` does not reject | `iomap_device` checks platform MMIO window membership and returns `InvalidRange` for invalid ranges. |
| T-02 | A PCI BAR remains zero after allocation, causing a page-zero mapping | High | The BAR allocator is exhausted or its range is uninitialized, and configuration fails to reject zero addresses | `configure_pci_device_if_needed` returns `NoMemory` on allocation failure; enumeration pass 3 skips BARs with `address == 0`. |
| T-03 | IRQ registration races with interrupt arrival, invoking an unready handler | High | The interrupt reaches the virtual IRQ before its action is installed | `kirq` installs the action in its IRQ descriptor before enabling the line according to policy; the threaded default-primary entry enforces generic `ONESHOT`. |
| T-04 | DMA double-free corrupts memory | High | `free_coherent` is called more than once or the layout does not match | `DmaAllocation` does not implement `Clone`; devres has exclusive ownership, and allocation/free reconstruct `Layout` from the same `DmaSpec`. |
| T-05 | A VirtIO device accesses unauthorized kernel memory through malicious DMA descriptors | High | A malicious or faulty VirtIO device constructs an invalid descriptor chain | Individual VirtIO devices currently lack IOMMU isolation; `dma_alloc` zeroing prevents information disclosure, and `kdma` manages `share`/`unshare`. |
| T-06 | Firmware spoofs a device compatible string and binds the wrong driver | Medium | A false DT compatible string matches a registered `FirmwareMatchSpec` | An unresponsive device fails probe and enters the unclaimed list. |
| T-07 | A PCI device spoofs its vendor:device ID and matches the wrong VirtIO type | Medium | A malicious PCI device advertises the Red Hat vendor ID and a known VirtIO device ID | `probe_pci_device` validates the transport response again during activation and returns `Unsupported` on mismatch. |
| T-08 | Too many handlers on a shared IRQ cause unbounded traversal | Medium | More than 4 handlers register on one IRQ | `request_irq` returns `ResError::Busy`; dispatch uses a fixed-size stack snapshot. |
| T-09 | A PCI BAR allocator race assigns the same MMIO address to two devices | Medium | Concurrent BAR allocation is not serialized correctly | `PCI_BAR_ALLOCATOR` is protected by `SpinNoPreempt`, and allocation occurs under the lock. |
| T-10 | The serial driver remaps the stdout UART's MMIO and creates duplicate ownership | Medium | The serial driver calls `devm_iomap` again for the stdout node | Serial probe uses `take_early_port` and `SerialIdent` to adopt the early stdout instance without a second mapping. |
| T-11 | Incorrect devres release ordering frees resources while a device still uses them | Medium | The driver's remove callback returns without stopping device DMA | Devres LIFO cleanup preserves release ordering; the driver's remove callback is responsible for stopping the device. |
| T-12 | VirtIO MMIO discovery reads unmapped or invalid registers | Medium | Firmware advertises `virtio,mmio` but no VirtIO device exists at the physical address | `classify_mmio_device` checks extent/alignment and identification without a transport; ownership lookup precedes mapping. Firmware must still describe readable hardware. |
| T-13 | An unchecked firmware IRQ number is registered on the wrong vector | Medium | `kirq::register` does not validate the IRQ number sufficiently | Depends on the platform IRQ backend and `kirq` descriptor handling; IRQ numbers passed by `kdriver` come from firmware or PCI INTx routing. |
| T-14 | Static platform device addresses such as AHCI_PADDR are misconfigured at compile time | Low | `kbuild_config` constants specify invalid physical addresses | `iomap_first_mmio` validates through `devm_iomap` and `iomap_device`; mapping failure prevents driver activation. |
| T-15 | Incorrect conversion between devres and kernel IRQ types causes bad IRQ configuration, lost source bitmaps, or unwoken IRQ threads | Medium | The `device-res-xkernel` adapter omits trigger/controller/event/wake-thread fields | `device-res-xkernel` centralizes `device_res` to `kirq` conversion; the IRQ core does not depend on devres. |
| T-16 | VirtIO input is published before its interrupt action is usable | Medium | Input construction succeeds but IRQ registration fails or races publication | `virtio/input.rs` requires an IRQ, registers its devres action first, and calls `publish_input` only after registration succeeds. |

Impact levels:

- High: Undefined behavior, memory corruption, or privilege escalation.
- Medium: Panic, service unavailability, or inconsistent state.
- Low: Performance degradation, lost logs, or reduced functionality.

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure Mode | Cause | Local Effect | System Effect | Severity | Mitigation |
|----|--------------|-------|--------------|---------------|----------|------------|
| F-01 | PCI bus enumeration fails | ECAM/MmioCam mapping failure or inaccessible configuration space | All PCI devices unavailable | PCI-dependent functions such as virtio-blk/net/gpu are unavailable | 2 | Log an error and return when `PciBus::new` fails, without blocking platform bus enumeration. |
| F-02 | Firmware provides no device description | Missing DT/ACPI tables or `has_device_description()` returns `false` | No devices registered through firmware enumeration | Only static devices remain, such as ramdisk and compile-time AHCI/sdmmc | 3 | Static device discovery is independent of firmware; log at info level. |
| F-03 | One device fails probe | The driver's `probe_device` returns an error | That device unavailable | Other devices on the same bus activate normally | 4 | Log the probe error at warn level and place the device in the unclaimed list; continue with subsequent devices. |
| F-04 | PCI BAR allocator uninitialized | `pci_bar_allocation_range()` returns `None` and a device has unassigned memory BARs | The PCI device is skipped | One PCI device unavailable | 3 | `configure_pci_device_if_needed` returns `NoMemory`, and the device is skipped. |
| F-05 | VirtIO MMIO discovery returns no device | No VirtIO device in the region or a `MagicValue` mismatch | The MMIO region is skipped | Other platform devices unaffected | 4 | `probe_mmio_device` and then `virtio_mmio_registration` return `None`; log at trace level and skip the region. |
| F-06 | IRQ handler registration fails | `kirq` rejects a shared action or its action limit is reached | The device cannot receive interrupts | Block and input devices are not published; no polling fallback | 3 | `request_irq` returns `Busy`; partial activation is dropped before probe returns an error. |
| F-07 | PCI host bridge adoption fails | Platform bus unregistered or `adopt_active_device` fails | PCI devices have no host bridge parent | PCI endpoints are still enumerated, but the device tree is incomplete | 3 | Log a warning and continue enumeration with a parentless layout. |
| F-08 | Static device MMIO mapping fails | Invalid `kbuild_config` address or absent hardware | The static device unavailable | Other devices on the same bus unaffected | 3 | `iomap_first_mmio` returns an error, and probe fails. |
| F-09 | Bus-type matcher is not ready when a driver registers | `register_bus_type` runs after driver registration | No device matches the driver | Devices enter the unclaimed list | 2 | `default_bus_manager` registers bus-type matchers before bus backends; drivers are then registered in `DeviceManager::new`. |
| F-10 | Rescan produces duplicate device descriptors | The backend uses the default `rescan` hook and reruns full `enumerate` | `kdevice` may reject duplicates or create redundant descriptors | Incomplete hot-plug support | 3 | Default `rescan` reruns `enumerate`; backends may override it with incremental scanning. |
| F-11 | VirtIO transport type differs from the driver's declaration | Reported `DeviceKind` differs from the matched driver's `device_type` | Driver probe returns `Unsupported` | The device enters the unclaimed list | 4 | `activate_virtio_device` validates the type again in PCI/MMIO activation paths. |
| F-12 | Quiesce fails to stop device interrupts | Incorrect bus backend `quiesce` implementation or delayed hardware response | Interrupts continue arriving during shutdown | IRQ handlers may access released resources | 2 | Devres resources are released during remove, not quiesce; quiesce only masks interrupt sources. |

Severity levels:

- 1: Fatal; system crash or data loss.
- 2: Serious; functionality unavailable and restart required for recovery.
- 3: Moderate; degraded functionality with automatic recovery possible.
- 4: Minor; limited impact that users can tolerate.

## Failure Management

- Device probe failures return `DriverError` (`InvalidInput`, `Io`, `NoMemory`,
  `ResourceBusy`, `Unsupported`, or `BadState`) without panicking.
- PCI bus initialization failure logs through `error!` and returns `Ok(())`, allowing
  platform bus enumeration to continue.
- Individual firmware device registration errors are collected through
  `first_error.get_or_insert`; enumeration returns the first error after completing.
- MMIO mapping failures are converted through `memspace::IoMapError`, `ResError`,
  and `DriverError`, with traceability at each layer.
- IRQ registration failures return `ResError::Busy`, `ResError::NoMemory`,
  `ResError::Unsupported`, or `ResError::InvalidResource`; the upper probe layer
  returns the corresponding error.
- Unmatched devices enter the `unclaimed` list. `info!` logs their identity,
  location, and origin to help diagnose missing drivers.
- Except for the `expect` in `VirtIoHalImpl::mmio_phys_to_virt`, which runs only after
  the VirtIO transport has confirmed device presence, unsafe failure paths return
  `Result` or log errors.
- Panic paths primarily come from `expect` after `LazyInit::call_once`, where static
  initialization failure indicates a platform configuration error, and overflow in
  `LocalIdAlloc::alloc`, where `u16` overflow indicates an abnormal device count.

## Privacy Analysis

`kdriver` processes firmware device identity information: compatible strings,
ACPI HID/CID, PCI vendor:device IDs, and class/subclass values.
It also handles hardware resource descriptions such as physical addresses and IRQ numbers.
Debug/info logs include device names, BDF addresses, physical address ranges, and IRQ
line numbers, without user process data.

The module does not persist data.
Device topology is stored in the shared `kdevice` core and its lifetime is managed
by the global device registry.

Trace logs expose compatible strings encountered during firmware enumeration and
register values read during VirtIO MMIO discovery.
Production deployments should control exposure through log levels.

## Known Limitations

- PCI supports only segment 0. Multiple segments require extending the `PciBackend`
  domain parameter.
- The PCI BAR allocator uses simple sequential allocation through `PciRangeAllocator`,
  without BAR relocation or defragmentation.
- A shared IRQ supports at most 4 handlers; exceeding the limit prevents device activation.
- The `ixgbe` feature is a placeholder. Its HAL implementation is not called and has
  not been validated at runtime.
- Without a firmware description, `fxmac` only logs a warning and skips the device;
  its MMIO base cannot be obtained from compile-time configuration.
- Firmware enumeration does not process ACPI _DSD or complex device properties;
  it reads only compatible/HID/CID, MMIO, and IRQ resources.
- PCI hot-plug is supported only through full re-enumeration with `rescan`.
  There is no native hot-plug event handling or PCIe AER / hot-plug controller driver.

## Audit Checklist

When modifying this module, verify:

- Every `unsafe` block has a `SAFETY:` comment.
- New MMIO mapping paths use `devm_iomap` or `iomap_mmio`, which validate physical
  address ranges internally.
- New DMA allocation paths use `devm_alloc_coherent` or `kdma::allocate_dma_memory`,
  with matching layouts at `free` time.
- New IRQ registration passes through the `device-res-xkernel` devres provider into
  the `kirq` action list, instead of maintaining separate line-local fanout in `kdriver`.
- VirtIO input keeps PCI INTx masked until its devres handler is installed,
  masks it again before handler release, and leaves an empty input ISR
  unclaimed. Its input-layer notifier is called only for a queue interrupt;
  configuration interrupts do not wake input readers.
- New IRQ release uses `device-res-xkernel` to call the matching `kirq` release API
  by token; `kirq` performs masking, synchronization, and cleanup after the final action leaves.
- When a new bus backend implements `BusBackend`, an individual device error in
  `enumerate` does not block enumeration of other devices.
- New PCI device-ID to VirtIO-type mappings are added in `pci_device_id_to_virtio_type`.
- New firmware compatible matching rules are declared in `firmware_specs.rs` and
  register the corresponding platform driver.
- New `DeviceDriver::probe_device` implementations return `DriverError` on failure
  rather than panicking.
- New `VirtIoHal` methods preserve `share`/`unshare` and `dma_alloc`/`dma_dealloc` pairing.

## Device management lock adapter (R1-01)

The factory and every `run_locked` call reject a non-sleepable current context
before allocation or lock acquisition, respectively. IRQ-off, preemption-disabled,
and explicit RCU read scopes are rejected. Invalid context takes precedence over
same-task recursion, which otherwise returns `Reentrant`. Neither error invokes
the callback. The adapter uses the safe mutex ownership query and adds no unsafe
code or manual Send/Sync implementation.

The callback is trusted kernel code, borrowed only for this synchronous call.
It must restore its execution context before returning and must not wait for work
requiring the same lock. The adapter detects same-task recursion but cannot detect
cross-task dependency cycles, enforce ordering between distinct locks, or bound
callback duration. RAII releases the lock on normal return; this does not promise
recovery from an aborting kernel panic. Arc allocation follows kernel OOM policy.
The capability introduces no user-memory, MMIO, DMA, or firmware input boundary.

The model retains the lock and domain identity with the device core, validates
foreign-domain references before entering the host lock, and revalidates the
registered object under its short state lock. Registration prepares and enters
management before publishing an entry. Probe and teardown retain the services
bundle chosen at registration, even if the domain default is later replaced.
Unregister deletes the entry; same-domain repeated removal does not repeat
cleanup. Unbind preserves registration and can be followed by a fresh bind.
Callbacks and Data visitors run outside the domain spinlock but inside the
device management lock. Different devices can progress concurrently, so the
adapter does not protect host-global mutable resources between devices.

The lifecycle test host now installs the real factory through `DeviceServices`.
Registration failure, invalid-context zero-side-effect behavior, same-task
callback reentry, same-device removal serialization, and independent-device
callback progress have external regression coverage. The test resources are
unsupported stubs, so these cases do not establish hardware teardown safety.
The host fixture uses private per-test domains and weak domain references in
reentrant hooks, avoiding global registrations and domain/driver ownership cycles.
Concurrent tests release callback gates and join exited workers before assertions;
if a regression prevents exit, a bounded watchdog reports failure rather than
entering an unbounded join. The enclosing run must terminate stuck kernel tasks.
R1-02 also covers an explicit binder contending with the same device's in-flight
probe: Completion and task-state observations establish contention, then one
probe and one cleanup are asserted after both tasks exit. No timing threshold
is used as evidence of serialization.

Production composition-root wiring and real controller/bus teardown still require
separate integration and validation. The model does not recursively remove children
or bridge members; the bus must stop enumeration and order downstream teardown.
The downstream test controller covers owned-only teardown, parent service lifetime,
an in-progress enumeration commit drained by shutdown, and old-binding rejection
after rebind. Its enumeration gate is separate from the service transaction lock;
child removal verifies that the latter is available and still live. Completion and
task-state handshakes order the race, with watchdogs only for failure detection.
No production enumeration gate or generic recursive removal is added. Legacy class migration
is still incomplete: ordinary unbind keeps the registration source online,
and neither this lock nor a retained DeviceRef
proves that publications or hardware operations have drained.

## Hardware entropy completion provider

`wait_until` requires a sleepable current task. `CompletionWait::prepare`
rejects invalid context before the driver's predicate can consume bytes.
Task interruption does not cancel this internal read. The device notification
registration is removed by RAII when the wait ends.

The provider registers wake sources before checking readiness. A notification
between that check and `park` remains a wake hint, preventing a lost completion;
spurious wakes only trigger another check. The driver must call IRQ-safe
`notify` after releasing its request lock, and its predicate must not sleep.
The provider retains neither the predicate nor DMA storage. Invalid context and
preparation failures do not establish a device fault. Once the predicate is invoked,
scoped registration and wakeup are allocation-free and cannot return registration errors.

R1-03 reliable drain uses a distinct WaitQueue and RngDrain contract. Illegal
context returns before the first private predicate call and leaves the registry
unchanged. After the predicate closes admission, the host must not return an
ordinary wait error; it uses CompletionWait's stack-scoped registration. Removed
subscribers are invoked by hwrng only after operations drain, not by the host wait
provider. Final source/runtime/provider destructors must not sleep; runtime
hardware teardown remains explicit driver work, not a consumer-handle Drop action.

R1-04 audits the normal remove chain: the domain enters the device management
provider, which rejects atomic context before callbacks; its sleepable mutex
does not disable preemption or IRQs across callbacks. The driver stops/wakes
runtime operations, then calls explicit unregister. CompletionWait preparation
therefore receives valid context if callbacks restored their context as required.
The private drain predicate only holds short locks, and removed notifications
run after drain without those locks. Resources are released after unregister.
The test driver records unexpected cleanup errors for assertion instead of
adding a blanket expect to normal fallible interfaces. Production drivers must
individually justify their teardown preconditions and hardware-stop invariants.
The test resource guard is simulated: real MMIO/IRQ/DMA teardown remains unverified.

The legacy RNG factory rejects non-sleepable context before hardware
initialization. It injects a yielding reset-wait interval for constructor
rollback, and its publisher retains a concrete runtime reference until
registration succeeds. Registration failure invokes explicit close/finish;
reset confirmation, not the yield callback or a timeout, authorizes release.
This does not establish a production hot-unplug path or certify the external
IRQ core's synchronization implementation.

The R1-07 concrete RNG lifecycle fixture exercises the real model management
lock and real completion/drain providers with a sleeping consumer. It checks
that normal remove and probe failure after publication both stop DMA ownership
before Data destruction, even while stale consumers retain the runtime.
Counting HAL wrappers observe allocation/mapping release; they do not establish
IOMMU/cache correctness. The IRQ provider is a synchronous simulation, not a
concurrent interrupt-core acceptance test. The separate production binding now
uses these contracts; the fixture does not validate its hardware.
## Model MMIO guard ownership

The new host MMIO helper is for trusted model resource callbacks, not untrusted
physical-address requests. Explicit regions are valid for bus-resolved BARs and
are not forced to duplicate descriptor entries. Safe IoMap construction is backed
by a successful native mapping covering the full requested extent. A unique
private release hook pairs one native reference with one guard Drop; shared and
pinned mappings retain memspace's existing lifetime policy. Native unmap errors
are logged, not silently swallowed or retried. Callers must stop IRQ/register
access before dropping their final mapping guard. Tests never dereference the
synthetic physical test interval and do not certify hardware quiescence.

# New-model firmware DMA conversion

`firmware_dma::from_firmware` rejects malformed or unsupported firmware paths
without replacing them with defaults. Missing window/coherency requires explicit
platform handling elsewhere. A successful result establishes numeric address
constraints only, not hardware identity, resource ownership or cache handling.
Every subsequent host allocation must validate its complete interval and unwind
the allocation on rejection. Legacy provider behavior is unchanged.

## Model coherent DMA host adaptation

`host_dma` accepts a borrowed model context, not caller-forged global DMA facts.
It checks all page-rounded bytes against descriptor window/mask and checks bus
alignment before exposing an address. The private unpublished native owner
releases on rejection. Once published, one CoherentMem hook owns release using
the original native bus token and layout; translated device addresses must never
be passed back as native tokens. The pointer reconstructed by that hook comes
from the original nonzero address, never an external integer input.

Zeroing includes page padding to avoid exposing prior allocation contents.
The adapter rejects non-coherent configurations rather than silently treating
missing cache maintenance as success. Numeric validation is not device isolation:
there is no IOMMU enforcement here, and callers must stop hardware DMA before
dropping the guard. kdma cleanup failures retain pages and log rather than reuse
pages whose mapping/share state is uncertain. Tests use real allocator/mapping
services but no hardware DMA; they do not certify cache or IRQ teardown behavior.

The opt-in QEMU MMIO DMA policy requires HAL-provided original-node provenance,
not the caller's matcher alias. Missing ranges alone do not authorize identity
DMA. Rejected firmware paths, invalid masks/windows and explicit windows are
never replaced. A bounded RAM window is an address constraint, not permission
to use arbitrary RAM; allocation and hardware quiescence obligations above remain.
This platform-specific policy trusts the boot firmware's platform identification.

Host service composition retains the IRQ provider and its exact resource/action
token in each guard; removing or replacing a domain's service bundle cannot
redirect release to another provider. Firmware mapping failures never proceed
to registration, and failed registrations never create a release obligation.
The IRQ provider is trusted to detach and synchronously drain the selected action
on release, and to return stable mappings for repeated resolution. Drivers must
stop hardware before releasing guards in the provider's permitted context. The
void release contract does not provide a recoverable teardown-error protocol.
The host adapter neither invents one nor treats this contract test as IRQ-core
acceptance. Services do not cache descriptor DMA/MMIO facts or grant new device
ownership; production boot selection is unchanged by constructing the aggregate.
