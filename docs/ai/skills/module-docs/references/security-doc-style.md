# X-Kernel Security Document Writing Style

Use this reference when creating or updating a crate-local
`docs/security.md`. It defines how to write a code-grounded threat, safety, and
reliability analysis. Follow the requirement levels in the parent `SKILL.md`.

This document records implemented controls and current residual risk. It is
not a generic security overview and `MUST NOT` turn recommendations into
claims about existing protection.

This is a code-level threat, safety, and reliability analysis, not a complete
NIST SP 800-30 risk assessment. It does not estimate likelihood or derive an
organizational risk rating unless the task explicitly requires a defined
method and sufficient evidence.

## Evidence to Derive Before Writing

Inspect enough code and callers to identify:

- protected assets, privileged operations, and security properties;
- trusted, partially trusted, and untrusted actors or components;
- entry points for user, device, firmware, network, file-system, IPC, and
  other external input;
- validation, normalization, authorization, isolation, and bounds checks;
- unsafe functions and blocks, FFI, assembly, MMIO or PIO, DMA, raw pointers,
  mappings, and lifetime-sensitive operations;
- shared state, synchronization, callbacks, teardown, and race-sensitive
  paths;
- error propagation, panic, retry, partial update, cleanup, and resource
  exhaustion behavior;
- known missing enforcement, caller-dependent guarantees, and intentionally
  unsupported protections.

Every material security claim `MUST` be traceable to a symbol, source module,
caller, or verified specification. Prefer symbol and path references. A line
number `MAY` be included when useful, but it `MUST` be maintained when code
moves.

## Threats and Failures Are Different

A threat involves an actor or externally influenced condition that can violate
a security property. A failure mode is an operational way the component can
fail even without malicious intent.

- Put crafted input, privilege bypass, confused ownership, malicious device
  behavior, and attacker-triggered resource exhaustion in `Threat Analysis`.
- Put allocation failure, device timeout, corrupted internal state, cleanup
  failure, and unavailable dependencies in FMEA.
- Record an issue in both sections only when its adversarial and accidental
  forms have materially different triggers, effects, or handling.

Do not treat the presence of `unsafe` as a threat by itself. The threat is the
way an invariant could be violated and the resulting impact.

## Document Shape

Use these sections when they apply:

```text
# Crate or Module Security and Reliability Analysis
## Trust Model
## External Boundaries and Attack Surface
## Unsafe Code Inventory
## Memory-Safety Invariants
## Thread Safety
## Threat Analysis
## Failure Modes and Effects Analysis
## Failure Handling
## Privacy
## Known Limitations
```

Combine closely related short sections when that improves clarity. Omit
genuinely inapplicable sections instead of inventing content. If the absence
of a risk is important, give one evidence-based sentence rather than a generic
claim.

## Trust Model

`Trust Model` `MUST` identify:

- assets and the confidentiality, integrity, availability, isolation, or
  memory-safety property that must be preserved;
- trusted components and the exact behavior trusted from them;
- partially trusted or untrusted actors, components, and inputs;
- caller obligations and environmental assumptions on which protection
  depends.

Avoid saying only that "the kernel is trusted" or "users are untrusted."
Describe which boundary matters to this component. A useful asset view is:

| Asset or Property | Required Protection | Trusted Enforcer | Relevant Untrusted Influence |
|-------------------|---------------------|------------------|------------------------------|

An assumption enforced only by a caller `MUST` be labeled caller-dependent.
It `MUST NOT` be described as validation performed by the component.

## External Boundaries and Attack Surface

Map each external or partially trusted input to the code that first handles
it. Include user memory and arguments, files, network packets, IPC, device
input, interrupts, MMIO or PIO, DMA, firmware, boot metadata, FFI, and
cross-subsystem callbacks as applicable.

Each boundary `MUST` state:

- the input or operation and its source;
- the receiving entry point;
- the state, resource, or privileged action it can influence;
- validation, authorization, copying, isolation, or serialization performed;
- behavior when the check fails;
- assumptions that remain outside the component.

Use this representation when several boundaries exist:

| Input or Operation | Source | Entry Point | Influence | Implemented Validation or Isolation | Failure Behavior |
|--------------------|--------|-------------|-----------|-------------------------------------|------------------|

Do not list a technology as an attack surface without explaining how data or
control crosses the boundary.

## Unsafe Code Inventory

Each important unsafe path `MUST` identify:

- the function or block by symbol and source path;
- the operation that requires unsafe code;
- the exact invariant relied upon;
- why the operation is sound while that invariant holds;
- the safe wrapper, guard, validation, or caller contract enforcing it;
- the consequence if the invariant is violated.

Use a table when the crate has several independent unsafe paths:

| Symbol and Path | Unsafe Operation | Required Invariant | Enforcing Guard or Caller | Violation Effect |
|-----------------|------------------|--------------------|-----------------------------|------------------|

Group unsafe blocks only when they share the same invariant and enforcement
path. If the crate contains no unsafe code, state the result briefly; do not
claim that this alone eliminates memory-safety or logic risks.

## Memory-Safety Invariants

State invariants as conditions that must remain true. Cover applicable
validity, alignment, provenance, initialization, bounds, aliasing, lifetime,
ownership, mapping, pinning, and integer-size conditions.

For each invariant, explain:

- which objects or operations it protects;
- when the invariant is established;
- which code maintains or checks it;
- what can violate it;
- the effect of violation.

Do not write only that Rust or the type system guarantees memory safety. Name
what remains outside compiler enforcement.

## Thread Safety

When state is shared, this section `MUST` explain:

- concurrent accessors and execution contexts;
- synchronization or ownership mechanism;
- protected data and lock or atomic scope;
- ordering, reentrancy, callback, and teardown constraints;
- relevant `Send` and `Sync` assumptions;
- race, deadlock, stale-reference, or use-after-free consequences if the
  constraint is violated.

