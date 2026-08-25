---
name: module-docs
description: Create or remediate English design, security, and Rust API
  documentation for X-Kernel crates and modules.
---

# Module Documentation

Use this skill to create or remediate documentation for an X-Kernel crate or
module. It defines what to write, how to derive the content from code, and how
to organize crate docs, module docs, public API rustdoc, design documents, and
security documents.

## Goal

Produce documentation that is:

- written in English;
- colocated with the crate or API it documents;
- derived from current code instead of generic template text;
- clear about purpose, ownership, usage, constraints, and safety;
- updated together with behavior or contract changes.

## Requirement Keywords

The keywords `MUST`, `MUST NOT`, `SHOULD`, `SHOULD NOT`, and `MAY` are to be
interpreted as described by RFC 2119 and RFC 8174:

- `MUST` and `MUST NOT` are mandatory writing requirements.
- `SHOULD` and `SHOULD NOT` may be overridden only for a stated reason.
- `MAY` marks an optional practice.

Do not silently weaken a `MUST` requirement. State any justified exception in
the task result.

## Scope

This standard covers:

- crate-level and module-level rustdoc;
- rustdoc for every publicly reachable API item, including but not limited to
  modules, types, traits, functions, methods, associated items, public fields,
  enum variants, type aliases, constants, statics, macros, and re-exports;
- crate-local `docs/design.md`;
- crate-local `docs/security.md`, including threat and failure analysis.

Apply these requirements to first-party X-Kernel crates. Exclude vendored
dependencies, generated code, build output, and imported third-party source
unless X-Kernel explicitly owns and maintains that source.

## Task Boundary

Before editing, define the target crates or modules from the user's request
and repository context. Do not silently expand a module-level task into a
crate- or repository-wide rewrite.

A crate-level remediation `MUST` cover:

- crate-level rustdoc;
- the complete hand-written module tree, including modules disabled by the
  active configuration. Any intentionally excluded architecture or feature
  variant `MUST` be reported in the task result;
- every publicly reachable API in the crate;
- non-obvious internal contracts, unsafe assumptions, and cross-module
  invariants;
- the applicability and current content of crate-local `docs/design.md` and
  `docs/security.md`.

For a narrower module-level task, inspect surrounding declarations and callers
only as far as needed to document the target accurately. Documentation debt
outside the declared target is not part of that task and `SHOULD` be reported
separately rather than edited without authorization.

## Documentation Standard

### MUST

- All new or remediated explanatory prose `MUST` be written in English.
- Every first-party crate `MUST` have crate-level rustdoc, normally written
  with `//!` in `src/lib.rs` or `src/main.rs`.
- Every hand-written source module `MUST` have module-level rustdoc. Document
  either the `mod` item with `///` or the module body with `//!`; do not
  duplicate both.
- Crate and module documentation `MUST` explain what the component does, what
  it owns, and how callers or sibling components use it.
- Every publicly reachable API item `MUST` have rustdoc that states its purpose
  and public behavior.
- Public API rustdoc `MUST` describe caller obligations, invariants, side
  effects, and execution-context constraints when applicable.
- A public API returning `Result` `MUST` document meaningful failure
  conditions under `# Errors`.
- A public API that may panic under supported use `MUST` document the trigger
  conditions under `# Panics`.
- Every `unsafe fn` and `unsafe trait` `MUST` have a `# Safety` section that
  states the complete contract callers or implementers must uphold.
- Documentation `MUST` match the current code and verified specifications.
- A crate `MUST` maintain `docs/design.md` when its behavior cannot be fully
  explained by crate-level rustdoc alone, including when it has multiple
  cooperating components, owned state or lifecycle, concurrency, non-trivial
  algorithms, or execution-context constraints.
- A crate `MUST` maintain `docs/security.md` when it has unsafe behavior,
  security or thread-safety invariants, external or partially trusted input,
  privileged operations, or failure modes that can materially affect the
  kernel or another subsystem.
- All template placeholders `MUST` be removed before completion.

### SHOULD

- Crate and module docs `SHOULD` point readers to the main public types and
  entry points instead of listing every item mechanically.
