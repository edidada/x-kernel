---
id: CG-024-lint-exceptions
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: clippy
    lint: [allow_attributes, allow_attributes_without_reason]
  - backend: xkmake
    lint: [inner-opt-out]
---

# CG-024-lint-exceptions: Justify lint exceptions at the narrowest scope

- **Rule**: Lint exceptions MUST use the narrowest practical scope and a concrete, truthful rule-id reason.
- **Scope**: Rust lint attributes, including generated attributes and inner crate-root opt-outs.
- **Why**: Focused reasons preserve the review obligation instead of hiding unrelated violations.
- **How to check**:
  1. Run the backend with the configured platform and review each diagnostic.
  2. Use the unified syntax in README.md. These Clippy lints check outer allow attributes and presence of a reason; review must check scope and substance. For `expect(dead_code)`, permit only simple intentional code likely to be used, and explain when it will be used.
  3. `make check_hygiene` (`inner-opt-out`) finds every inner `#![allow(..)]`
     or `#![expect(..)]`, also under `cfg_attr` and in any module, of a lint
     that a rule file lists under its `rustc`, `clippy` or `dylint` check.
     Each file and lint must be registered in [exceptions.toml](exceptions.toml)
     with the subject `<path>:<lint>`, e.g.
     `mm/kalloc/src/lib.rs:clippy::expect_used`, and the reason
     `CG-024-lint-exceptions: <owning rule id>: case (a)/(b): <why>`, or
     `…: TODO(cg): <planned repair>` for a transitional expectation; for this
     rule's own lints the owning id is not repeated. Unregistered and stale
     entries fail. Lints no rule owns, such as `missing_docs` or `dead_code`,
     are not checked.
- **Exceptions**: Only the README unreliability cases permit outer allow. Inner opt-outs of rule lints are the [README.md](README.md#per-crate-opt-outs) per-crate cases or module-level cases the owning rule's **Exceptions** permit, registered in [exceptions.toml](exceptions.toml). For both allowance lints the stricter case (b) threshold is twenty semantically diverse legacy sites. Review new code in opted-out crates against scope and reason requirements. Use the syntax in [README.md](README.md#suppressing-a-check).
- **Counter-example**:

```rust,ignore
#[allow(clippy::unwrap_used)]
mod production;
```

This violates the requirement or needs a specific permitted exception.

- **Example**:

```rust,ignore
#[expect(clippy::unwrap_used, reason = "CG-021-no-panic-on-reachable-failure: validated nonempty input guarantees a first element")]
let first = input.first().unwrap();
```
