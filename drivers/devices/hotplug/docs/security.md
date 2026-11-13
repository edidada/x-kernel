# CPU and Memory Hotplug Security

Hotplug writes change global resource availability and are restricted to
`CAP_SYS_ADMIN`. Sysfs checks the credential of the writing task at write time,
not the credential captured when the file was opened.

All write validation happens in the class state machine. Sysfs only parses a
single `0` or `1` and forwards the request; it never treats an unsupported
operation as success. In particular, the boot CPU and the last online CPU are
rejected with `EPERM` before any transition, and an absent device returns
`ENODEV`.

The class controller is the policy boundary for sysfs and event-source writes.
It rechecks the device class, serializes the policy decision with the generic
transition, and validates class-wide constraints before any hardware callback.
The generic `kdevice::set_system_device_online` API is narrower and does not by
itself enforce sysfs policy; kernel callers using it directly are responsible
for the equivalent preflight. `CpuControl` still rechecks CPU boot/last-CPU
policy, and `MemoryControl` still validates the fixed block shape, because both
controls can be reached through the public generic API.

Memory online accepts only one 128 MiB block: size must equal
`MEMORY_BLOCK_SIZE_BYTES`, the physical start must be block-aligned, and the
index must match `physical_start_bytes / MEMORY_BLOCK_SIZE_BYTES`. The class
controller additionally rejects overlap with another present memory block with
`EBUSY` and incomplete firmware `FREE` coverage with `EINVAL` before mapping or
adding pages.

Removal is refused with `EBUSY` unless every page in the block is free,
including pages cached in the local PCP, which prevents freeing memory still
owned by the kernel, a driver or a user mapping. Remote PCP caches are not
drained, so the check can conservatively report busy; it cannot silently remove
a page still cached on another CPU.

A failure after partial online work leaves the device offline and published
state consistent. If this request installed a direct mapping and allocator
addition then failed, the controller unmaps that mapping and returns the
allocator error; a failed unmap is logged but does not replace the original
error. Offline intentionally retains the direct mapping after removing pages
from the allocator: the mapping alone grants no allocator ownership, and
retaining it keeps the reserved physical range addressable for a later online
request.

No ACPI AML interpreter is present, and no event source is registered in this
tree. Event sources may only report real firmware or hypervisor notifications;
they must not poll or synthesize events. `kdevice` registry locks are
non-IRQ-safe spin locks, so all hotplug transitions run in task context and
callbacks are invoked outside those locks.
