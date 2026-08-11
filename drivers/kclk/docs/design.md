# kclk - Design

## Responsibility

`kclk` is the in-kernel common clock framework core. It owns:

- the provider-side hardware operation contract ([`ClkHwOps`]),
- the clock-tree node that caches rate/accuracy and serializes reference-counted
  gating and rate/parent changes ([`ClkCore`], [`ClkBuilder`]),
- the global provider registry consumers query ([`ClkLookup`], [`ClkTable`],
  [`register_provider`]),
- the consumer handle returned to drivers ([`ClkHandle`]),
- the reusable building-block clock types ([`FixedRate`], [`FixedFactor`],
  [`Gate`], [`Mux`], [`Divider`], [`Composite`]).

It is the in-kernel analogue of the Linux `drivers/clk/clk.c` core plus the
`clk-fixed-rate` / `clk-gate` / `clk-mux` / `clk-divider` / `clk-composite`
helpers. Concrete SoC clock-controller drivers (PLLs, CCUs) are *not* part of
this crate; they live in a sibling crate and register their outputs here.

[`ClkHwOps`]: ../src/ops.rs
[`ClkCore`]: ../src/core.rs
[`ClkBuilder`]: ../src/core.rs
[`ClkLookup`]: ../src/provider.rs
[`ClkTable`]: ../src/lookup.rs
[`register_provider`]: ../src/provider.rs
[`ClkHandle`]: ../src/handle.rs
[`FixedRate`]: ../src/basic/fixed_rate.rs
[`FixedFactor`]: ../src/basic/fixed_factor.rs
[`Gate`]: ../src/basic/gate.rs
[`Mux`]: ../src/basic/mux.rs
[`Divider`]: ../src/basic/divider.rs
[`Composite`]: ../src/basic/composite.rs

## Layering

Clock support is split across three crates, mirroring how IRQ is wired through
`device-res` → `kdriver::resource` → `kirq`:

```text
device-res (OS-agnostic resource model)
  ClkSpec / ClkConsumer / ClkOp / Clk / devm_clk_get
        |  does NOT depend on kclk
        v
kdriver::resource (OS adapter)
  impl ClkOp for HostResourceProvider   -> calls kclk::clk_get_*
  KclkConsumer(ClkHandle)               -> wraps kclk as a ClkConsumer
        |
        v
kclk (framework core)            <-- this crate
  ClkHwOps / ClkCore / ClkBuilder / ClkLookup / ClkTable / ClkHandle
```

Keeping `device-res` free of any `kclk` dependency lets the resource model stay
OS-neutral: porting the framework to another kernel only requires reimplementing
`ClkOp` and `ClkConsumer`, exactly as IRQ ports reimplement `IrqOp`.

## Components

| File | Role |
|------|------|
| `src/ops.rs` | `ClkHwOps` provider trait (the `clk_ops` equivalent), with the two-phase `prepare`/`enable` gate and the `RoundResult`-returning `round_rate`. |
| `src/core.rs` | `ClkCore` tree node + `ClkBuilder`; rate/accuracy caching, reference-counted gating, rate/parent write propagation, global clock lock, and rate-change notifiers. |
| `src/handle.rs` | `ClkHandle`, the consumer-side reference. |
| `src/provider.rs` | `ClkLookup` trait + global provider *list* keyed by `ProviderId` (`register_provider`, `clk_get_by_*`). |
| `src/lookup.rs` | `ClkTable`, a ready name- and index-keyed `ClkLookup`. |
| `src/reg.rs` | `ClkRegAccess` trait + `MmioClkReg` default impl. |
| `src/event.rs` | `ClkEvent`, `NotifierResult`, and the `ClkNotifier` subscriber type. |
| `src/basic/*` | Reusable clock types. |
| `src/flags.rs` | `ClkFlags` policy bits (`SET_RATE_PARENT`, `IS_CRITICAL`, ...). |
| `src/error.rs` | `ClkError` / `ClkResult`. |

## Core Abstractions

### `ClkHwOps` (provider side)

Every method receives only what the hardware needs; framework state (reference
counts, rate cache) is owned by `ClkCore`. The default implementations model a
pass-through node that neither gates nor changes rate, so a new type only
overrides the methods relevant to it.

The gate is two-phase on purpose:

