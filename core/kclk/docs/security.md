# kclk - Security And Reliability

## Trust Model

Firmware (device-tree / ACPI) descriptions, MMIO register contents, and
platform-supplied register base addresses are external inputs. The framework
trusts its own invariants (reference counts, cached rate, parent links) and
treats everything read from or described by hardware as untrusted numeric data.

A clock controller driver is a trust boundary: it translates firmware
descriptions into `ClkHwOps` behavior. The framework core (`kclk`) validates the
*shape* of operations (parent indices in range, non-zero dividers, rate change
results) but not their *correctness* — programming a divider with a nonsense
field encoding is the controller driver's responsibility.

## External Boundaries

- **Firmware register base addresses** are consumed by the controller driver to
  establish an MMIO mapping. This crate never sees an address: it receives a
  `ClkRegAccess` and reads registers through it. Mapping validity and teardown
  are owned by the host adapter and the driver's devm-managed mapping.
- **MMIO register contents** are read by `Gate`, `Mux`, and `Divider` through
  `ClkRegAccess`. They are treated as untrusted integers; field extraction never
  indexes out of bounds.
- **Consumer clock specifiers** (`ClkIndex` DT index or output name) come from
  drivers and, in the future, from DT parsing. An unknown name or out-of-range
  index yields `None` / `NotFound`, never a panic.

## Unsafe Code

The crate's only `unsafe` is one `ptr::read` that moves an `Arc` out of a
`ManuallyDrop<Self>` (see the second half of this section); it owns no MMIO
mechanism and no MMIO raw pointer. Register access is a trait call; the
production implementation
(`IoMapClkReg` in the X-Kernel provider adapter) wraps `driver_os_api`'s
`IoMap`, which is where the volatile loads/stores, the alignment and
bounds checks, and the mapping lifetime live. What this crate contributes is
the *contract* those accesses are validated against:

- every node constructor checks `offset_bytes` is word-aligned and that its
  register word stays inside `ClkRegAccess::window_bytes()` when the accessor
  reports one, so a malformed table is rejected with `ClkError::InvalidValue`
  instead of reaching memory the mapping does not cover;
- the framework serializes the *sleepable* write paths
  (`set_rate`/`set_parent`/`prepare`) under `CLK_LOCK`; atomic
  `enable`/`disable` callbacks run under `CLK_ENABLE_LOCK` and only flip a gate
  bit. Topology migration nests the locks in that order. See the *Known
  Limitations* note on register-word sharing (use `HIWORD_MASK`).

`ClkHandle::into_core` moves the `Arc<ClkCore>` out of a `ManuallyDrop<Self>`
with one documented `unsafe` `ptr::read`: no `Drop` runs on the forgotten
handle, so the consumer state (rate vote, notifier id) is intentionally left
behind and the node reference transfers to the caller.
`Gate`/`Mux`/`Divider` field math and parent recursion otherwise use safe code.

## Invariants

- Consumers only ever hold `ClkHandle` (kclk's own guard, or the
  capability-contract wrapper around it); they cannot reach `ClkHwOps` or the
  raw register accessor.
- `enable_count` / `prepare_count` never go below zero (decrement guards).
- A hardware gate op (`prepare`/`enable`/`disable`/`unprepare`) is invoked at
  most once per 0→1 / 1→0 count transition.
- `set_parent` rejects an out-of-range or orphan (unmapped) parent slot before
  touching hardware.
- A divider never divides by zero: `rate_for` treats a zero divider as
  pass-through, and `raw_to_div` clamps a decoded zero to 1.
- Cached rate is refreshed under the node lock after every successful
  `set_rate` / `set_parent`.
- All sleepable mutations are serialized by `CLK_LOCK`; `enable`/`disable` and
  topology commits are serialized by `CLK_ENABLE_LOCK`, with the sole nesting
  order `CLK_LOCK` → `CLK_ENABLE_LOCK`. Lockless `get_rate` may observe the old
  or new per-node cache during a multi-node transition, but never an invalid
  memory state.
- Notifier callbacks run under `CLK_LOCK`; they may sleep but must not re-enter
  the framework (no `clk_get_*` / `set_rate` / `enable`) — re-entrancy panics.
- `set_rate_range` rejects an inverted range (`min > max`) with `InvalidRate`;
  later `set_rate`/`round_rate` requests are clamped into `[min, max]`.
- `get_rate` re-reads the hardware rate when `GET_RATE_NOCACHE` is set.
- `clk_disable_unused` skips clocks marked `IS_CRITICAL` or `IGNORE_UNUSED` and
  only gates clocks with `enable_count == 0`.
- `ClkBuilder::build` rejects a parent index outside the parent table (except a
  root with no parents) rather than silently treating it as a root.
- `determine_rate` may select a `best_parent_index`; the core validates it in
  `switch_parent_locked` (rejecting out-of-range or orphan slots) before any
  register write.
- Orphan adoption (`parent_name`) fills only the declared slot when a
  matching-name clock is registered, migrates all active references, and
  refreshes descendant caches. Failure restores the orphan entry and releases
  partially migrated references. Failed table registration similarly restores
  pre-existing adoptees before removing exactly its batch nodes.
- A failed `set_rate` restores any parent selection made by `determine_rate`;
  if upward propagation programmed the parent but the child cannot recalculate,
  the parent is restored to its previous rate. A failed rollback is surfaced as
  `BadState` rather than reported as an ordinary rejected request.
