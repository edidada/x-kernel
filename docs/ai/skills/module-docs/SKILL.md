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

- written in English for the skill-owned outputs defined below;
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

The English-language requirement applies only to crate and module rustdoc,
public API rustdoc, and crate-local `docs/design.md` and `docs/security.md`
created or remediated by this skill. It does not apply to explicitly localized
artifacts, such as `README_CN.md`, files or directories marked `-cn`, `_CN`, or
`zh-CN`, or to other files whose governing repository instructions require a
different language. Do not rewrite, reject, or treat those files as
non-compliant merely because they contain Chinese prose.

## Task Boundary

Before editing, define the target crates or modules from the user's request
and repository context. Do not silently expand a module-level task into a
crate- or repository-wide rewrite.

A crate-level remediation `MUST` cover:

- crate-level rustdoc;
- the complete first-party, hand-written module tree determined by the
  coverage procedure below, including modules not selected by the active
  configuration. Any intentional exclusion `MUST` be reported in the task
  result;
- every publicly reachable API in the crate;
- non-obvious internal contracts, unsafe assumptions, and cross-module
  invariants;
- the applicability and current content of crate-local `docs/design.md` and
  `docs/security.md`.

For a narrower module-level task, inspect surrounding declarations and callers
only as far as needed to document the target accurately. Documentation debt
outside the declared target is not part of that task and `SHOULD` be reported
separately rather than edited without authorization.

## How to Determine Coverage

Determine coverage from the declared task boundary and source ownership, not
only from the crates and modules compiled by the current Kconfig, Cargo feature
set, host architecture, or target architecture.

For a crate-level task:

1. Start from the target package manifest and the library or binary source
   roots declared by Cargo, normally `src/lib.rs` and `src/main.rs`.
2. Recursively follow file-backed and inline `mod` declarations. Resolve
   explicit `#[path = "..."]` attributes and hand-written files brought into
   the module tree by `include!` when their path can be determined.
3. Traverse declarations behind every `#[cfg(...)]` and `#[cfg_attr(...)]`
   branch without filtering them through the active configuration. This
   includes architecture-, platform-, feature-, test-, and unittest-gated
   modules.
4. Inspect the package's Cargo features, target declarations, and repository
   configuration only to discover additional maintained source roots and
   conditional branches; do not treat the currently resolved Cargo dependency
   graph as the complete source inventory.

For a subsystem- or repository-level task, first enumerate first-party package
manifests within the declared directory boundary, including packages not
selected by the current Kconfig or build graph, and then apply the crate-level
procedure to each package. Workspace membership and `cargo metadata` are useful
discovery evidence but are not sufficient reasons to exclude a repository-
maintained package under that boundary.

Include a source file when it is a maintained Cargo target root or is reachable
from one under any conditional branch. Do not automatically classify every
`.rs` file below the package as a module: report an unreferenced file as a
possible stale, orphaned, generated, or separately owned source file instead of
silently documenting or deleting it. Exclude generated, vendored, imported
third-party, and build-output files. If macro expansion or an unresolved path
prevents a reliable decision, report the ambiguity and the chosen treatment.

## Documentation Standard

### MUST

- Explanatory prose added to or remediated in the skill-owned rustdoc,
  `docs/design.md`, and `docs/security.md` outputs defined in Scope `MUST` be
  written in English.
- Every first-party crate `MUST` have crate-level rustdoc, normally written
  with `//!` in `src/lib.rs` or `src/main.rs`.
- Every hand-written source module `MUST` have module-level rustdoc. Document
  either the `mod` item with `///` or the module body with `//!`; do not
  duplicate both.
- Every module introduction `MUST` state its specific responsibility. Public,
  stateful, boundary, lifecycle, concurrency, or algorithmically complex
  modules `MUST` additionally describe applicable ownership, interactions,
  entry points, and constraints. A small private helper module `MUST NOT` be
  padded with inapplicable ownership, lifecycle, or caller-obligation text.
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
- A non-trivial crate `MUST` maintain `docs/design.md` when its behavior cannot
  be fully explained by crate-level rustdoc alone, including when it has
  multiple cooperating components, owned state or lifecycle, concurrency,
  non-trivial algorithms, or execution-context constraints.
- A crate `MUST` maintain `docs/security.md` when it has unsafe behavior,
  security or thread-safety invariants, external or partially trusted input,
  privileged operations, or failure modes that can materially affect the
  kernel or another subsystem.
- All template placeholders `MUST` be removed before completion.

### SHOULD

- Crate and module docs `SHOULD` point readers to the main public types and
  entry points instead of listing every item mechanically.
- A small, single-purpose private module `SHOULD` normally use one precise
  summary sentence. Add further paragraphs only when they communicate a real
  contract, invariant, usage sequence, or design boundary.
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
- Writers `SHOULD NOT` turn every source file into the same multi-paragraph
  introduction or force pure helper modules to claim that they own state.
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
- The English-output artifacts defined in Scope `MUST NOT` contain untranslated
  Chinese prose; identifiers, proper names, specification terms, paths, and
  necessary quotations may retain their original spelling. This rule `MUST
  NOT` be applied to explicitly localized or separately governed files outside
  those outputs.

