# VMM host backing ownership

## Scope and trust boundary

This document covers host physical backing allocation and transfer in mm/mod.rs
and second-stage table owners. It does not replace architecture VM-entry, MMIO
permission, device or virtualization security reviews. Callers supply guest range
geometry; the host allocator supplies owned, permanently mapped RAM.

## Memory lifetime invariants

GuestRamReservation owns a FrameRun for all actual backing pages. The guest-visible
range is nonzero, representable and page aligned; zero filling occurs before any
guest mapper or vCPU uses it. The reservation cannot outlive its static allocator.
VmGuestMemory declares mapper before reservation, so mapper destruction precedes
backing release. vCPU-held VM references must retain the VM for all guest hardware
use; reference counting is not a substitute for stopping hardware walkers.

GlobalPage uniquely owns safe data views. Table children transfer their held PA and
count with into_raw, without decrementing descriptor references. free_pt_page is
unsafe: its caller must supply the exact outstanding one-page transfer and prove
walker retirement, then it restores and drops GlobalPage exactly once. VMXON keeps
its transferred hold intentionally while virtualization is active. Wrong PA/count,
foreign transfers or stale restoration remain caller-contract violations.

## Unsafe paths

- reserve_guest_ram: write_bytes accesses the freshly allocated, unpublished
  mapped backing for exactly the validated visible length.
- free_pt_page: restores the original GlobalPage transfer by physical address;
  mapping conversion is confined to kalloc's backend adapter.
- architecture table installation and destruction: the existing architecture
  routines must exclude hardware/software walkers before ending child transfers.

## Failure and remaining limitations

Allocation failure returns None without exposing guest memory. Failed mapper
creation releases the reservation by RAII. This migration does not redesign partial
second-stage table construction or prove all existing VMM retirement paths. Tests
exercise reservation rounding/zeroing and inactive raw table retirement without
entering guest hardware. FrameRun teardown is per-base-page and may add release
cost for large reservations; no measured performance claim is made.