- Each rustdoc item `SHOULD` begin with a concise one-sentence summary.
- Core public APIs `SHOULD` include a practical `# Examples` section. Examples
  `SHOULD` be doctests when the X-Kernel build environment permits it.
- `# Arguments` and `# Returns` `SHOULD` be added only when the signature and
  main description do not already make the contract clear.
- Documentation `SHOULD` explain why important designs exist and record
  significant tradeoffs.
- The first meaningful mention of a main public type or entry point `SHOULD`
  use an intra-doc link when the link materially improves navigation. Later
  mentions normally use plain code formatting.
- English prose `SHOULD` be concise, grammatical, and consistent with code
  terminology.

### SHOULD NOT

- Documentation `SHOULD NOT` repeat signatures or restate code line by line.
- The same API contract `SHOULD NOT` be duplicated in rustdoc, `design.md`, and
  `security.md`.
- Writers `SHOULD NOT` add empty sections or generic claims merely to satisfy
  a template.
- Obvious private helpers `SHOULD NOT` receive mechanical, low-value rustdoc.
- Examples `SHOULD NOT` demonstrate syntax without explaining realistic use.
- Crate and module introductions `SHOULD NOT` link every identifier, repeat a
  link for the same item, or substitute a mechanical API link list for an
  explanation of purpose and usage.

### MUST NOT

- Documentation `MUST NOT` invent behavior, architecture, threats,
  mitigations, or safety guarantees.
- Documentation `MUST NOT` leave stale names, paths, line references, or
  template placeholders.
- Proposed hardening `MUST NOT` be presented as already implemented.
- Known caller obligations or unsafe preconditions `MUST NOT` be hidden behind
  vague wording.
- Formal X-Kernel documentation `MUST NOT` contain untranslated Chinese prose;
  identifiers, proper names, specification terms, paths, and necessary
  quotations may retain their original spelling.

Non-obvious `pub(crate)` and `pub(super)` contracts `MUST` be documented at the
narrowest useful location. Private helpers `MAY` omit rustdoc when their
behavior is obvious, and test-only modules `MAY` use one brief purpose
sentence. Non-obvious private invariants, unsafe assumptions, and design
decisions still `MUST` be documented at the narrowest useful location.

## Output Layout

Keep crate-level design and security documentation under the crate directory:

```text
<crate>/
|-- src/
|-- Cargo.toml
`-- docs/
    |-- design.md
    `-- security.md
```

A trivial re-export or declaration-only crate `MAY` omit an inapplicable
`design.md` or `security.md`. State the reason in the task result instead of
creating an empty document.

## Source Material

Use these sources in priority order:

1. the current crate source and its actual callers;
2. existing crate-local documentation;
3. repository-local examples such as `util/klazy/docs/`, for structure only;
4. this skill and the normative local references below.

Current code is the source of truth. Existing documentation and examples
`MUST NOT` override actual behavior.

## Documentation Update Triggers

Review the owning document when a code change affects its subject:

| Code Change | Documentation to Review |
|-------------|--------------------------|
| Public behavior, errors, panic conditions, safety obligations, side effects, or caller constraints | The affected API rustdoc |
| Crate or module addition, removal, ownership, responsibility, entry points, or usage path | Crate-level or module-level rustdoc |
| Architecture, component roles, critical flows, state, concurrency, lifecycle, or cleanup | `docs/design.md` |
| Trust boundaries, unsafe paths, security invariants, threats, failure handling, or residual risk | `docs/security.md` |

An internal mechanical refactor that changes none of these documented
contracts normally does not require rewriting unaffected documentation.

## Document Responsibilities

### Rustdoc in `src/`

Keep API-level documentation in rustdoc:

- crate and module purpose, scope, ownership, and usage;
- public API semantics and caller obligations;
- parameters and return contracts when non-trivial;
- errors, panics, and side effects;
- unsafe contracts and invariants;
- practical examples for core APIs when feasible.

Before creating or updating rustdoc, read
[Rustdoc Writing Style](references/rustdoc-style.md). It turns the writing
patterns used by Rust `std` and `core` into concrete X-Kernel rules for crate
and module introductions, each kind of public item, contract sections,
examples, and intra-doc links.