- `prepare` / `unprepare` carry the slow, controller-global setup; like all
  callbacks they run under a spinlock and **must not sleep** — a PLL lock wait is
  a bounded busy-wait on a status bit, never a scheduler sleep;
- `enable` / `disable` must be atomic (toggle a gate bit only).

`round_rate` returns a [`RoundResult`] carrying both the closest achievable
rate *and* an optional `parent_target`: when a node cannot satisfy a request
on its own (a mux, or a fixed-factor node), it asks the framework to change
its parent's rate. The framework honors `parent_target` only when the node
carries [`ClkFlags::SET_RATE_PARENT`], then re-rounds the node against the new
parent rate — so a divider behind a mux can be reached by switching the mux's
upstream source.

### `ClkCore` / `ClkBuilder`

A node caches its rate and accuracy and carries a parent table (entries may be
`None` for an orphan not yet registered). `ClkBuilder` queries the hardware
(`recalc_rate` / `recalc_accuracy`) at registration to seed the cache.

Reference-counted gating propagates up the tree: `prepare`/`enable` act on the
parent first, then on this node, and roll the parent back if this node's
hardware op fails. Rate and parent changes refresh the cache and, when
[`ClkFlags::SET_RATE_PARENT`] is set, forward the request to the parent.

All sleepable mutations (`prepare`/`unprepare`/`set_rate`/`set_parent`/
`round_rate`) are serialized by a single global clock lock (see below); the
atomic gate (`enable`/`disable`) is *not* — it stays on the per-node spinlock so
it remains usable from contexts that cannot block.

[`RoundResult`]: ../src/rate.rs
[`ClkFlags::SET_RATE_PARENT`]: ../src/flags.rs

### `ClkLookup` / `ClkTable` / registry

A SoC has several clock controllers (CCU, PMUCRU, video CCU, ...). Each
registers its own `ClkLookup` under a `ProviderId` (its DT phandle) via
`register_provider`; the registry keeps a *list* of providers, not a single
one. Consumers resolve a clock by `(provider_id, index)` — the two cells of a
DT `clocks = <&ccu 5>` specifier — through `clk_get_by_index`, or by a
globally-unique name through `clk_get_by_name`. Two controllers may both
publish index 0 without colliding, because the registry keys on the provider id
first. This mirrors Linux's `of_clk_providers` list.

`ClkTable` is the recommended helper: it stores both a name map and an index
vector behind a spin lock, is `const`-constructible for a `static`, and answers
either query.

### Global clock lock

A single `SpinRaw` static (`CLK_LOCK` in `core.rs`) serializes every sleepable
mutation. This mirrors Linux's single `clk_prepare_lock`: any rate change
(which may recurse up the tree and reprogram a PLL) is atomic with respect to
other changes, so a concurrent `get_rate` reader never observes a half-updated
tree. Clock operations are not a hot path (boot/probe/DVFS), so one global lock
is acceptable; `get_rate` reads the per-node cache and never touches this lock.

The lock is a spinlock, not a sleepable mutex: callbacks must not sleep while
held. Current register-backed ops are non-sleeping; a future PLL whose
`prepare` truly needs to block will require migrating to a sleepable mutex.

### Rate-change notifiers

A driver registers a [`ClkNotifier`] on a clock to learn when its rate
changes. `set_rate` delivers [`ClkEvent::PreRateChange`] before the change
(a subscriber may veto by returning `NotifierResult::Stop`, which aborts with
`ClkError::Aborted`) and [`ClkEvent::PostRateChange`] after. Notifiers fire on
the node on which `set_rate` was called; intermediate parents changed during
upward propagation do not receive separate notifications.

Callbacks run while the global lock is held, so they must be quick, must not
sleep, and must not re-enter the framework.

[`ClkNotifier`]: ../src/event.rs
[`ClkEvent::PreRateChange`]: ../src/event.rs
[`ClkEvent::PostRateChange`]: ../src/event.rs

## Data Flow

### Registration (provider side, boot/probe time)

```text
controller driver
   |-- builds ClkHwOps (basic type or custom PLL)
   |-- ClkBuilder::new(name, ops).parent(..).flags(..).build()  -> Arc<ClkCore>
   |-- ClkTable::register(index, name, core)
   \-- register_provider(&TABLE)  (each controller its own id)
```

