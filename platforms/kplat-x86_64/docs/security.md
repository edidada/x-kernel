# x86 NMI ownership and failure handling

NMI entry must be installed before advertising Hardware mode or enabling any
source. Source actions are registered before publication/enable. Local setup
rejects NMI callers and uses one CPU/IRQ exclusion scope. Data read in NMI is
immutable or atomic, with release callback/armed publication and acquire readers.
No NMI handler takes registry/Line/management locks or allocates.

The PMU path trusts validated CPUID and exclusive vacancy. It masks LVTPC before
configuration and retains hardware/callback state after incomplete teardown.
The peer path owns permanent timer storage and publishes pending bits before
NMI submission. Native IPI pending state can coalesce; neither submitted IPI nor
a successful queue operation proves target completion. Shutdown closes starts,
observes synchronous local completion on every participating CPU and retains
failed work for retry. Missing CPUs are not certified stopped by fatal reports.

Periodic source and explicit report notification have separate KIRQ actions.
Only an owned PMU overflow or published peer request invokes the periodic action.
Notification can still work without a PMU in entry tests. Producer completion
must check whether a consumer quiesced the source and must not resurrect it.

Peer timers require at least two boot CPUs. If every observer loses ordinary
interrupt progress, no periodic notification is generated. This limitation is
explicit in Kconfig and the QEMU config; the PMU path is the independent local
sampling option. Tests on TCG cannot certify hardware PMU behavior or x2APIC.

Panic, page fault or debug exceptions inside NMI are terminal: kcpu stops the
CPU without returning through an exception IRET or acquiring ordinary logging
locks. The report path prints raw saved state and retained stacks only.
