---
id: CG-037-error-vocabulary-names
level: SHOULD
paths: ["drivers/**/*.rs"]
checks:
  - backend: ai
    lint: []
---

# CG-037-error-vocabulary-names: Name error types by the phase they describe

- **Rule**: An error type encodes the phase it belongs to: initialization
  failures are `*InitError`, steady-state failures are `*Error` or the shared
  `RuntimeError`, and probe control flow is `ProbeError`. Do not introduce a
  second name for a phase that already has one.
- **Scope**: Error enums and structs under `drivers/`.
- **Why**: A Linux driver transcription maps each Linux failure path to one
  phase (`probe` / `remove` / data plane / initialization). Mixed names
  (`ResError`, `PciInitError`, `AcpiInitError`, `FirmwareInitError`,
  `PublishError`, `AddDeviceError`) make the reader decide the phase from the
  call site, which is exactly the decision the framework should already have
  made.
- **How to check**:
  1. On a new error type, name the phase in the type name.
  2. If the failure is init-only, prefer `*InitError`; if it is the shared
     driver-visible vocabulary, reuse `RuntimeError` instead of adding a type.
  3. Run `make clippy`; the AI review checks this rule on driver diffs.
- **Exceptions**: A type that deliberately spans phases (e.g. `ProbeError`
  carrying both deferral and fatal outcomes) keeps its name; do not split it.
  Use the syntax in [README.md](README.md#suppressing-a-check).
- **Counter-example**:

```rust,ignore
pub enum ResError { InvalidResource, MappingFailed, /* ... */ }
```

A provider-internal error that callers must translate by hand duplicates the
phase already expressed by `RuntimeError`.

- **Example**:

```rust,ignore
pub enum DeviceAddError { ParentMissing, /* ... */ }
impl From<ResError> for RuntimeError { /* the single mapping */ }
```
