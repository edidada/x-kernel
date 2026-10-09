# AArch64 boot: EL0 cache-access boundary

## Trust and unsafe boundary

`mmu.rs::init_mmu` trusts privileged entry state, linker addresses and initialized
boot page tables that preserve its executing code, stack and required data.
The primary and secondary assembly callers establish the exception-level and
boot-stack prerequisites before invoking it.
The UCT/UCI policy uses the existing typed register read-modify-write API and
adds no raw pointer access, assembly block or unsafe operation.
Register state is CPU-local; the existing ISB synchronizes the control change.

## Access granted

EL0 can read the implemented cache description and execute the cache operations
governed by these controls.
Page-table permissions, exception classification and other architectural trap
controls remain separate and unchanged.
This does not enable all privileged instructions, create executable mappings,
or enable the separately controlled `DC ZVA` instruction.
User code must still follow the required translation, cache and barrier rules.

Cache parameters become visible to userspace, and cache maintenance can affect
timing and contention; the change makes no side-channel isolation guarantee.
Heterogeneous systems need a separate policy for a safe common cache description.
The VHE path aliases control-register accesses and is outside the demonstrated
non-VMM regression scope; it must not be described as unaffected or validated.

## Failure analysis and review

| Failure | Cause and effect | Control or limitation |
|---|---|---|
| Valid user cache operations fault | Reset permissions disable CTR/cache access, causing SIGILL | Set named UCT/UCI fields explicitly and execute the instructions from EL0. |
| Behavior depends on CPU placement | Only the primary CPU is configured | Keep the update in the shared primary/secondary boot path. |
| Boot control ordering regresses | Unrelated SCTLR bits or the ISB are changed | Preserve read-modify-write and the original enable sequence. |
| Fault handling is accidentally masked | A workaround suppresses illegal-instruction delivery | Retain a genuine undefined-instruction negative control. |

This focused note does not audit the crate's other boot paths.
The change introduces no runtime resource or diagnostic data collection.
