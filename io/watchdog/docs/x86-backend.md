# x86 watchdog integration

Native NMI entry, KIRQ source dispatch, periodic source selection and report
notification are implemented. Entry uses dedicated IST stacks and exact GS-base
preservation; IRQ-masked and SWAPGS-window tests exercise real NMI IPI delivery.

The PMU option probes supported Intel/AMD capabilities and reserves local counter
ownership. Intel uses fixed reference cycles; AMD uses core cycles with legacy
or PerfMonV2 status handling. Actual PMI requires physical or virtual PMU support.
QEMU's x86 PMU is KVM-backed and is not implemented by the x86 TCG accelerator.
The local QEMU 8.0.0 guest was also tested with `-cpu max,pmu=on`; the backend
still found no usable PMU counter. This is architecture-specific: QEMU's ARM
target has a separate virtual PMU implementation usable with TCG.

`platforms/kplat-x86_64/qemu_watchdog_defconfig` explicitly selects an SMP peer
timer/NMI IPI source for x86 TCG. It can interrupt a CPU with ordinary IRQs
disabled, but cannot detect every peer stopping timer progress simultaneously.
On x86 KVM or physical hardware, select the independent PMU source instead.

The architecture changes retain periodic-source selection as mandatory. Generic
NMI registration, local action publication, context and retirement remain in
KIRQ; hardware probing/claims/producer lifetime belong to the selected source.
LAPIC startup/immutable runtime access and shared ICR admission permit NMI access
without overlapping mutable borrows or waiting on an interrupted sender.

See `arch/kcpu/docs/`, `platforms/kplat-x86_64/docs/` and
`drivers/platform/x86-pmu/docs/` for entry, lifecycle and hardware contracts.
