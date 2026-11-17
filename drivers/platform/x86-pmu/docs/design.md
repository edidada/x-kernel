# x86 periodic PMU hardware

`x86-pmu` owns vendor-specific register sequences only. It has no IRQ, task,
memory-management or kernel allocation dependency. The platform integration
owns CPU pinning, counter reservation, callback publication and LVTPC routing.

CPUID probes reject unknown vendors, unsupported AMD families and missing
architectural fixed counters. Intel uses fixed counter 2 (unhalted reference
cycles), width from CPUID.0AH, and its bit in GLOBAL_STATUS/OVF_CTRL. AMD uses
CPU-cycle event 76h, either legacy four-counter registers or PERFCTR_CORE's
extended bank; PerfMonV2 additionally validates the advertised counter count
and uses its global control/status registers. Unknown capability combinations
return unavailable before any MSR access.

The initial integration exclusively reserves the local PMU: vacancy rejects
active global/fixed controls or another programmed event selector. It does not
steal a counter from perf or multiplex consumers. Once reserved, the platform
initializes the preload with counting stopped, publishes its callback/action,
enables LVTPC and starts counting. Native entry claims only its own overflow,
stops the producer, invokes KIRQ dispatch, then reloads/clears/restarts only if
the callback has not terminally quiesced it. Intel status clearing names only
fixed counter 2; AMD names its reserved counter, with sign-bit overflow
recognition on pre-PerfMonV2 parts.

Preloads clamp to a nonzero budget smaller than half the hardware width, so
large periods cannot wrap into zero or evade AMD's sign-bit classification.
The platform currently converts periods using its validated TSC frequency.
Intel's reference-cycle source follows that clock closely; AMD core cycles
vary with frequency, so the requested interval is an estimate. Watchdog checks
monotonic elapsed heartbeat time, not the number or exact spacing of PMIs.

Hardware references: Intel SDM volume 3, architectural performance monitoring
(fixed-function counters, CPUID.0AH and IA32_PERF_GLOBAL_*); AMD64 APM volume 2,
performance monitoring and CPUID.80000001H/80000022H. The corresponding Linux
paths are arch/x86/events/intel/core.c and arch/x86/events/amd/core.c.

Pure guest tests validate unsupported Intel capability rejection, preload
arithmetic and AMD bank selection. PMU availability depends on CPU/hypervisor;
TCG's native NMI tests do not establish actual PMI delivery.