Do not duplicate line-level API contracts in crate-local Markdown.

#### Crate and module documentation

Write crate and module docs as a reader-oriented introduction, following the
organization and level of precision used by Rust `std` and `core`. Do not copy
their wording or force every module into the same headings.

Crate-level docs `MUST` cover the following information when applicable. They
`SHOULD` normally present it in this order, but `MAY` use another order when it
materially improves clarity:

1. an opening paragraph that identifies the crate's role in X-Kernel;
2. its responsibilities, owned state or resources, and explicit boundaries;
3. the main public types, traits, or entry points and how they relate;
4. the normal usage path, including the order in which callers use those entry
   points;
5. important execution-context, lifecycle, concurrency, or safety constraints.

Module-level docs `MUST` explain:

- the module's responsibility within its parent crate;
- the state, resources, or abstraction it owns;
- how its parent, sibling modules, or external callers use it;
- its main entry points and non-obvious constraints.

"How to use it" does not always require compilable example code. For internal
kernel modules, concrete prose that names the entry points, expected call
sequence, required initialization, and relevant context is sufficient. Use a
`# Examples` section when code can demonstrate the real contract without
inventing unavailable setup.

Use this as an adaptable shape, not as text to copy mechanically:

```rust
//! Provides <specific capability> for <X-Kernel subsystem or callers>.
//!
//! This crate owns <state/resources> and does not own <important boundary>.
//! Callers normally start with [`MainType`], call `entry_function`, then
//! <describe the real call sequence and result>.
//!
//! <State initialization, execution-context, blocking, concurrency, or safety
//! constraints when they affect correct use.>
```

Use intra-doc links selectively. Link the first meaningful occurrence of a
main public type or entry point, then use code spans for repeated mentions.
Private implementation details normally use code spans. Describe component
relationships in prose instead of emitting a linked inventory of modules and
items.

#### Public API documentation

For each publicly reachable item:

1. `SHOULD` begin with a one-sentence summary of its observable purpose;
2. `MUST` explain behavior that is not evident from the name and signature;
3. `MUST` state preconditions, side effects, ownership or lifetime effects, and
   execution-context constraints when applicable;
4. `MUST` describe the meaning of the return value when it is non-obvious;
5. `MUST` or `SHOULD` add `# Errors`, `# Panics`, `# Safety`, and `# Examples`
   according to the requirement level defined above.

Document each meaningful `Result` failure by condition rather than merely
listing error variants. In `# Panics`, name the supported-use condition that
causes the panic. In `# Safety`, state every obligation required for soundness,
including pointer validity, aliasing, lifetime, initialization, alignment,
concurrency, and execution-context obligations when relevant.

### `docs/design.md`

Use `design.md` for:

- purpose, scope, and ownership;
- architecture and component relationships;
- execution context and calling constraints;
- state machines, algorithms, and critical flows;
- concurrency and synchronization model;
- design decisions and tradeoffs;
- resource lifecycle and cleanup behavior.

Before creating or updating this document, read
[Design Document Writing Style](references/design-doc-style.md). It defines
how to derive and write each section, describe components and flows, record
execution and concurrency constraints, and separate design decisions from
implementation inventories.

Delete inapplicable sections instead of filling them with `None` or generic
text. Keep API-level contracts in rustdoc and security-specific analysis in
`security.md`.

### `docs/security.md`

Use `security.md` for:

- trust boundaries and external attack surfaces;
- unsafe code and memory-safety invariants;
- thread-safety conditions;
- concrete threats, mitigations, and residual risks;
- failure modes and effects analysis (FMEA);
- failure handling, privacy considerations, and known limitations.

Before creating or updating this document, read
[Security Document Writing Style](references/security-doc-style.md). It
defines how to derive trust boundaries, unsafe invariants, threats, controls,
residual risks, and operational failure modes from code evidence.

Delete genuinely inapplicable sections instead of inventing risks. Existing
controls, caller-dependent assumptions, proposed hardening, and known gaps
`MUST` be clearly distinguished.

## Workflow

1. Define the target crates or modules and keep unrelated documentation debt
   outside that boundary.
