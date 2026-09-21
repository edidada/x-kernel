# Boundaries And Context

Use this file when the change touches syscalls,
user pointers, file descriptors, filesystem context,
or any API whose meaning depends on the current execution context.

## API Semantics

- if an API depends on a "current" resource,
  state whether it requires a current process thread
  or only a current execution path;
- do not hide current-context assumptions behind generic names;
- prefer passing validated context objects into deeper helpers
  rather than re-reading implicit global current-state repeatedly.

## Validation Boundaries

- for user pointer, user address, and user ABI carrier rules, apply
  `docs/ai/code-guidelines/context-cn.md` (`CG-CTX-001`, `CG-CTX-002`);
- validate at subsystem boundaries, then trust validated internal invariants;
- syscall frontends are the primary validation boundary for user input;
- keep nullable-pointer semantics explicit at the syscall boundary.

## ABI Separation

Apply `docs/ai/code-guidelines/context-cn.md` (`CG-CTX-002`).

## X-Kernel-Specific Context Rules

- for process-only filesystem paths,
  use process-specific context helpers;
- for helpers that may run from either a user thread
  or a kernel task, use shared execution-context helpers;
- for file descriptor operations,
  use the highest abstraction layer that satisfies the need
  instead of reaching directly into lower-level tables.

## When Reviewing

Check specifically for:

- hidden assumptions about "current process" vs "current execution path";
- violations of `CG-CTX-001` or `CG-CTX-002`;
- low-level fd-table manipulation in code that should use higher-level wrappers.
