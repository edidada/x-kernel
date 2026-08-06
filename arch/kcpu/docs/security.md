# kcpu security for XHyper

## Trust boundary

XHyper executes the AArch64 `kcpu` path at EL2/VHE. Exception state and fault
addresses are untrusted at trap entry and are decoded before dispatch.

## Invariants

- The exception vector address is supplied by `excp.S` and satisfies the Arm
  alignment requirement.
- `TTBR0_EL1` is cleared during trap initialization so accidental low-address
  translations are unavailable.
- EL0 timer access and user-return preparation are not enabled.
- There is no Linux exception-table recovery. An unhandled synchronous page
  fault terminates through the kernel panic path rather than rewriting the
  saved instruction pointer.
- No syscall number, Linux errno, or syscall-restart state crosses this crate's
  AArch64 API boundary.
