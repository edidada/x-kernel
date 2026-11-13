# CPU and Memory Hotplug Design

`hotplug` owns the class state machines that sit between firmware or hypervisor
notifications and `kdevice`'s generic system-device lifecycle. It is host-kernel
policy, not a reusable driver, so it depends on `khal`, `kalloc`, `kcpu_id_map`
and `memspace` in addition to the driver contracts. This is a deliberate
exception to the usual driver dependency boundary, recorded here because CPU
and memory hotplug must manipulate host scheduler topology, the direct map and
the global allocator.

## Topology and state

`hotplug::init()` enumerates present logical CPUs and 128 MiB-aligned memory
blocks that intersect `MemFlags::FREE` RAM, then installs one
`SystemDeviceDescriptor` per device. CPU indices are logical CPU ids. A memory
device is exactly one `MEMORY_BLOCK_SIZE_BYTES` (128 MiB) block: its size must
equal the block size, its physical start must be block-aligned, and its index
must equal `physical_start_bytes / MEMORY_BLOCK_SIZE_BYTES`. This is the same
indexing exposed by `phys_index` and the sysfs block bitmap.

Presence is separate from online state: a device directory exists iff its
descriptor is installed, while `online` follows `kcpu_id_map::is_cpu_online` or
the memory controller. Boot topology is installed from firmware descriptions,
so it appears even when no hotplug event source has been registered or started.

The generic `kdevice::set_system_device_online` transition owns publication for
transitions that it performs: `GoingOffline` precedes a successful teardown,
`Offline` follows commit, and a failure restores the previous state and
publishes the corresponding restoring terminal event (`Online` after a failed
offline, `Offline` after a failed online). Controllers must not publish events
and must leave the device unchanged on error. The memory controller uses this
transition. `HotplugController::set_online` preflights class policy before
entering it so rejected memory requests do not produce a transient
`going-offline` event. A controller that rejects before entering the generic
transition, as the CPU controller does for every real transition, therefore
publishes no event at all and leaves the published state unchanged.

## Validation boundary

Sysfs and hotplug event sources dispatch writes through
`hotplug::controller_for(class)`. That class controller is the policy boundary:
it validates request shape and class-wide constraints, then calls the generic
`kdevice` transition. The per-device `CpuControl` and `MemoryControl` are the
narrower hardware side and revalidate only invariants that must hold even when
`kdevice::set_system_device_online` is called directly by kernel code.

For memory, the class controller checks overlap with every other present memory
device and requires the block to be covered by firmware-described `FREE` RAM.
`MemoryControl` validates the fixed block shape before mapping or adding pages,
but deliberately does not repeat the overlap or free-RAM scans: those are
policy decisions, and a kernel caller using the generic API is responsible for
the same preflight. Keeping that boundary explicit avoids claiming that the
generic API enforces sysfs policy.

## CPU state machine

The boot CPU is logical CPU 0 and also the CPU currently executing a request;
neither identity may be offlined. The last online CPU can never go offline.
Missing IDs report `ENODEV` and sysfs rejects values other than `0`/`1` with
`EINVAL`. A request to offline either protected CPU returns `EPERM` before the
generic transition. Valid requests that would change state currently return
`EOPNOTSUPP`: the platform provides only one-way `karch::stop_cpu()` at a
shutdown/IPI boundary and boot-time secondary CPU startup. There is no runtime
per-CPU reinitialization, task migration or interrupt-affinity migration API
that would make online/offline safe. Returning `EOPNOTSUPP` is intentional; no
asynchronous `online` completion is fabricated.

A controller-local transition lock serializes concurrent policy checks so the
last-online-CPU decision is made against a stable count. CPU requests do not
enter the generic transition: every real transition is rejected with
`EOPNOTSUPP`, so no `going-offline` or terminal event is published for an action
the platform cannot perform. `CpuControl` repeats the boot/last-CPU and
availability policy because direct callers can bypass the class controller; such
a direct generic call may still observe the generic rollback events for its
failed attempt.

## Memory state machine

Online requires a valid fixed block, no overlap with another present block,
complete `FREE` coverage, a present direct mapping and allocator acceptance.
The controller asks `ensure_direct_mapped` to establish the linear mapping. A
fully mapped range is reused; a partially mapped range returns `ResourceBusy`
because the caller cannot safely describe which subrange belongs to this
device.

It then calls `global_add_memory`. Buddy registration validates size,
alignment and all heap/metadata overlaps before mutating its region table. If
allocator addition is rejected, `MemoryControl` unmaps the range only when this
request installed it, logs a warning if that unmap fails, and returns the
allocator error. The published device state remains offline and `kdevice`
publishes the restoring `Offline` event.

Offline calls `global_remove_memory`, which verifies that every page in the
range is free, including the local PCP caches, before reserving the pages.
`AllocError::NoMemory` is translated to `ResourceBusy` because the public
allocator error set has no `Busy` variant; this preserves the required `EBUSY`
userspace result. The direct mapping is deliberately retained: removal only
reverses page availability, and keeping the linear mapping makes a later online
request cheap and keeps the range available for diagnostics. Removal never
tears down allocator metadata or the byte heap.

The local PCP is flushed before probing. Remote CPUs' PCPs are not drained, so
genuinely free pages cached on another CPU can still make offline fail as busy.
Offline is therefore only SMP-correct after those caches are quiesced; this is
a known limitation, not silently ignored.

## Event sources

The registry accepts multiple distinct `HotplugEventSource` implementations and
rejects duplicate registrations. This tree has no real ACPI general-event/AML
or virtio-mem transport, so no source is registered. Boot topology still
appears because it comes from firmware memory regions and the CPU ID map, never
from fabricated polling events.