### Resolution (consumer side, driver probe)

```text
driver calls device_res::devm_clk_get(device, ClkSpec::by_name("uart"))
   -> kdriver::resource: ClkOp::get_clock -> kclk::clk_get_by_name
   -> ClkLookup::get_by_name -> Arc<ClkCore>
   -> wrapped as ClkConsumer, returned as device_res::Clk
```

### Two-phase gating

```text
ClkHandle::prepare_enable()
   -> ClkCore::prepare()  : parent.prepare(); ops.prepare();  prepare_count++
   -> ClkCore::enable()   : parent.enable();  ops.enable();   enable_count++
```

On the last release, `disable`/`unprepare` walk the same chain in reverse,
calling the hardware op once when the count reaches zero.

## Execution Context

- Registration, resolution, rate/parent changes, and prepare are **sleepable**
  probe/boot operations. They must not run in interrupt context.
- `enable`/`disable` are the atomic fast path and may run with IRQs disabled,
  matching Linux. A provider's `enable`/`disable` must not sleep.
- `prepare`/`unprepare` run serialized under the global `CLK_LOCK` (a spinlock),
  **not** a sleepable mutex: a provider's `prepare` must not sleep either. Slow
  work that needs to wait (a PLL lock) must be a **bounded busy-wait** polling a
  status bit, never a scheduler sleep.
- Hardware callbacks run *under* the global lock and the node's spinlock
  together, not outside them. They must be quick, non-sleeping, and must not
  re-enter the framework (see the `Re-entrancy` note on `ClkHwOps`).

## Concurrency

- Each node's mutable state is one `SpinNoIrq<ClkState>`; the notifier list is
  a separate `SpinNoIrq<Vec<ClkNotifier>>`.
- All sleepable mutations are serialized by the global `CLK_LOCK` (a `SpinRaw`),
  so a rate change — including parent recursion and re-rounding — is atomic
  w.r.t. other changes. `get_rate` reads the per-node cache without this lock.
- The atomic gate (`enable`/`disable`) intentionally bypasses `CLK_LOCK`, using
  only the per-node spinlock, so it stays usable where blocking is impossible.
- Parent recursion clones the parent `Arc` and releases the child state lock
  first, so propagation acquires locks strictly parent-before-child — no
  lock-ordering deadlock across the tree.

## Design Decisions

- **Two provider/consumer traits.** Mirrors `struct clk_hw`/`struct clk`:
  providers implement `ClkHwOps`, consumers only ever hold `ClkHandle` and never
  touch hardware details.
- **`ClkConsumer` as a trait object in `device-res`.** Clock consumer operations
  outnumber IRQ's, so dispatching through `Arc<dyn ClkConsumer>` is simpler than
  IRQ's token table, while `device-res` still imports no concrete framework type.
- **Register access behind `ClkRegAccess`.** Basic types that touch registers
  (gate/mux/divider) never depend on a host MMIO mechanism; `MmioClkReg` is one
  default impl, and tests inject an array-backed mock.
- **`RATE` is `u64` Hz**, matching Linux's `unsigned long`.
- **Per-node spinlocks plus a global `CLK_LOCK`.** Sleepable mutations take the
  global lock (atomic w.r.t. other changes); `enable`/`disable` stay on the
  per-node lock so they remain usable where blocking is impossible. Parent
  recursion releases the child lock first to keep lock ordering
  parent-before-child.

## Adding a Clock Controller Driver

This is the workflow for a new SoC clock controller (e.g. an RK3588 CCU, a
PL011-derived UART clock tree, a virtual `fixed-clock`).

### Step 1 — decide: basic types vs custom `ClkHwOps`

Prefer the reusable types whenever the hardware matches one of them; they are
field-tested by the unit tests and need no per-driver code:

| Hardware | Use |
|----------|-----|
| Fixed crystal (`osc24m`) | `FixedRate` |
| Fixed PLL ratio (×50, ÷1) | `FixedFactor` |
| A single enable bit | `Gate` |
| Parent selector field | `Mux` |
| Programmable divider field | `Divider` |
| Node with mux + divider + gate | `Composite` |

