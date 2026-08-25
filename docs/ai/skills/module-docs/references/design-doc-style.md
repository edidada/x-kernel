# X-Kernel Design Document Writing Style

Use this reference when creating or updating a crate-local `docs/design.md`.
It applies the architecture-description principles of ISO/IEC/IEEE 42010 to
the level needed for an X-Kernel crate or module. Follow the requirement
levels in the parent `SKILL.md`.

The document describes the current design. Source code and verified callers
are authoritative. Existing prose, issue discussions, and intended future
work may explain rationale, but they `MUST NOT` override implemented behavior.

## Evidence to Derive Before Writing

Derive the design from code rather than filling a prepared outline. Inspect:

- the crate entry point and complete hand-written module tree;
- major public and internal types and the state or resources they own;
- public entry points and representative callers;
- dependencies and the boundary at which work is delegated;
- initialization, normal operation, error, teardown, and drop paths;
- shared state, locks, atomics, queues, wait paths, and callbacks;
- real state transitions, algorithmic invariants, and ordering constraints;
- platform, scheduler, process, interrupt, memory-mapping, or CPU-local
  assumptions.

A design claim `MUST` be supportable by a symbol, source module, caller, or
verified project specification. Prefer stable symbol and path references over
line numbers that will become stale.

## Document Shape

Use these sections when they apply:

```text
# Crate or Module Design
## Purpose
## Background
## Scope
## Architecture
## Execution Context and Calling Constraints
## State Machine
## Core Flows and Algorithms
## Concurrency Model
## Design Decisions
## Resource Lifecycle and Cleanup
```

The structure is adaptable. Combine closely related short sections when doing
so improves clarity. Omit genuinely inapplicable sections. Do not retain an
empty heading or write `None`, `N/A`, or generic filler merely to preserve the
outline.

## Purpose

`Purpose` `MUST` identify:

- the capability the component provides within X-Kernel;
- the callers or subsystems it serves;
- the concrete problem or responsibility it addresses;
- the result or abstraction it exposes to those callers.

Keep this section short. Do not repeat the crate name as its purpose, list
source files, or use claims such as "provides efficient and secure support"
without naming the actual behavior.

## Background

`Background` `SHOULD` include only context necessary to understand why the
design has its current shape. Useful context includes a kernel semantic that
must be preserved, a hardware constraint, a compatibility requirement, or a
repository ownership boundary.

Do not write a general tutorial on file systems, scheduling, memory
management, or Rust. Omit this section when the design is understandable
without that context.

## Scope

`Scope` `MUST` define both ownership and exclusions:

- state, resources, policy, and lifecycle owned by the component;
- public behavior and source modules covered by the document;
- behavior delegated to another crate or subsystem;
- closely related behavior intentionally outside this document.

Boundaries `MUST` be phrased in terms of responsibility, not directory layout
alone. For example, state that a filesystem owns in-memory inode contents but
delegates VFS pathname traversal, rather than merely listing both crates.

## Architecture

`Architecture` `MUST` describe the smallest set of components needed to
understand the design. For every major component, state:

- its responsibility;
- the state or resource it owns;
- its inputs and observable outputs;
- the component or caller that invokes it;
- important dependencies and invariants.

When a table is clearer, use these fields:

| Component | Responsibility | Owned State or Resource | Called By | Delegates To |
|-----------|----------------|-------------------------|-----------|--------------|

After naming the components, explain the direction of control and important
data movement between them. Do not stop at a source-file inventory.

Add a diagram only when it materially clarifies three or more components, a
non-obvious control path, or ownership that is difficult to express in prose.
A diagram `MUST` agree with the accompanying text and use real component
names. It `SHOULD` show direction and boundary meaning, not decorative boxes.

## Execution Context and Calling Constraints

This section `MUST` state the conditions under which the component may be
called. Determine every applicable constraint:

- required initialization and shutdown ordering;
- early-boot, process, task, scheduler, or platform-service availability;
- process, interrupt, exception, softirq, worker, or thread context;
- whether an operation may sleep, block, allocate, retry, or invoke callbacks;
- reentrancy and recursive-entry behavior;
- CPU-local state, affinity, preemption, interrupt-enable, or pinning needs;
- required address spaces, mappings, credentials, or lock state.

