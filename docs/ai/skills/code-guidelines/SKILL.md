# Code Guidelines

Use this skill when an AI agent needs to write,
edit, or review Rust kernel code in the X-Kernel repository.

This skill is the shared entry point for code agents.
The canonical coding-rule facts live in:

- `docs/ai/code-guidelines/README.md`

This skill explains how an agent should use those rules when writing or
editing code. It must not define a second, competing copy of the same coding
standard.

## Scope

This skill covers:

- naming and API shape;
- implementation comments;
- `unsafe` usage and safety reasoning;
- visibility, modules, attributes, and workspace dependencies;
- syscall and current-context boundary semantics;
- concurrency, lock ordering, and atomics;
- error handling;
- logging;
- resource management, data structure selection,
  and performance-sensitive code review.

This skill does not replace:

- project build/run/test commands
  (`docs/ai/skills/build-workflow/SKILL.md`);
- rustdoc and per-crate design/security documentation rules
  (`docs/ai/skills/module-docs/SKILL.md`).

This skill primarily targets Rust code-writing and code review.
It also includes lightweight extension topics for:

- commit structuring and regression-test expectations;
- assembly style for `.S` and `global_asm!`
  when a code change touches them.

## Topic Files

First load the canonical rule file:

- `docs/ai/code-guidelines/README.md`

Then load the canonical rule files and supporting topic files relevant
to the change:

- `naming-and-structure.md`
- `comments.md`
- `api-design.md`
- `boundaries-and-context.md`
- `modules-and-attributes.md`
- `unsafety.md`
- `concurrency.md`
- `error-handling-and-logging.md`
- `performance-and-resources.md`
- `git-and-testing.md`
- `assembly.md`

Treat `git-and-testing.md` and `assembly.md`
as extension topics.
For ordinary Rust code changes,
the core topic files above them are the primary path.

## Writing Workflow

When writing or editing Rust kernel code:

1. Identify whether the change touches `unsafe`,
   raw pointers, FFI, uninitialized storage,
   shared state, logging, public APIs, or hot paths.
2. Load `docs/ai/code-guidelines/README.md`.
3. Load the matching canonical rule files under `docs/ai/code-guidelines/`
   and supporting topic files from this skill directory.
4. Apply the mandatory rules from the canonical rule file.
5. Re-read the patch specifically for:
   naming,
   boundary validation,
   current-context assumptions,
   `unsafe` reasoning,
   unsafe scope minimization,
   lock scope,
   error propagation,
   and logging quality.
6. If the change alters public behavior or module contracts,
   load `docs/ai/skills/module-docs/SKILL.md` for rustdoc and
   crate-local documentation requirements.

## Review Workflow

When reviewing Rust kernel code:

1. Check whether names reflect the real semantics and units.
2. Check whether comments explain intent rather than paraphrasing code.
3. If the patch touches `unsafe`, load
   `docs/ai/code-guidelines/unsafe-cn.md` and `unsafety.md`;
   apply `CG-UNSAFE-001` and `CG-UNSAFE-002`, then use
   `unsafety.md` for scenario-specific guidance.
4. Check unsafe audit surface, wrapper shape, and scenario-specific invariants.
5. Check whether raw-pointer, FFI, and `MaybeUninit`
   usage is modeled with the narrowest sound pattern available.
6. Check whether lock scope, lock order, and wake/block behavior are safe.
   For new or moved `Mutex` / `RwLock` / spin statics, also check
   `concurrency.md` lock construction (`static_lock!` vs `::new`).
7. Check whether syscall boundaries and current-context assumptions are explicit.
8. Check whether `?`, typed errors, and logging levels are used appropriately.
9. Check whether modules, visibility, macros, and attributes stay disciplined.
10. Check whether the code introduces avoidable hot-path scans,
   copies, allocations, or atomics.

## Review Checklist

Before considering a code change aligned with this skill, verify:

- names are descriptive, accurate, and unit-aware where needed;
- implementation comments explain intent;
- current-context semantics are explicit where the API depends on them;
- unsafe block proof obligations satisfy `CG-UNSAFE-001`;
- public unsafe API and unsafe impl contracts satisfy `CG-UNSAFE-002`;
- `unsafe fn` bodies do not silently rely on implicit whole-body unsafety;
- `unsafe` review uses the scenario catalog from `unsafety.md`
  instead of ad hoc reasoning;
- uninitialized storage uses `MaybeUninit<T>` or an equivalent explicit model;
- raw-pointer and FFI code states validity, alignment,
  lifetime, aliasing, and ABI assumptions where relevant;
- shared-state code has sane lock scope and lock ordering;
- static `Mutex` / `RwLock` / spin items use `static_lock!`,
  and heap locks use `::new` rather than `const_new` unless
  an untracked const initializer is required;
- boundary validation happens at subsystem edges;
- visibility changes satisfy `CG-MOD-001`;
- workspace-managed dependencies satisfy `CG-MOD-002`;
- modules, imports, and attributes stay narrowly scoped;
- error paths use `Result` and `?` rather than hidden panics;
- logging uses crate-standard prefixes and appropriate severity;
- container choices match their access pattern; queues needing middle
  removal, stable element addresses, or IRQ-safe enqueue are not built
  on growable `Vec` bookkeeping, and linked-list needs use the
  project-standard intrusive `linked_list_r4l`;
- the patch does not introduce obvious hot-path regressions.

## Canonical Human References

The repository still keeps the full human-oriented guideline corpus under:

- `docs/coding-guidelines/`
- `docs/coding-guidelines-cn/`

Those directories are background references,
not the primary execution path for this skill.
