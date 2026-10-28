---
id: CG-021-no-panic-on-reachable-failure
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: clippy
    lint: [panic, unwrap_used, expect_used]
  - backend: ai
    triggers:
      - '^(?!\s*//).*(?:\.unwrap\(\)|\.expect\(|panic!)'
      - '^(?!\s*//).*(?:unreachable!|debug_assert!)'
      - 'clippy::(?:panic|unwrap_used|expect_used)\b'
---

# CG-021-no-panic-on-reachable-failure: Handle legitimate failures without panicking

- **Rule**: Code MUST NOT use `.unwrap()`, `.expect()`, explicit panics, or hidden
  panic paths where failure is a legitimate possibility.
- **Scope**: Failure paths in all Rust code, including helpers reached from
  normal control flow. Determine reachability from inputs and established
  invariants; an operation proven infallible does not have a legitimate failure
  path. Runtime validation must not be downgraded to `debug_assert!`. The three Clippy lints run at warn; `make clippy` promotes warnings to errors.
  CI therefore rejects an unsuppressed `.unwrap()`, `.expect()` or `panic!`
  outside test code in every crate that a Jenkins Clippy configuration builds.
  Clippy does not see code outside those builds (LoongArch64, `vendor/`,
  `xtask/`, `tools/`, crates such as `virt/kvmm` that no CI defconfig links,
  code behind features no CI configuration selects, such as `lock_stat`),
  and the crate-level allowances below switch a lint off for the whole crate.
  AI review judges every `clippy::panic`/`unwrap_used`/`expect_used`
  suppression against **Exceptions**, `unreachable!` and `debug_assert!`,
  panic sites in that unlinted code, and whether each failure is reachable.
  Not this rule: the Dylint lint `untyped_error_result` ([CG-010](CG-010-typed-error-result.md)) owns unit/string
  `Result` error types, and
  [CG-036](CG-036-ordinary-error-propagation.md) owns ordinary `?`
  propagation style; this rule owns terminating or omitting runtime validation
  on a reachable legitimate failure. Log severity belongs to
  [CG-022](CG-022-log-level-matches-event.md), and unfinished
  `todo!`/`unimplemented!` bodies to [CG-023](CG-023-unfinished-implementations.md),
  rather than reporting those same defects again as panic findings.
- **Why**: An ordinary failure should follow the layer error contract instead of terminating the kernel.
- **How to check**:
  1. Run `make clippy` with the configured platform for the mechanical checks.
  2. Inspect each panic-capable operation and the helpers it invokes.
  3. Trace whether valid control flow, external input, or a failed operation
     can reach the panic; identify any invariant that rules it out.
  4. For a legitimate failure, check that the code handles or propagates it
     instead of panicking or relying on debug-only runtime validation.
- **Exceptions**: Per-site: a proven internal invariant; `Fatal boot: ` with its failure condition, no recovery and no error-return channel; shared test macros or `test scenario: ` helper reasons. `panic` additionally permits `TODO(cg): ` when legitimate failure cannot be reported by the current interface; state the planned interface or lifecycle repair. Do not disable boot crates or change boot interfaces for lint cleanup.
  - Crate-level `panic`, `unwrap_used` and `expect_used` opt-outs, case (a) or (b), are registered in [exceptions.toml](exceptions.toml) under `CG-024-lint-exceptions`/`inner-opt-out`; `make check_hygiene` fails on an unregistered or stale one.
  - Case (b) threshold is ten diagnosed sites across platform configurations; only src/lib.rs is exempt. `kernel-boot` and `fs_boot` stay enabled. Use the syntax in [README.md](README.md#suppressing-a-check).
  - `debug_assert!` may check correctness invariants; it must not replace runtime validation.
- **Counter-example**:

```rust,ignore
fn read_config() -> Result<Config, Error> {
    Ok(load_config().unwrap())
}
```

This violates the requirement or needs a specific permitted exception.

- **Example**:

```rust,ignore
fn read_config() -> Result<Config, Error> {
    let config = load_config()?;
    Ok(config)
}
```
