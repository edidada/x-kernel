# xkernel lints

Dylint library holding the repository's custom lints. Each lint enforces a
rule in `docs/code-guidelines/`. The crate builds one cdylib that
`make dylint` loads to scan the configured kernel.

## Prerequisites

```bash
rustup component add rustc-dev llvm-tools --toolchain nightly-2026-03-08
rustup target add aarch64-unknown-none-softfloat x86_64-unknown-none --toolchain nightly-2026-03-08
make install-tools   # cargo-dylint and dylint-link 6.0.4
```

## Running

```bash
make dylint          # scan the kernel selected by .config
make dylint_test     # run every lint's UI test
make dylint XKMAKE_ARGS="-- --message-format=json"
```

A scan only sees code that the configuration compiles; CI scans the
aarch64 and x86_64 `qemu_defconfig` configurations. Add the platform's
kernel target to the toolchain before scanning another configuration.

The first run builds the Dylint driver into `$DYLINT_DRIVER_PATH`
(default `~/.dylint_drivers`).

## Adding a lint

1. Add `src/<lint>.rs`: declare the lint with `rustc_session::declare_lint!`
   and the pass with `declare_lint_pass!`, then implement the pass the rule
   needs:
   - `LateLintPass`, registered with `register_late_pass`: resolved types,
     paths and macro output. Use it unless the rule is about source syntax.
   - `EarlyLintPass`, registered with `register_pre_expansion_pass`: the
     attributes as written, such as `derive` and `cfg_attr`. It visits every
     module file but not items that macros generate, and it does not expand
     `cfg_attr`.
   - `register_early_pass` runs after expansion, where `derive` and
     `cfg_attr` are already gone.

   Early contexts emit through `LintContext::emit_span_lint`, which takes a
   `rustc_errors::Diagnostic` value instead of a closure.
2. End the module with a `#[cfg(test)]` UI test: an embedded source file and
   the expected compiler output, run through `crate::testing::run_ui` with
   the mock crates the source uses (for example `testing::MOCK_KSPIN`).
3. In `src/lib.rs`, add `mod <lint>;` and register the lint and its pass in
   `register_lints`.
4. Add the rule file `docs/code-guidelines/CG-<nnn>-<descriptive>.md`.

`dylint_testing` runs UI tests under one lock. When a test fails, the lock is
poisoned and every later UI test fails with a `PoisonError`; fix the first
failure, or run one test with `cargo test <lint>`.

## Toolchain

The library links against rustc internals. Bump these together:
`rust-toolchain.toml`, the Makefile `fmt` toolchain, Jenkins
`AUX_RUST_TOOLCHAIN`, and the `dylint_*` / `cargo-dylint` / `dylint-link`
versions.

This standalone host workspace does not inherit the kernel root
`[workspace.lints]`. Validate its compiler-private code and embedded fixtures
with its pinned nightly through `make dylint_test`; kernel lint policy remains
in [the unified guidelines](../../docs/code-guidelines/README.md).