2. For a crate-level remediation, recursively enumerate the complete
   hand-written module tree, including modules disabled by the active
   configuration, and record any intentionally excluded architecture or
   feature variant.
3. Inventory publicly reachable APIs and identify non-obvious internal
   contracts, owned state, invariants, unsafe paths, and cross-crate boundaries
   that affect the target documentation.
4. Inspect important implementation paths, especially unsafe, concurrent,
   stateful, hardware-facing, and externally reachable code.
5. Inspect representative callers to verify usage and execution context.
6. Identify which target documents or API items require creation or correction
   before writing prose.
7. Read `references/rustdoc-style.md`, then create or update crate-level,
   module-level, and public API rustdoc using its item-specific rules.
8. When `docs/design.md` applies, read `references/design-doc-style.md` and
   create or update the design document.
9. When `docs/security.md` applies, read `references/security-doc-style.md` and
   create or update the security and reliability analysis.
10. Perform the author self-check below.

## Author Self-Check

Before declaring the writing task complete:

- confirm that all required documentation within the declared target has
  meaningful content and that any omitted design or security document has a
  stated reason;
- re-check names, paths, entry points, behavior, caller descriptions, and
  contract sections against current code and verified callers;
- remove unsupported claims, stale references, untranslated prose, empty
  sections, duplication, mechanical filler, and template residue;
- keep future hardening distinct from implemented controls;
- report material exceptions to `SHOULD` and every exception to `MUST` in the
  task result.

The task result `SHOULD` briefly name the declared target, state whether the
complete module tree and public API surface were inventoried, list the rustdoc,
design, and security documentation changed, explain any omitted applicable
document, and note remaining out-of-scope debt or policy exceptions. Keep this
summary concise.

This self-check applies only to documentation within the declared target.

## Normative X-Kernel Requirements

The requirements in this `SKILL.md`, the following local writing references,
and applicable repository architecture and coding policies define the
X-Kernel documentation standard:

- [X-Kernel Rustdoc Writing Style](references/rustdoc-style.md) is the local,
  operational rustdoc standard.
- [X-Kernel Design Document Writing Style](references/design-doc-style.md) is
  the local standard for architecture and design descriptions.
- [X-Kernel Security Document Writing Style](references/security-doc-style.md)
  is the local standard for threat, safety, and reliability analysis.

## Informative External References

The following sources guide organization, terminology, and writing style. They
do not directly define X-Kernel-specific coverage requirements:

- [RFC 2119](https://www.rfc-editor.org/rfc/rfc2119) and
  [RFC 8174](https://www.rfc-editor.org/rfc/rfc8174) for requirement keywords.
- [The Rustdoc Book](https://doc.rust-lang.org/rustdoc/how-to-write-documentation.html)
  for crate, module, item, summary, example, and panic documentation
  conventions.
- [The Rust API Guidelines documentation checklist](https://rust-lang.github.io/api-guidelines/documentation.html)
  for crate-level docs, examples, failure conditions, safety requirements,
  and intra-doc links.
- Rust `std` and `core` documentation, especially
  [`std::path`](https://doc.rust-lang.org/std/path/index.html),
  [`core::ptr`](https://doc.rust-lang.org/core/ptr/index.html), and
  [`core::mem::MaybeUninit`](https://doc.rust-lang.org/core/mem/union.MaybeUninit.html),
  as style references rather than templates.
- [ISO/IEC/IEEE 42010:2022](https://www.iso.org/standard/74393.html) for
  architecture-description concerns, boundaries, viewpoints, and rationale.
- [The Rustonomicon](https://doc.rust-lang.org/nomicon/) for unsafe boundaries
  and soundness reasoning.
- [NIST SP 800-30 Rev. 1](https://csrc.nist.gov/pubs/sp/800/30/r1/final) for
  threat, impact, mitigation, and residual-risk analysis.
- [CWE](https://cwe.mitre.org/) for established software and hardware weakness
  terminology when a precise mapping is useful.
- [IEC 60812:2018](https://webstore.iec.ch/en/publication/26359) for systematic
  failure-mode and effects analysis.

Do not copy external documentation mechanically or claim conformance that the
project has not established. Apply relevant principles to the actual X-Kernel
code and normative local policy.
