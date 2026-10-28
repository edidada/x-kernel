---
id: CG-051-test-source-layout
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: xkmake
    lint: [src-test-file, inline-test-module, src-test-function, legacy-host-tests-dir, ut-tests-host-attr, tests-guest-marker, test-expect-attr]
---

# CG-051-test-source-layout: Keep tests out of production sources

- **Rule**: Test code MUST NOT live in a crate's `src/`. Guest unit tests
  MUST live in `<crate>/ut-tests/`, use `#[def_test]`, and be mounted from
  `src/` with `unittest::path_mod_test!`. Host tests MUST live in
  `<crate>/tests/` and use `#[test]`. Test sources MUST NOT hand-write
  `#[expect]` for `clippy::unwrap_used`, `clippy::expect_used` or
  `clippy::panic`; the test macros and `clippy.toml` already grant them.
- **Scope**: Rust files of every root workspace member, including implicit
  path members, whether tracked or untracked but not ignored; paths are
  relative to the crate root. Host tests are either Cargo integration tests
  (`tests/*.rs`) or host unit tests that need private items, kept in
  `tests/host/` and mounted with
  `#[cfg(test)] #[path = "../tests/host/<name>.rs"] mod <name>;`.
  Separate Cargo workspaces such as `xtask` are not checked. File names
  inside `ut-tests/` and `tests/` are not ruled. Not this rule: which lints
  the test macros exempt is recorded in [lints.md](lints.md); panic policy
  outside tests belongs to [CG-021](CG-021-no-panic-on-reachable-failure.md).
- **Why**: Production files then contain only production code. Guest tests
  share one `cfg(unittest)` gate and lint policy, host tests build with bare
  host Cargo, and tools and reviewers can tell tests from production by path.
- **How to check**:
  1. Run `make check_hygiene`, or `make check_test_layout` for this rule
     only; neither needs a `.config`. `src-test-file`: a file under `src/`
     named `tests.rs`, `test_*` or `*_tests.rs`. `inline-test-module`: in
     `src/`, a `mod` item directly after `#[cfg(test)]`, `#[cfg(unittest)]`,
     `#[mod_test]` or `#[unittest::mod_test]`, inline or a file module, even
     when its body only `include!`s test files; a `pub` module under a cfg
     marker counts only with an inline body, so
     `#[cfg(unittest)] pub mod ahci;` passes. `src-test-function`: in any
     other `src/` file, a `#[test]` or `#[def_test]` attribute, matched by
     last path segment, also inside `cfg_attr`, disabled cfgs or
     `macro_rules!` bodies. `legacy-host-tests-dir`: any Rust file under
     `host-tests/`, the former host-test location now replaced by `tests/`.
     `ut-tests-host-attr`: a `#[test]` attribute
     under `ut-tests/`. `tests-guest-marker`: a `#[def_test]` attribute under
     `tests/`, including `tests/host/`. `test-expect-attr`: under `ut-tests/`
     or `tests/`, a line starting `#[expect(` or `#![expect(` that names one
     of the three Clippy lints.
  2. Attribute checks read Rust tokens, so comments and string literals do
     not count; module and expectation checks are textual. None expands
     macros or evaluates cfgs, and an `include!` alone is not evidence of
     test code: production modules include generated configuration too.
  3. When host tests change, run `make check_host_tests`: every member with
     `tests/*.rs` must build with `cargo test --tests --no-run` on the host,
     and each `[[test]]` with `required-features` with those features. It is
     a manual check, not part of `check_hygiene`. Guest tests run with
     `make unittest`.
  4. Keep cargo-shear accurate. Root `Cargo.toml`
     `[workspace.metadata.cargo-shear]` ignores the `unittest` dependency and
     the test paths (`ignored-paths`, currently `**/ut-tests/*.rs` and
     `**/tests/host/*.rs`); a dependency used only by tests goes in the
     crate's `[package.metadata.cargo-shear] ignored` with an adjacent reason
     comment, which `make check_deps` requires.
- **Exceptions**: None standing. Register an accepted finding in
  [exceptions.toml](exceptions.toml) with this rule id, its `check`, the
  reported `subject` and a reason starting with the rule id; see
  [README.md](README.md#registering-an-exception).
- **Counter-example**:

```rust,ignore
// src/copy.rs
#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    #[def_test]
    fn copies_bytes() { /* ... */ }
}
```

The guest test lives in production source; `inline-test-module` and
`src-test-function` report it.

- **Example**:

```rust,ignore
// src/copy.rs
unittest::path_mod_test!(tests, "../ut-tests/copy.rs");

// ut-tests/copy.rs
use unittest::def_test;

use super::*;

#[def_test]
fn copies_bytes() { /* ... */ }
```

Host tests of the public API go in `tests/copy.rs` with `#[test]`.
