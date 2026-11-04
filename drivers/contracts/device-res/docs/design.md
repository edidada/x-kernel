# device-res — Design

## Remaining responsibility

This is a transitional framework resource-description and cleanup facade.
It no longer defines MMIO, DMA, IRQ or time capabilities or resource guards.
All such contracts belong to driver-os-api. The deleted src/irq.rs is not
replaced by another local IRQ model.

src/lib.rs owns ResourceDesc, ResourceSet and DeviceResource. ResourceDesc is
a discovery container; its IRQ variant contains os-api's resolved IrqResource.
ResourceSet is a small vector. DeviceResource lends the discovered resources
and registers a device-owned cleanup callback.

src/provider.rs combines os-api traits into ResourceProvider and implements
devm acquisition helpers. Provider selection is explicit; this crate installs
no global provider, IRQ registry, allocator or device topology.

## Lifecycle

Each helper first acquires an os-api guard. If acquisition fails, no cleanup
callback is registered. On success it moves that guard into a cleanup closure.
The device framework runs registered closures in reverse order at probe
rollback/removal. It must stop hardware and prevent new operations before
releasing resources; LIFO cleanup alone is not a hardware-stop protocol.

IRQ helpers use DevIrq for ordinary, explicit-threaded and default-primary
registration. Unsupported threaded registration stays an error. The guard
retains the exact provider/resource/action token and synchronously releases
that action on drop. There is no duplicate armed flag or local IRQ wrapper.

MMIO helpers return a mapped pointer while cleanup owns IoMap. Coherent DMA
helpers return CPU/bus addresses while cleanup owns CoherentMem. These raw
views do not keep a device binding alive and must not escape its permitted
resource-access lifetime.

## Context and failure boundaries

Providers define acquisition context. IRQ cleanup may sleep while draining
callbacks and must run without locks needed by those callbacks. The framework
must invoke cleanup outside its own conflicting locks. DeviceResource does
not implement serialization or device liveness checks.

register_cleanup currently returns no allocation error. Closure allocation
and framework cleanup storage retain their existing allocation policy; this
migration does not claim recoverable registration OOM.

## Migration and verification

IRQ names are temporarily re-exported from os-api for remaining framework
consumers. Concrete VirtIO drivers and the xkernel provider now use os-api
directly and no longer depend on device-res. Next, move the remaining
description/cleanup integration to its framework owner and retire this crate;
do not move device identity or cleanup registries into os-api.

ut-tests/lib.rs verifies provider error propagation, no cleanup after failed
acquisition, reverse cleanup order, and ordinary/threaded IRQ release pairing.
os-api tests verify exact registration identity, provider retention and guard
failure/drop behavior. Native/RNG SMP tests verify callback-tail drain.
