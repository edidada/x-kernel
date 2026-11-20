# Platform Contract Boundaries

Providers own hardware and firmware operations. Boot phases retain IRQ masking
and once-per-CPU sequencing obligations. CPU startup accepts validated logical
IDs; translating them to hardware IDs belongs to the platform. Power terminals
do not perform filesystem/process/device cleanup.

PMU IRQ registration precedes activation. NMI source and mechanism protocols
retain their separate contexts. Local NMI preparation requires an initialized
CPU mechanism and a disabled registered source on the first call. KIRQ retains
partial-failure cleanup state; callers must not enable delivery after rejection.
The source owner still excludes counter restart during shutdown and cannot use
terminal quiescence as evidence that handlers/resources may be freed.
Reusable `shutdown_periodic_nmi` reports only the calling CPU's producer and
controller teardown. A false result retains resources; successful remote queue
submission is not a successful shutdown result. The owner must serialize restart
and observe completion from every participating CPU before shared handler removal.
`PmuInterruptIf::shutdown_overflow_irq` delegates that global responsibility to
the PMU owner. Consumers must authorize losing periodic service and exclude CPU
hotplug/system stop. Providers retain partial failure and block restoration until
all source/controller acknowledgements and software-reader retirement finish.
`restore_overflow_irq` republishes only the handler, not old consumer callbacks.
MMIO/DMA preparation errors must be handled by
their callers; moving or renaming contracts does not certify rollback on
partial platform sharing failure. The existing AArch64 partial-sharing error
policy is tracked separately in kiface's capability audit.

## x86 system sleep

The unsafe caller/provider boundary is `sys::SystemSleepIf` in
`src/sys.rs`. The x86 implementation lives in `kplat-x86_64/src/suspend.rs`.
The PM coordinator owns exclusion and lifetime across these calls.

- `prepare` runs on the boot CPU. Serialize PM and exclude CPU hotplug until
  `finish` releases the wake resources.
- `park_cpu` runs on the target CPU with local IRQs and migration disabled.
  These exclusions and usable devices are required through the resumed
  return of `SystemSleepIf::park_cpu`, including the abort path. Keep both
  the `extern "C"` callback and the storage addressed by its raw argument
  valid for that entire call. The callback returning or acknowledging alone
  does not release the caller's lifetime obligation.
- `enter` requires other CPUs, devices, NMI sources, IRQs and clocks to be
  quiesced before firmware entry on the boot CPU.
- `restore_cpus` requires boot CPU IRQs to remain disabled and shared resources
  to remain excluded while saved remote contexts restart.
- `finish` requires all remote CPU callbacks to have joined before releasing
  ownership. A timeout alone does not establish that join.

`persistent_time` is a safe optional clock read. The forwarding import
`khal::power::SystemSleepIf` exposes the same contract to existing callers.
