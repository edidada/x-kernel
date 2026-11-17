# x86 PMU safety boundary

All PMU MSR operations are unsafe APIs requiring CPL0, the probing CPU, truthful
CPUID capabilities and exclusive hardware ownership. A capability snapshot is
not a transferable reservation. The platform serializes setup/release with CPU
and IRQ exclusion; NMI operations retain the published reservation and never
release memory. Register helpers create no mutable handle over an interrupted
caller, take no lock and allocate nothing.

Vacancy is checked before programming. No other PMU consumer may appear between
that check and release. Counter width/counts are bounded at probing, and preload
conversion uses u128 multiplication and saturation. Status tests/clears target
only the reserved producer. Counters remain stopped across initialization and
reload. The platform must stop the producer before KIRQ teardown and must not
restart it after terminal quiescence.

PMU-less virtual CPUs are an expected unavailable result. No speculative RDMSR
probe or exception-fixup is attempted. Unsupported vendors/families are rejected.
AMD sampling cadence depends on core frequency; this is not a strict wall-time
NMI timer. A halted core does not produce unhalted-cycle samples; fatal report
collection must use explicit notification and bounded waits.

Remaining validation: real Intel/AMD overflow delivery and generation-specific
PMI errata require matching physical CPUs or virtual PMUs. QEMU peer-timer
validation exercises native NMI entry, not these MSR sequences.