Distinguish prevention implemented by the component from ordering guaranteed
by its caller. Do not infer thread safety merely from the presence of a lock or
an automatic `Send` or `Sync` implementation.

## Threat Analysis

Derive threats from assets and boundaries using this chain:

1. identify the actor or externally influenced source;
2. identify the reachable entry point and triggering condition;
3. identify the invariant, authorization rule, or security property at risk;
4. state the local and system-level impact;
5. identify the control that the current code actually implements;
6. state what risk remains after that control.

Every threat entry `MUST` include a concrete trigger, impact, implemented
control, and residual risk. Add a CWE identifier only when it precisely names
the underlying weakness; do not force every threat into a CWE category.

Stop when the evidence does not establish an externally influenced source, a
reachable entry point, a concrete trigger, and material impact. A hypothetical
future coding regression is normally an invariant or review concern, not a
current threat. Simple crates `MUST NOT` manufacture threat entries to fill a
table.

Use a table when there are multiple threats:

| ID | Asset or Boundary | Threat and Trigger | Severity | Implemented Control and Evidence | Residual Risk | Recommended Hardening |
|----|-------------------|--------------------|----------|----------------------------------|---------------|-----------------------|

Use the following X-Kernel-local impact-severity scale consistently. It
describes impact only and is not a NIST likelihood-and-impact risk rating:

- `High`: may violate memory safety or isolation, gain privilege, corrupt
  persistent or kernel-wide state, or cause an unrecoverable system failure;
- `Medium`: may cause controlled denial of service, resource exhaustion,
  information exposure, or incorrect behavior with limited scope;
- `Low`: has minor, recoverable, or tightly contained effects.

Severity does not describe implementation effort. If likelihood is materially
uncertain, state that uncertainty in residual risk instead of inventing a
probability.

An implemented control `MUST` name the actual validation, type, lock, bounds
check, authorization decision, or isolation boundary and its code evidence.
Do not write vague controls such as "input is validated." A recommendation
`MUST` appear only in `Recommended Hardening` or `Known Limitations`, never in
the implemented-control column.

## Failure Modes and Effects Analysis

FMEA `MUST` analyze credible operational failures separately from threats. A
failure-mode entry requires a reachable path supported by current code or a
verified dependency contract; a merely imaginable failure is not sufficient.
Simple crates `MUST NOT` manufacture failure modes to fill a table. For each
failure mode, state:

- the failed operation or lost function;
- initiating cause;
- local effect inside the component;
- effect on callers or the wider kernel;
- detection mechanism, if any;
- actual handling, containment, or cleanup;
- severity.

Use this representation:

| ID | Failure Mode | Cause | Local Effect | System Effect | Detection | Handling | Severity |
|----|--------------|-------|--------------|---------------|-----------|----------|----------|

Use the X-Kernel-local FMEA severity scale: `1` for fatal or unrecoverable
system effects, `2` for serious service or integrity effects, `3` for moderate
and recoverable effects, and `4` for minor effects. Do not calculate a
risk-priority number unless the project defines occurrence and detection
scales with evidence.

## Failure Handling

Explain how failures move across the component boundary. State when the code:

- returns or transforms an error;
- retries, blocks, waits, or falls back;
- rolls back or leaves a partial update;
- releases, leaks, quarantines, or defers cleanup of resources;
- logs, warns, degrades service, aborts an operation, or panics.

Name which behavior applies to which path. Do not use a general statement such
as "errors are propagated" when some paths panic or discard failures.

## Privacy

Identify sensitive, user-derived, credential, address, content, metadata, or
device-originated data handled by the component. Describe where it is stored,
how long it remains, who can observe it, and whether it appears in logs or
diagnostics.

If the component processes no such data, one evidence-based sentence is
sufficient. Do not equate the absence of network access with the absence of
privacy concerns.

## Known Limitations

Record protections that are absent, incomplete, intentionally unsupported, or
dependent on callers. Each limitation `MUST` state:

- the missing or incomplete property;
- affected operation or boundary;
- current consequence or residual risk;
- current workaround or caller obligation, if one exists;
- proposed hardening, clearly labeled as future work.

Do not hide a known gap behind optimistic wording. Do not convert a possible
future change into a guarantee.

## Claim Language

Use wording that shows the status of a claim:

- an implemented control uses present tense and names the enforcing symbol or
  operation;
- a caller-dependent assumption names the caller obligation and states that
  the component does not enforce it;
- a known limitation states the currently absent behavior and its consequence;
- a recommendation is labeled `Recommended hardening` and describes a future
  action rather than an existing guarantee.

If code evidence is inconclusive, narrow the claim or state the uncertainty.
Do not guess that a control exists.

## Cross-Document Boundaries

Keep detailed public API conditions in rustdoc. Keep component architecture,
normal flows, and design rationale in `design.md`. Use `security.md` for the
security consequence of those behaviors, unsafe traceability, threats,
controls, residual risk, operational failures, and limitations.

Reference symbols or the design document instead of copying their prose.

## Reference Basis

- [NIST SP 800-30 Rev. 1](https://csrc.nist.gov/pubs/sp/800/30/r1/final)
  informs threat-source, impact, control, and residual-risk terminology.
- [CWE](https://cwe.mitre.org/) provides shared weakness terminology; use it
  only when a precise mapping improves the analysis.
- [IEC 60812:2018](https://webstore.iec.ch/en/publication/26359) provides the
  systematic basis for FMEA and FMECA.
- [The Rustonomicon](https://doc.rust-lang.org/nomicon/) informs unsafe-code
  and soundness analysis.

These sources guide structure and terminology. X-Kernel does not claim formal
conformance to the complete standards.