- `ClkTable::unregister` drops both the index and name entries for a clock;
  consumer-held handles keep the node alive by reference counting.

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | Invalid MMIO base behind a `ClkRegAccess` | High | Controller driver hands an unmapped or wrong address | The adapter stores a validated `IoMap` instead of an address: mapping success, region size and unmap are the host's, and every access is bounds- and alignment-checked there. Residual risk is the host mapping it supplies. |
| T-02 | Out-of-range parent index | Medium | Firmware or driver requests parent `N` beyond the table | `ClkCore::set_parent` returns `InvalidParent` without touching hardware. |
| T-03 | Zero or huge decoded divider | Medium | Register field decodes to 0, or a divider that would overflow | `rate_for` passes through on 0; `closest_div`/`div_to_raw` clamp the `POWER_OF_TWO` exponent to the field mask, capped at 31 so the decoded `u32` divider stays representable; `FixedFactor::scale` rejects overflow with `InvalidRate`. |
| T-04 | Concurrent rate change on a sibling | Low | Two probes reprogram overlapping subtrees | Serialized by the global `CLK_LOCK`; only one sleepable mutation runs at a time across the whole tree. |
| T-05 | Provider lookup before install | Low | Driver resolves a clock before controller probe completes | Returns `NoProvider`; the integration layer maps this to deferred probe instead of permanent failure. |
| T-06 | Subscriber re-enters the framework in a notifier | High | A `ClkNotifier` callback calls `set_rate`/`get_rate` re-entrantly | Documented constraint: callbacks run under `CLK_LOCK` and must be non-re-entrant (re-entrancy panics). Audit checklist enforces it. |
| T-07 | `set_rate` reprograms a clock a consumer depends on without notice | Medium | DVFS changes a shared PLL underneath a display | Rate-change notifiers deliver `Pre`/`Post` events; a `Pre` veto aborts with `Aborted`. |

## Failure Modes

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | A hardware `prepare`/`enable` fails mid-chain | Parent already enabled | Clock left disabled | 3 | Roll-back: the parent's count is decremented and its hardware op reversed before returning the error. |
| F-02 | `set_rate` unsupported on a node without `SET_RATE_PARENT` | Rate unchanged | Driver sees `NotSupported` | 3 | Propagated as `ClkError::NotSupported` → `ResError::Unsupported`. |
| F-03 | `set_parent` to an orphan slot | Parent unchanged | Driver sees `InvalidParent` | 3 | Rejected before any register write. |
| F-04 | Provider not installed at lookup time | `NoProvider` returned | Consumer probe fails and the device stays unclaimed | 3 | `NoProvider` maps to a probe error at the capability-contract boundary. A deferred-probe/wake mechanism that retried the same driver is planned as a follow-up framework change. |
| F-05 | Divider field width outside `1..32`, or a mux field that encodes more than 255 parents | Configuration error | Provider probe sees `ClkError::InvalidValue` | 2 | Both values describe the provider's register layout, so a bad one is a reachable input failure, not a broken internal invariant: `Divider::new` and `Mux::new` validate at construction and return `ClkError::InvalidValue` (`1u32 << width` would be undefined behavior at width >= 32, and the framework's parent index is a `u8`). `register_table` propagates the error before publishing any node. |

## Known Limitations

- No authenticity/integrity check on firmware-described clocks; a misconfigured
  tree produces wrong but representable rates.
- Notifiers fire on every reprogrammed clock (the entry node and each parent
  reprogrammed during `SET_RATE_PARENT` propagation); a `Pre` veto anywhere
  aborts the whole change.
- The slow-operation lock is a sleepable mutex, so `prepare`/`unprepare` and
  `set_rate` provider callbacks may block. `enable`/`disable`, `set_parent`,
  reparent-time recalculation, and otherwise lock-free callbacks must not sleep.
- Register-window validation is only as good as `window_bytes`: an accessor that
  returns `None` (test doubles, `NullClkReg`) leaves descriptors unchecked, and
  the framework cannot verify the *contents* of a window an accessor does
  report.

## Audit Checklist

- Confirm every `ClkRegAccess` implementation is built from a device mapping
  (the adapter's `IoMapClkReg` over `IoMap`), not from a hand-passed address, and
  that accessors which know their region size report it through `window_bytes`.
- Confirm the only `unsafe` in this crate stays the `ManuallyDrop` move in
  `ClkHandle::into_core`, and that register access stays in the host adapter's
  implementation.
- Confirm a controller driver keeps all bit fields within a 32-bit word and
  validates `width`/`shift` against its register layout.
- Confirm new `ClkHwOps` impls do not re-enter the framework (no nested
  `clk_get_*` / `set_rate`) from within an op callback.
- Confirm any registered `ClkNotifier` does not call back into the clock
  framework (it runs under the non-re-entrant `CLK_LOCK`).
- Confirm consumers that veto rate changes handle `ClkError::Aborted`.
- Confirm `set_parent` callers handle `InvalidParent` rather than ignoring it.
- Confirm consumers release clocks via `devm_` (preferred) or explicit drop, so
  reference counts return to zero on probe failure / removal.
- Confirm `set_rate_range` callers do not set `min > max`.
- Confirm `clk_disable_unused` runs only after all providers are registered.
