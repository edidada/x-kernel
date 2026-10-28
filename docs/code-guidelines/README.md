# Code Guidelines

Machine-checkable code rules, one rule per file, each with the human
explanation. The YAML frontmatter is read by tooling such as xreview; the body
is written for people. English only.

## Frontmatter

| Field | Meaning |
| --- | --- |
| `id` | `CG-<nnn>-<descriptive>`; equals the filename without `.md`. |
| `level` | `MUST` or `SHOULD`. |
| `paths` | Glob patterns the rule applies to, e.g. `["**/*.rs"]`. |
| `checks` | List of `{ backend, lint }`. `backend` is `rustc`, `clippy`, `dylint`, `xkmake` or `ai`; `lint` lists the lint names that enforce the rule (cargo JSON `code.code`; for `xkmake`, the check names registered with `xkmake hygiene check`, see [Repository checks](#repository-checks-xkmake)). The `ai` check has no `lint`; it may carry `triggers`. |
| `checks[ai].triggers` | Optional, inside the `- backend: ai` item only. Case-sensitive regular expressions, valid in both ECMAScript and Python `re`, matched against added diff lines in files matched by `paths`. xreview opens a rule session only for lines they match outside test code; without `triggers` it reviews every changed hunk of those files. Entries may also be trigger groups (xreview's `name`, `patterns`, `paths`, `exclude`, `lines`). Lint backends ignore them. |

## Body

`# <id>: <title>`, then bullets in this order: **Rule**, **Scope**, **Why**,
**How to check** (numbered steps), **Exceptions**, **Counter-example**
(code block plus one sentence), **Example**. Keep it short; runtime details
belong to the tool's own README.

## Index

- [lints.md](lints.md): rustc/Clippy levels, configuration, coverage, pending
  candidates, auxiliary checks, and open questions.
- `CG-*.md`: the rule registry for all backends, indexed below. Rules with
  mechanical and AI checks state both the enforced condition and the remaining
  judgment; CG-017 separately covers Send/Sync arguments.
- [pending.md](pending.md): Dylint candidates; landing a lint removes its row.
  Rustc/Clippy candidates stay in [lints.md](lints.md).
- [exceptions.toml](exceptions.toml): accepted findings of repository
  checks; see [Repository checks](#repository-checks-xkmake).

## Suppressing a check

Rust lints are suppressed with attributes in code, as described here;
findings of repository checks are registered in
[exceptions.toml](exceptions.toml) instead
(see [Repository checks](#repository-checks-xkmake)).
Use the narrowest scope and start every reason with the full rule id:

```rust
#[expect(clippy::unwrap_used, reason = "CG-021-no-panic-on-reachable-failure: <specific invariant and why it holds>")]
```

The default is `#[expect(<lint>, reason = "<rule-id>: <why>")]`. Use `allow`
only where an expectation is unreliable: cfg-, feature- or platform-dependent
hits, macro-generated code, or Dylint lints unknown to ordinary Clippy.
An outer `allow` must also satisfy the workspace's `clippy::allow_attributes`
deny by pairing it with an expectation at the same scope:

```rust
#[expect(clippy::allow_attributes, reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy")]
#[allow(unknown_lints, static_mut_option, reason = "CG-004-static-mut-option: <specific permitted exception>")]
static mut TEST_STATE: Option<u32> = None;
```

Dylint allowances include `unknown_lints` so ordinary `make clippy` accepts
custom lint names. The wrapper's reason identifies CG-024; the underlying
allowance identifies the substantive rule. The reason must satisfy that rule's
**Exceptions** and explain the actual invariant or constraint, rather than
say only "needed" or "legacy". Where a default auxiliary lint has no registry
rule, use `CG-024-lint-exceptions: <lint>: <why>`; this identifies the exception
policy without inventing a substantive rule for a default check. Module-docs
checks remain on their separate review track.

After the id, keep the established reason categories: `TODO(cg): <planned
repair>` for authorized outstanding repairs, `test scenario: <purpose>` for
helpers outside shared test macros, and `Fatal boot: <failure condition,
no recovery and no error-return channel>` for individually authorized boot
sites. Internal-invariant reasons name the invariant and why it holds. Rustc
ignores expectations on macro invocation statements such as `panic!();`;
place them on the smallest containing arm or item instead.

### Vendored code

`vendor/` holds third-party code imported as-is (see
[vendor/README.md](../../vendor/README.md)). CG rules and the module-docs
(DD/SD) checks do not apply to it. Hunks that this repository modifies locally
are reviewed for defects only, not for rule or documentation conformance.

### Per-crate opt-outs

Keep `#![allow(<lint>, reason = "<rule-id>: case (a)/(b): <specific reason>")]`
at each actually linted target root. Case (a) means the rule does not apply
(e.g. intentional host build failure or a pure test crate). Case (b) means
at least ten legacy sites in the union of Jenkins configurations, unless the
rule sets a stricter threshold. This is not a quota for new code: new code
in case (b) targets still follows the rule. Do not exempt targets that are
never linted. Inner allowances do not need the outer `allow_attributes`
wrapper.

A transitional crate-root expectation still requires an issue:
`#![expect(<lint>, reason = "<rule-id>: TODO(cg): legacy cleanup, tracked in <issue>")]`.
The fatal-boot policy stays per site; do not change boot interfaces or disable
boot crates merely to remove those expectations.

Register every inner opt-out of a rule lint, at a target root or in a
module, in [exceptions.toml](exceptions.toml) with the CG-024 check
`inner-opt-out` (see [Registering an exception](#registering-an-exception)):

```toml
[[exception]]
rule = "CG-024-lint-exceptions"
check = "inner-opt-out"
subject = "mm/kalloc/src/lib.rs:clippy::expect_used"
reason = "CG-024-lint-exceptions: CG-021-no-panic-on-reachable-failure: case (b): <specific reason>"
```

The subject is the file and the lint as the attribute writes it; the reason
wraps the attribute's reason as for outer allowances, or uses `TODO(cg): `
for a transitional expectation. A rule lint is one a rule file lists under
`rustc`, `clippy` or `dylint`; opt-outs of other lints are not registered.

### Verification

A scratch crate using the pair above passed Clippy 1.95.0 with both allowance
lints denied and `-D warnings`, and the nightly-2026-03-08 Dylint driver with
`-D warnings`. The driver loaded the repository lint library: removing the
allowance produced `static_mut_option`. The Clippy tool-lint expectation was
**not** reported as unfulfilled by Dylint; no conditional wrapper or broad
`unfulfilled_lint_expectations` suppression is needed. The same pair is also
checked through the configured kernel's `make clippy` during this migration.

## Repository checks (xkmake)

Rules over manifests, Kconfig, file layout or source attributes rather
than compiled Rust run in `xkmake hygiene check`. `make check_hygiene` runs every registered check
and needs no `.config`; `--rule <id>`, `--check <name>` and
`--defconfig <path>` (each repeatable) narrow a run, and
`make check_features` and `make check_test_layout` run CG-047 and CG-051
alone. Check names are the `lint` names under the rule's `xkmake` backend.
A unit test fails when the xkmake registry and the `CG-*.md` frontmatter
disagree in either direction, so a check and its rule file change together.

Findings print as text, or as JSON with `--format json`; each names its
rule, check, file and subject. `--report` adds advisory items that never
fail. The exit status is 0 without violations, 1 with violations, and 2
when a check cannot run, for example when `cargo metadata` or Kconfig
parsing fails. `xkmake hygiene features --base <rev>` remains the CG-047
diff mode. `deps`, `header`, `lints` and `host-tests` stay separate
`xkmake hygiene` commands without a CG rule.

CI runs `make check_hygiene` once, in its own Jenkins stage
`Check: Repository hygiene`; the other stages set `SKIP_HYGIENE=1`. Locally,
`make clippy`, `make dylint` and `make dylint_test` still run it first, and
the pre-commit hook runs `check_hygiene`.

### Registering an exception

A finding the rule's **Exceptions** permits is registered in
[exceptions.toml](exceptions.toml):

```toml
[[exception]]
rule = "CG-047-kconfig-feature-ownership"
check = "capability-bypass"
subject = "kdma -> alloc-engine/tlsf"
reason = "CG-047-kconfig-feature-ownership: crate-local implementation feature: <why the edge is local>"
```

`rule` and `check` must name a registered check, and `subject` must equal
the subject the finding reports. The reason follows the attribute reasons
above: after the full rule id, it names the case of the rule's
**Exceptions** that permits the finding and why it holds, or, for a
violation awaiting repair, `TODO(cg): <planned repair>`. The check rejects
a reason that is empty after the rule id or after `TODO(cg): `. An
exception that no longer matches a finding is stale and fails the check, so
fixing a violation removes its entry. A run limited with `--defconfig` does
not judge staleness; `--rule` and `--check` runs judge the entries of the
checks they run.

## Rules

| Rule | Level | Backend | Lints |
| --- | --- | --- | --- |
| [CG-001-static-spinlock-registration](CG-001-static-spinlock-registration.md) | MUST | dylint | `unregistered_static_spinlock` |
| [CG-002-static-mutex-registration](CG-002-static-mutex-registration.md) | MUST | dylint | `unregistered_static_lock` |
| [CG-003-runtime-lock-tracking](CG-003-runtime-lock-tracking.md) | MUST | dylint | `untracked_runtime_lock` |
| [CG-004-static-mut-option](CG-004-static-mut-option.md) | MUST | dylint | `static_mut_option` |
| [CG-005-user-abi-layout](CG-005-user-abi-layout.md) | MUST | dylint | `unsound_user_abi_impl` |
| [CG-006-user-copy-abi-contract](CG-006-user-copy-abi-contract.md) | MUST | dylint | `user_copy_without_abi_contract` |
| [CG-007-attribute-order](CG-007-attribute-order.md) | MUST | dylint | `derive_not_last`, `unsorted_attributes` |
| [CG-008-unsafe-attribute-safety](CG-008-unsafe-attribute-safety.md) | MUST | dylint | `undocumented_unsafe_attribute` |
| [CG-009-vec-shift-in-loop](CG-009-vec-shift-in-loop.md) | SHOULD | dylint | `vec_shift_in_loop` |
| [CG-010-typed-error-result](CG-010-typed-error-result.md) | SHOULD | dylint | `untyped_error_result` |
| [CG-011-time-unit-names](CG-011-time-unit-names.md) | SHOULD | dylint | `unitless_time_integer` |
| [CG-012-private-foreign-items](CG-012-private-foreign-items.md) | SHOULD | dylint | `exported_foreign_item` |
| [CG-013-no-sleep-under-spinlock](CG-013-no-sleep-under-spinlock.md) | MUST | ai | — |
| [CG-014-spin-noirq-needs-irq-contender](CG-014-spin-noirq-needs-irq-contender.md) | SHOULD | ai | — |
| [CG-015-no-split-critical-section](CG-015-no-split-critical-section.md) | MUST | ai | — |
| [CG-016-safety-comment-substance](CG-016-safety-comment-substance.md) | MUST | clippy, ai | `clippy::undocumented_unsafe_blocks` |
| [CG-017-unsafe-impl-send-sync-argument](CG-017-unsafe-impl-send-sync-argument.md) | MUST | ai | — |
| [CG-018-uninitialized-storage-model](CG-018-uninitialized-storage-model.md) | MUST | clippy, ai | `clippy::disallowed_methods` |
| [CG-019-user-pointers-stop-at-syscall-boundary](CG-019-user-pointers-stop-at-syscall-boundary.md) | MUST | ai | — |
| [CG-020-raw-output-buffer-capacity](CG-020-raw-output-buffer-capacity.md) | MUST | ai | — |
| [CG-021-no-panic-on-reachable-failure](CG-021-no-panic-on-reachable-failure.md) | MUST | clippy, ai | `clippy::panic`, `clippy::unwrap_used`, `clippy::expect_used` |
| [CG-022-log-level-matches-event](CG-022-log-level-matches-event.md) | SHOULD | ai | — |
| [CG-023-unfinished-implementations](CG-023-unfinished-implementations.md) | MUST | clippy | `clippy::todo`, `clippy::unimplemented` |
| [CG-024-lint-exceptions](CG-024-lint-exceptions.md) | MUST | clippy, xkmake | `clippy::allow_attributes`, `clippy::allow_attributes_without_reason`, `inner-opt-out` |
| [CG-025-minimal-unsafe-scope](CG-025-minimal-unsafe-scope.md) | MUST | clippy, rustc | `clippy::multiple_unsafe_ops_per_block`, `unsafe_op_in_unsafe_fn` |
| [CG-026-unsafe-api-safety-docs](CG-026-unsafe-api-safety-docs.md) | MUST | clippy | `clippy::missing_safety_doc` |
| [CG-027-narrow-visibility](CG-027-narrow-visibility.md) | SHOULD | rustc | `unreachable_pub` |
| [CG-028-focused-functions](CG-028-focused-functions.md) | SHOULD | clippy | `clippy::too_many_lines` |
| [CG-029-shallow-control-flow](CG-029-shallow-control-flow.md) | SHOULD | clippy | `clippy::excessive_nesting` |
| [CG-030-explicit-boolean-parameters](CG-030-explicit-boolean-parameters.md) | SHOULD | clippy | `clippy::fn_params_excessive_bools` |
| [CG-031-intrusive-linked-lists](CG-031-intrusive-linked-lists.md) | SHOULD | clippy | `clippy::linkedlist` |
| [CG-032-camel-case-types](CG-032-camel-case-types.md) | SHOULD | clippy | `clippy::upper_case_acronyms` |
| [CG-033-explicit-result-discard](CG-033-explicit-result-discard.md) | SHOULD | clippy | `clippy::unused_result_ok` |
| [CG-034-used-binding-names](CG-034-used-binding-names.md) | SHOULD | clippy | `clippy::used_underscore_binding` |
| [CG-035-declared-cfgs](CG-035-declared-cfgs.md) | SHOULD | rustc | `unexpected_cfgs` |
| [CG-036-ordinary-error-propagation](CG-036-ordinary-error-propagation.md) | SHOULD | clippy | `clippy::question_mark` |
| [CG-037-spin-raw-context-precondition](CG-037-spin-raw-context-precondition.md) | MUST | ai | — |
| [CG-038-prepare-reusable-work-handles](CG-038-prepare-reusable-work-handles.md) | MUST | ai | — |
| [CG-039-execution-context-contract](CG-039-execution-context-contract.md) | MUST | ai | — |
| [CG-040-lock-stat-accounting](CG-040-lock-stat-accounting.md) | MUST | ai | — |
| [CG-041-reuse-safe-boundary-abstractions](CG-041-reuse-safe-boundary-abstractions.md) | SHOULD | ai | — |
| [CG-042-abi-carrier-semantic-conversion](CG-042-abi-carrier-semantic-conversion.md) | SHOULD | ai | — |
| [CG-043-reuse-once-lazy-lifecycle](CG-043-reuse-once-lazy-lifecycle.md) | SHOULD | ai | — |
| [CG-044-unsafe-precondition-assertions](CG-044-unsafe-precondition-assertions.md) | SHOULD | ai | — |
| [CG-045-non-time-integer-units](CG-045-non-time-integer-units.md) | SHOULD | ai | — |
| [CG-046-atomic-ordering-rationale](CG-046-atomic-ordering-rationale.md) | SHOULD | ai | — |
| [CG-047-kconfig-feature-ownership](CG-047-kconfig-feature-ownership.md) | MUST | xkmake, ai | `feature-map`, `kconfig-implication`, `capability-bypass`, `peer-forwarding`, `build-args` |
| [CG-048-direct-owner-conversion](CG-048-direct-owner-conversion.md) | SHOULD | ai | — |
| [CG-049-no-std-box-in-kernel-tests](CG-049-no-std-box-in-kernel-tests.md) | MUST | dylint | `std_box_in_kernel_tests` |
| [CG-050-scheduler-lock-order](CG-050-scheduler-lock-order.md) | MUST | ai | — |
| [CG-051-test-source-layout](CG-051-test-source-layout.md) | MUST | xkmake | `src-test-file`, `inline-test-module`, `src-test-function`, `legacy-host-tests-dir`, `ut-tests-host-attr`, `tests-guest-marker`, `test-expect-attr` |

### Candidates not yet ruled

The second AI batch defers these candidates; their text remains in the skill:

- R02, Name semantics, scans and observable work honestly:
  [naming-and-structure.md](../ai/skills/code-guidelines/naming-and-structure.md#names),
  [performance-and-resources.md](../ai/skills/code-guidelines/performance-and-resources.md#performance)
  and the [SKILL.md checklist](../ai/skills/code-guidelines/SKILL.md#review-checklist).
- R11, Keep the crate logging facade consistent:
  [error-handling-and-logging.md](../ai/skills/code-guidelines/error-handling-and-logging.md#logging)
  and the [SKILL.md checklist](../ai/skills/code-guidelines/SKILL.md#review-checklist).
- R12, Cite the external behavioral contract:
  [comments-and-rustdoc.md](../ai/skills/code-guidelines/comments-and-rustdoc.md#comments).
- R14, Keep contract comments true after behavior changes, and R15, Explain
  non-obvious intent rather than paraphrase code:
  [comments-and-rustdoc.md](../ai/skills/code-guidelines/comments-and-rustdoc.md#comments)
  and its [review checklist](../ai/skills/code-guidelines/comments-and-rustdoc.md#when-reviewing).

## Adding a rule

First choose the backend. Rustc/Clippy checks inherit `[workspace.lints]` in
root `Cargo.toml`; parameters live in root `clippy.toml`. Validate through
`make clippy` after preparing a platform defconfig, as described in
`docs/ai/skills/build-workflow/SKILL.md`. Dylint candidates are listed in
[pending.md](pending.md); landing one removes its row. Checks over manifests,
Kconfig or file layout rather than compiled Rust are registered with
`xkmake hygiene check` and list their check names under the `xkmake`
backend; see [Repository checks](#repository-checks-xkmake). AI checks
describe judgments the other backends cannot prove. xreview loads only
`CG-*.md` and selects rules with an `ai` check; `triggers` belong to that
check, so files without one have none and are ignored by AI review.

For a Dylint rule:

1. Write the lint module `tools/lints/src/<lint>.rs`, including its embedded UI test.
2. In `tools/lints/src/lib.rs`, add `mod <lint>;` and register its lint and pass in `register_lints`.
3. Add `docs/code-guidelines/CG-<nnn>-<descriptive>.md` and its row in the table above.
4. Clean up the rule's existing violations (the backlog) in the same commit.

For an xkmake rule:

1. Implement the check in `xtask/xkmake/src/hygiene/` and add it to the
   registry, with tests for each check name.
2. Add the rule file with those names under `backend: xkmake`, and its row
   in the table above.
3. Fix existing violations or register them in
   [exceptions.toml](exceptions.toml) in the same commit.

## Relationship to existing guidance

`docs/coding-guidelines/` and `docs/coding-guidelines-cn/` are the older
general guidelines. `docs/ai/skills/code-guidelines/` is the AI-facing skill.
Rules migrate here one at a time; when a rule is migrated, its prose is deleted
from its old location without leaving a per-rule pointer. The skill routes
writing and review to this directory; `lints.md` records backend coverage.
Read the registry rule for the full requirement and judgment criteria.