Non-obvious `pub(crate)` and `pub(super)` contracts `MUST` be documented at the
narrowest useful location. Private helpers `MAY` omit rustdoc when their
behavior is obvious, and test-only modules `MAY` use one brief purpose
sentence. Non-obvious private invariants, unsafe assumptions, and design
decisions still `MUST` be documented at the narrowest useful location.

## Rust Comment Coverage and Placement

Classify each crate, module, or API item before writing. The project
requirement that every hand-written module has module docs is intentionally
stronger than Rust's `missing_docs` lint, which checks publicly exposed items.
It does not require the same documentation depth for every module.

| Target | Required treatment |
|--------|--------------------|
| Crate root | Use `//!` in `src/lib.rs` or `src/main.rs`; explain purpose, boundary, main integration path, and applicable constraints. |
| Public module | Use reader-oriented module docs that explain its public abstraction and how callers use it. |
| Private stateful, boundary, lifecycle, concurrency, or complex algorithm module | Use module docs that explain the responsibility and the non-obvious state, interaction, sequence, or invariant that justifies the module boundary. |
| Small private helper module | Use one precise module-summary sentence unless more information is genuinely needed. |
| Publicly reachable API item | Use `///` on the item and document its observable contract. |
| Non-obvious `pub(crate)` or `pub(super)` item | Document the cross-module contract at the item or narrowest shared abstraction. |
| Obvious private helper item | Omit rustdoc; use a normal `//` comment only when the implementation needs a reason or invariant explained. |
| Test-only module | One brief purpose sentence is sufficient. Generated, vendored, and imported third-party code remains excluded. |

Place comments according to the reader and subject:

- use inner `//!` comments at the top of a file-backed crate or module;
- use outer `///` comments on an inline module declaration or `mod` item when
  documenting at the declaration is clearer;
- document a module at only one of those locations;
- use outer `///` comments immediately before the API item they describe;
- use ordinary `//` comments next to implementation decisions, algorithms,
  lock ordering, state transitions, and invariants that do not form a caller
  contract;
- place the local `SAFETY:` justification immediately before its `unsafe`
  block; this is distinct from the `# Safety` contract on an `unsafe fn` or
  `unsafe trait`.

Do not move an implementation explanation into a module introduction merely
to make the file header longer. Keep information at the narrowest location
where its intended reader can act on it.

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

Review the document that owns the changed contract or design concern:

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

- crate and module purpose plus applicable scope, ownership, and usage;
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

Do not duplicate item-level API contracts in crate-local Markdown.

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

- the module's specific responsibility within its parent crate.

For public or non-trivial internal modules, add the following information only
when it applies:

- state, resources, policy, or abstraction owned by the module;
- interaction with its parent, sibling modules, or external callers;
- main entry points or operation sequence;
- non-obvious initialization, lifecycle, execution-context, concurrency,
  algorithmic, or safety constraints.

A small private helper module may satisfy the project module-doc requirement
with one precise sentence, for example:

```rust
//! Bounds-checked little-endian codecs for ext4 disk structures.
```

Do not add paragraphs merely to state that a pure helper owns no state, repeat
its filename, inventory its functions, or name obvious callers.

"How to use it" does not always require compilable example code. For internal
kernel modules, concrete prose that names the entry points, expected call
sequence, required initialization, and relevant context is sufficient. Use a
`# Examples` section when code can demonstrate the real contract without
inventing unavailable setup.

Use this as an adaptable example shape for generated documentation, not as
text to copy mechanically:

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
5. add `# Errors`, `# Panics`, `# Safety`, and `# Examples` when required or
   recommended above.

A simple getter, field-like accessor, or obvious constant normally needs only
a precise summary plus units, range, or sentinel meaning when those are not
obvious. It does not need a mechanical example, arguments list, return-value
section, ownership discussion, or implementation narrative.

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
- concrete threats, implemented controls, and residual risks;
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

For each step, produce the listed output before moving to the next step. Keep
outputs concise and tied to repository evidence. If a stop condition is met,
ask for clarification or report the blocker instead of writing unsupported
documentation.

1. Confirm the documentation scenario before editing.

   Classify the task as one of:
   - documenting a new crate or module;
   - remediating documentation for an existing crate or module;
   - updating documentation after a behavior, contract, or architecture change;
   - reviewing documentation for correctness.

   Required output: one sentence naming the scenario and the requested target.
   Stop when: the scenario or target cannot be inferred from the request,
   changed diff, or repository context.

