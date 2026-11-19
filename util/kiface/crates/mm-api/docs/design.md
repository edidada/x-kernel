# MM API Design

DmaMappingIf accepts only the two attribute transitions needed by coherent DMA:
Coherent and Normal. It cannot request arbitrary user/execute/page-table flags.
memspace owns the mapping lock, range validation and flag selection. kdma
owns allocation, device quiescence and platform sharing. No runtime adapter,
page allocator or page-table implementation is part of this API.

`set_mode` returns only after memspace completes the required kernel TLB
invalidation. The provider holds one task-context kernel address-space mutex
through the whole transition, leaving IRQs enabled for initiators and contenders. kdma must
call it without holding its allocator lock, so CPUs competing for that lock can
service the shootdown. Errors can follow partial PTE changes; the caller retains
the allocation and must not publish it to a device or recycle its pages.
