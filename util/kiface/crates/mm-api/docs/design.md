# MM API Design

DmaMappingIf accepts only the two attribute transitions needed by coherent DMA:
Coherent and Normal. It cannot request arbitrary user/execute/page-table flags.
memspace owns the mapping lock, range validation and flag selection. kdma
owns allocation, device quiescence and platform sharing. No runtime adapter,
page allocator or page-table implementation is part of this API.

`set_mode` retains its optional `DmaMappingCompletion` result. The memspace
provider completes its PTE changes and required TLB invalidation while holding
the kernel task mutex, then returns `None`. Runtime calls require sleepable task
context. kdma invokes the provider without holding its IRQ-safe allocator lock.

A provider that returns a completion still requires the caller to run
`complete()` outside its IRQ-safe locks and check its result before publishing
or recycling the range. Errors can follow partial PTE changes; the caller
retains the allocation. The completion interface remains unchanged.