2. Define the target scope from the scenario and evidence.

   Identify:
   - the primary target: the crate, module, API item, or document the task is
     actually about;
   - the trigger: new code, behavior change, API contract change, architecture
     change, safety or security change, documentation remediation, or review;
   - the required documentation surfaces: crate rustdoc, module rustdoc, public
     API rustdoc, implementation comments, `docs/design.md`, and/or
     `docs/security.md`;
   - the evidence boundary: source files, changed diff, callers, configuration
     variants, and existing docs that must be inspected to make the
     documentation accurate;
   - the exclusion boundary: related documentation debt that is outside the
     request and should be reported rather than edited.

   Scale the scope to the trigger:
   - new crates usually require crate rustdoc, module rustdoc for the initial
     module tree, public API rustdoc, and applicable design or security
     documents;
   - new modules require module rustdoc, rustdoc for introduced public APIs, and
     updates to crate-level design or security docs only when ownership,
     architecture, trust boundaries, unsafe behavior, or lifecycle changes;
   - API-only changes usually require the changed API rustdoc, plus design or
     security updates only when the documented contract changes at that level;
   - internal behavior changes require design or security updates only when they
     affect documented architecture, state, lifecycle, concurrency, unsafe
     invariants, trust boundaries, failure handling, or residual risk;
   - documentation-only tasks should not expand beyond the declared target
     unless a verified stale or false claim requires a wider correction.

   Required output: primary target, trigger, required documentation surfaces,
   evidence boundary, and exclusion boundary.
   Stop when: the necessary scope would exceed the requester-approved boundary.

3. Establish and record implementation facts before writing.

   - for new code, inspect the new crate or module entry points, public API,
     owned state, integration path, and intended callers;
   - for existing code, inspect the current implementation, existing callers,
     and existing docs;
   - for behavior changes, compare the pre-change and post-change contracts and
     identify what changed;
   - for documentation-only changes, verify each material claim against source
     code, callers, or a verified project specification.

   Record only facts needed to decide documentation content and scope; do not
   turn this step into a broad design summary.

   Required output: a short fact list tied to source paths, symbols, callers,
   diffs, or verified project specifications.
   Stop when: a material documentation claim cannot be verified.

4. Apply How to Determine Coverage.

   For a new crate or crate-level remediation, enumerate every maintained
   source root and its complete hand-written module tree across conditional
   branches, and record intentional exclusions or unresolved ownership.

   Required output: source roots, in-scope module tree, conditional branches
   considered, existing docs, and unresolved coverage or ownership decisions.
   Stop when: macro expansion, unresolved paths, or unclear ownership prevent a
   reliable coverage decision.

5. Classify the in-scope documentation subjects.

   Classify each in-scope module as public, non-trivial internal, small private
   helper, test-only, generated, vendored, or imported third-party code.
   Inventory publicly reachable APIs and identify non-obvious internal
   contracts, owned state, invariants, unsafe paths, and cross-crate boundaries
   that affect the target documentation.

   Required output: module classification, public API inventory, and non-obvious
   internal contracts or invariants that affect documentation depth.
   Stop when: public reachability or module ownership cannot be determined.

6. Inspect in-scope implementation paths.

   Inspect especially unsafe, concurrent,
   stateful, hardware-facing, and externally reachable code.

   Required output: implementation paths inspected and the relevant ownership,
   lifecycle, unsafe, concurrency, external-input, and failure-handling facts.
   Stop when: a required implementation path cannot be inspected.

7. Inspect representative callers when they exist or can be inferred.

   Verify usage and execution context.

   Required output: callers inspected, inferred integration points, execution
   context, and any caller-dependent assumptions.
   Stop when: caller behavior is required for an accurate claim but cannot be
   verified or reasonably inferred.

8. Decide the documentation changes before writing prose.

   Identify which target documents or API items require creation, correction,
   or no change.

   Required output: a write plan listing each documentation surface to update,
   leave unchanged, or report as out-of-scope debt.
   Stop when: the write plan would change documents outside the approved scope.

9. Read every applicable reference before writing.

   Read `references/rustdoc-style.md` for rustdoc,
   `references/design-doc-style.md` when `docs/design.md` applies, and
   `references/security-doc-style.md` when `docs/security.md` applies.
   Use the paired good/bad comparisons in each applicable reference to
   calibrate specificity and documentation depth. Treat their symbols and
   claims as illustrative; do not copy them into repository documentation.

   Required output: the applicable reference files read.
   Stop when: a required reference cannot be read.

10. Create or update crate-level, module-level, public API, and implementation
   comments with documentation depth proportional to the classification from
   step 5.

11. Create or update the applicable design and security documents.

12. Perform the author self-check below.

## Author Self-Check

Before declaring the writing task complete:

- confirm that all required documentation within the declared target has
  meaningful content and that any omitted design or security document has a
  stated reason;
- re-check names, paths, entry points, behavior, caller descriptions, and
  contract sections against current code and verified callers;
- remove unsupported claims, stale references, untranslated prose from the
  English-output artifacts, empty sections, duplication, mechanical filler,
  and template residue;
- confirm that small private modules were not padded into multi-paragraph
  templates and that implementation rationale remains near the relevant code;
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
- [The Rust Reference comment chapter](https://doc.rust-lang.org/reference/comments.html)
  for the distinction between ordinary comments, outer item docs, and inner
  crate/module docs.
- [The `missing_docs` lint](https://doc.rust-lang.org/rustc/lints/listing/allowed-by-default.html#missing-docs)
  for compiler-enforced public-item coverage. X-Kernel's all-module coverage is
  an additional local requirement.
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