Write a custom `impl ClkHwOps` only when the clock is genuinely custom — e.g. a
PLL whose `set_rate` runs a multi-step lock sequence, or a fractional divider
with non-standard encoding.

### Step 2 — map the controller's register window

Acquire the controller's MMIO base the same way any driver does, then wrap it in
`MmioClkReg` (a `ClkRegAccess`). Each node is described by an `offset` within the
window plus a bit field, so a controller shares one `Arc<dyn ClkRegAccess>`
across all its nodes:

```rust
use alloc::sync::Arc;
use kclk::{ClkRegAccess, MmioClkReg};

// `base` is the virtual address returned by memspace::iomap_device / devm_iomap.
let reg: Arc<dyn ClkRegAccess> = Arc::new(MmioClkReg::new(ccu_vaddr));
```

### Step 3 — build the tree bottom-up with `ClkBuilder`

Parents must exist before children. Register each node, keeping the `Arc<ClkCore>`
so children can reference it:

```rust
use kclk::{ClkBuilder, ClkFlags, ClkTable, Divider, DividerFlags, FixedRate, Gate, GateFlags, Mux};

static CCU: ClkTable = ClkTable::new();

pub fn init_ccu(ccu_vaddr: usize) {
    let reg: Arc<dyn kclk::ClkRegAccess> = Arc::new(MmioClkReg::new(ccu_vaddr));

    // Root oscillator.
    let osc24m = ClkBuilder::new("osc24m", FixedRate::new(24_000_000).into_ops())
        .build()
        .expect("osc24m");
    CCU.register(0, "osc24m", osc24m.clone());

    // Fixed ×50 PLL on top of the crystal.
    let pll = ClkBuilder::new("pll1200", FixedFactor::new(50, 1).unwrap().into_ops())
        .parent(osc24m)
        .build()
        .expect("pll1200");
    CCU.register(1, "pll1200", pll.clone());

    // A UART clock: divider (bits[7:4], one-based) + gate (bit 11), one parent.
    let uart = ClkBuilder::new(
        "uart_clk",
        kclk::Composite::new(
            None,
            Some(Divider::into_ops(reg.clone(), 0x30, 4, 4, None, DividerFlags::ONE_BASED)),
            Some(Gate::into_ops(reg.clone(), 0x30, 11, GateFlags::SET_TO_ENABLE)),
        ),
    )
    .parent(pll)
    .build()
    .expect("uart_clk");
    CCU.register(5, "uart_clk", uart);
}
```

### Step 4 — install the provider once, during platform init

Each controller registers itself with `register_provider` (each under its own
`ProviderId`, usually the DT phandle) before any consumer resolves its clocks.
This belongs in the platform's early init (alongside
`install_resource_provider`):

```rust
// platforms/kplat-aarch64/src/init.rs
pub fn early_driver_init() {
    // ... timer, irq, boot console ...
    kdriver::install_resource_provider();

    let ccu_vaddr = /* discovered from device tree */;
    my_ccu_driver::init_ccu(ccu_vaddr);
    kclk::register_provider(&my_ccu_driver::CCU);
}
```

### Step 5 — consume a clock from any driver

Drivers never name `kclk` directly; they go through the `device-res` resource
model, exactly like MMIO and IRQ:

```rust
use device_res::{Clk, ClkSpec, devm_clk_get};
// or, for the device-object-bound helper:
use kdriver::devm_clk_get;

fn probe(uart: &DeviceObject) -> DriverResult {
    // by_name (a DT clock-names entry) or by the two cells:
    //   ClkSpec::by_phandle_index(phandle_of_ccu, 5)
    let clk: Clk = devm_clk_get(uart, ClkSpec::by_name("baudclk"))
        .map_err(map_res_err)?;
    clk.prepare_enable().map_err(map_res_err)?;
    // Full consumer surface (round_rate/set_rate/set_parent/register_notifier)
    // is available on Clk too, so drivers never bypass device-res for DVFS.
    let rate = clk.get_rate();
    // ... program the UART divisor from `rate` ...
    Ok(())
}
```

The handle is released automatically when the device's probe fails or it is
removed (the `devm_` cleanup drops the reference), so no explicit `put` is
needed. Note this only reclaims the **reference** — it does *not* call
`disable_unprepare`. A driver that enabled a clock must balance it in its own
remove path; otherwise the `prepare`/`enable` counts leak and the clock stays
on. (This matches Linux `devm_clk_get`.)