Name the affected entry point or operation. Avoid global statements such as
"the crate may block" when only one path can block. When a constraint is
enforced by a caller rather than by the component, say so explicitly.

## State Machine

Include this section only when the implementation has meaningful states whose
transitions affect accepted operations or observable behavior. It `MUST`
describe:

- each state and its invariant;
- the event or call that requests a transition;
- prerequisites for the transition;
- the resulting state and externally visible effect;
- rejected transitions and their error or recovery behavior.

A useful representation is:

| Current State | Event or Entry Point | Preconditions | Next State | Observable Effect or Rejection |
|---------------|----------------------|---------------|------------|--------------------------------|

Do not invent a state machine for ordinary sequential control flow. Do not
label enum variants as states unless they actually govern lifecycle or valid
operations.

## Core Flows and Algorithms

Describe each important flow from a real entry point to its result. A flow
`MUST` include, when applicable:

1. its trigger and prerequisites;
2. validation and boundary checks;
3. delegated calls and state changes in execution order;
4. synchronization or context changes;
5. success result and externally visible effects;
6. error propagation, partial effects, rollback, and cleanup.

Name the relevant functions and types, but explain their roles rather than
copying a call graph. Include algorithm choice, complexity, retry bounds, or
fairness only when it affects correctness, scalability, or caller behavior.

## Concurrency Model

When shared state exists, this section `MUST` identify:

- the shared object or state;
- all contexts that may access it concurrently;
- the lock, atomic, ownership rule, queue, or serialization point protecting
  it;
- the scope of protection and state deliberately left outside it;
- lock ordering, callback-under-lock, and nested-acquisition rules;
- the reason for non-default atomic ordering when relevant;
- whether operations may block, sleep, or run in interrupt context;
- lifetime rules that prevent use-after-free during concurrent access.

Use a table when several shared objects use different protection:

| Shared State | Concurrent Accessors | Protection | Ordering or Context Constraint |
|--------------|----------------------|------------|--------------------------------|

Do not write only that Rust types or a lock make the component thread-safe.
Explain what invariant the synchronization preserves. If the component has no
shared mutable state, one evidence-based sentence is sufficient and the
remaining concurrency detail may be omitted.

## Design Decisions

Record a decision only when an alternative was plausible or the choice has a
non-obvious consequence. Each decision `SHOULD` contain:

- `Context`: the constraint or problem that required a choice;
- `Decision`: the implemented choice;
- `Rationale`: why it satisfies the constraint;
- `Alternatives`: credible options considered, if known;
- `Tradeoff`: cost, limitation, or consequence accepted.

Do not invent rejected alternatives to make the section look complete. Do not
present a future proposal as the current decision.

## Resource Lifecycle and Cleanup

When the component owns resources, this section `MUST` trace:

1. allocation or acquisition;
2. initialization and publication;
3. normal use and ownership transfer;
4. ordinary release or `Drop` behavior;
5. cleanup after partial initialization or failure;
6. behavior when external references, callbacks, or asynchronous work remain.

State who owns each resource at each stage. Identify leaks, deferred cleanup,
intentional retention, and caller-owned cleanup as implemented behavior or
known limitations rather than silently assuming successful release.

## Cross-Document Boundaries

Keep the design document at architecture level:

- rustdoc owns the exact contract of a public item, including its errors,
  panics, safety obligations, and usage examples;
- `design.md` owns component relationships, flows, state, concurrency,
  rationale, and lifecycle;
- `security.md` owns threats, unsafe traceability, security invariants,
  controls, residual risks, FMEA, and known security limitations.

Reference the owning document or symbol instead of copying the same contract.
Brief overlap is acceptable only when it is needed to make an architectural
flow understandable.

## Reference Basis

- [ISO/IEC/IEEE 42010:2022](https://www.iso.org/standard/74393.html) provides
  the architecture-description concepts behind the focus on scope, concerns,
  relationships, and rationale.

Apply those concepts at crate scale. X-Kernel does not require conformance to
the full standard or a particular architecture-description language.
