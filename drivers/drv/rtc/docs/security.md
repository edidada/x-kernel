# drv-rtc — Security and reliability

## Trust model and external boundaries

Firmware (DT) and board configuration are trusted to describe *where* a
clock is, not to provide a valid time: every register value is treated as
untrusted hardware input. The drivers validate samples before publishing
them, and the class service only consumes `Ok` values. The board-static
descriptors (`rtc-cmos`, `rtc-ls7a`) come from the composition root, not
from firmware. The guest fixtures in `ut-tests/common.rs` and
`ut-tests/ls7a_toy.rs` check timestamp bounds and scripted LS7A samples
without accessing real MMIO; the LS7A cases retain the backend's `ls7a`
feature gate.

## Unsafe inventory and memory safety

| Site | Unsafe operation | Invariant |
| --- | --- | --- |
| `ls7a.rs` (`MmioToy`) | volatile 32-bit read/write in the mapped aperture | `base` is the virtual address of the `IoMap` held by `Ls7aData`; register offsets stay inside the 4 KiB window; the mapping outlives every read reachable through the publication |
| `pl031.rs` | `arm_pl031::Rtc::new(*mut u32)` | pointer is the live `IoMap` base, held by `Pl031Data` for the publication's lifetime |
| `goldfish.rs` | `riscv_goldfish::Rtc::new(usize)` | same mapping-lifetime invariant |

Backend crates (`arm_pl031`, `riscv_goldfish`, `x86_rtc`) encapsulate their
own volatile access. No driver holds a lock across a read, registers an
interrupt, or performs DMA; `read_time` never sleeps.

## Threats and failure handling

| Failure | Effect | Control |
| --- | --- | --- |
| Faulting register block | Garbage time | LS7A rejects out-of-range fields, PL031/CMOS/Goldfish reject epochs above `i64::MAX` |
| Torn LS7A read across new-year | Wrong year paired with a new-year date | `read_year_consistent` re-samples; a persistently unstable year yields `KError::Io` |
| Node matched via generic `arm,primecell` that is not a PL031 | Wrong driver claims the device | Probe checks `arm,primecell-periphid`; a mismatch returns `Fatal` and the domain tries the next candidate |
| Descriptor port range disagrees with the CMOS pair | Reading an unowned port | CMOS probe refuses to publish on mismatch |

The mapping is released only after the class publication is withdrawn (field
order in `Data`), so no consumer can observe a device whose registers were
unmapped. Failure to map or publish is a probe error and releases any
already-acquired guard through ordinary drop.

## Known limitations

- No alarm, wakeup, or periodic-interrupt support; the trait is read-only.
- CMOS uses the architecture-fixed port pair described above.
- The PL031 periphid fallback costs one failed probe attempt for other
  PrimeCell nodes; the domain records it per driver and continues.