### Step 6 — custom PLL (when basic types are not enough)

Implement `ClkHwOps` directly. Override only the methods the hardware supports;
leave the rest at the pass-through default:

```rust
use alloc::sync::Arc;
use kclk::{ClkError, ClkHwOps, ClkResult, Rate, RoundResult};

pub struct MyPll { reg: Arc<dyn kclk::ClkRegAccess> }

impl ClkHwOps for MyPll {
    fn prepare(&self) -> ClkResult {
        // power up, then bounded busy-wait on the LOCK status bit
        // (callbacks run under a spinlock; never scheduler-sleep)
        Ok(())
    }
    fn unprepare(&self) -> ClkResult { Ok(()) }
    fn is_prepared(&self) -> ClkResult<bool> { /* read LOCK bit */ Ok(true) }
    fn recalc_rate(&self, parent: Rate) -> ClkResult<Rate> {
        // read FBK_DIV, REF_DIV, POST_DIV; compute VCO
        Ok(/* rate */)
    }
    fn round_rate(&self, rate: Rate, parent: Rate) -> ClkResult<RoundResult> {
        // A PLL can reprogram itself, so it satisfies the request on its own
        // and does not ask the parent to change.
        Ok(RoundResult { rate, parent_target: None })
    }
    fn set_rate(&self, rate: Rate, parent: Rate) -> ClkResult<Rate> {
        // program dividers, trigger relock, return actual rate
        Ok(rate)
    }
}
```

Register it exactly like a basic type: `ClkBuilder::new("my_pll",
Arc::new(MyPll { reg })).parent(..).build()`.

## Known Limitations

- **Callbacks must not sleep.** The global lock is a spinlock, not a sleepable
  mutex; a provider `prepare` that needs to wait must bounded-busy-wait a
  status bit. (A sleepable mutex is future work if a PLL genuinely needs
  scheduler sleep.)
- **Notifiers fire only on the entry node.** A `set_rate` that propagates up
  the tree delivers events only to the node on which it was called; intermediate
  parents reprogrammed during propagation do not receive separate notifications.
- **No orphan re-parenting.** Parents must be registered before children
  (`ClkBuilder::parent` takes an `Arc<ClkCore>`); there is no Linux-style
  deferred-probe adoption of an orphan slot.
- **Register writes are not atomic across shared words.** The default
  `ClkRegAccess::reg_modify` is a non-atomic read-modify-write. The atomic
  `enable`/`disable` path bypasses `CLK_LOCK`, so gate and rate fields that
  share a register word must use a controller with an atomic set/clear register
  pair (override `reg_modify`), or live in separate words.
- **No `clk_save_context` / `clk_restore_context`** for suspend/resume yet; the
  hooks would be straightforward but x-kernel has no PM framework to drive them.
- **`IGNORE_UNUSED` / `IS_CRITICAL` / `GET_RATE_NOCACHE`** are reserved flags
  (see `flags.rs`): accepted but not yet honored. `SET_RATE_GATE` *is* honored
  (gate around `set_rate`/`set_parent`).

## Testing

`src/tests.rs` exercises, under `--cfg unittest`:

- fixed-rate immutability and `round_rate`;
- fixed-factor scaling and zero-factor rejection;
- a three-level tree (osc → factor → divider) rate caching;
- divider `set_rate` field programming and `round_rate` rounding;
- gate enable/disable bit polarity (normal and inverted);
- mux parent selection and field masking;
- `ClkCore::set_parent` rate recomputation and out-of-range rejection;
- two-phase gating propagation with reference counting and failure roll-back;
- `Composite` dispatch to sub-nodes;
- provider name/index resolution and missing-clock handling;
- **multi-provider disambiguation**: two controllers publishing index 0;
- `SET_RATE_PARENT` pass-through propagation (mux → divider);
- rate-change notifiers: `Pre`/`Post` delivery around `set_rate`;
- notifier veto aborting a change with `ClkError::Aborted`;
- divider-aware upward propagation: a `FixedFactor` reprogramming its divider
  parent via `SET_RATE_PARENT`.

Run them with:

```bash
make UNITTEST=y UNITTEST_CRATE=kclk run
```
