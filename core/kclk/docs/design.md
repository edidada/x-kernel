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

## Linux Semantics Alignment

The basic types mirror Linux CCF (`include/linux/clk-provider.h`) in both
names and polarity. Verified against `drivers/clk/clk-*.c`:

| Ours | Linux | Same direction? |
|------|-------|-----------------|
| `DividerFlags::ONE_BASED` | `CLK_DIVIDER_ONE_BASED` | ✓ `div = raw` (default: `raw + 1`) |
| `DividerFlags::POWER_OF_TWO` | `CLK_DIVIDER_POWER_OF_TWO` | ✓ `div = 1 << raw`; encode is `floor(log2)` (`fls-1`) |
| `DivTableEntry(val, div)` | `struct clk_div_table` | ✓ sparse (val, div) pairs |
| `GateFlags::SET_TO_DISABLE` | `CLK_GATE_SET_TO_DISABLE` | ✓ default writes 1 to enable |
| `GateFlags::HIWORD_MASK` | `CLK_GATE_HIWORD_MASK` | ✓ |
| Mux `table[index] = raw` | `clk_mux` table | ✓ index → register value |
| `ClkFlags::SET_RATE_GATE` | `CLK_SET_RATE_GATE` | ✓ rejects `set_rate` when enabled |
| `ClkFlags::SET_PARENT_GATE` | `CLK_SET_PARENT_GATE` | ✓ rejects `set_parent` when prepared |
| `ClkFlags::SET_RATE_PARENT` | `CLK_SET_RATE_PARENT` | ✓ |
| `ClkFlags::IS_CRITICAL` / `IGNORE_UNUSED` | same names | ✓ honored by `clk_disable_unused` |
| `ClkFlags::GET_RATE_NOCACHE` | `CLK_GET_RATE_NOCACHE` | ✓ |
| enable-before-prepare | `__clk_enable` WARN | rejected with `ClkError::BadState` in every build profile (not a debug-only assertion); `enable()` failures are logged |

`ClkFlags` bit positions differ from the Linux C ABI (Rust bitflags, no FFI
sharing), so flags must be written by name, never by numeric value.

## Layering

`kclk` is the framework core. It is host-free in the driver-model sense: it
names no bus, no device model, no firmware parser, and no driver subsystem
crate, so it runs unchanged on mocks. It does depend on the kernel primitive
crates every framework core needs — `kspin` for the clock-tree locks (IRQ-safe
where the enable fast path touches them, preemption-only elsewhere),
`ksync` for the sleepable mutation mutex, and (unittest builds only, for the
concurrency stress tests) `ktask`. The integration surfaces around it are
split the way the driver subsystem splits every capability:

```text
provider drivers (drv/clk/*: fixed-clock, fixed-factor, SoC controllers)
  platform drivers; their probe builds the hardware ops and registers outputs
        |  kclk::register_provider / register_table
        v
kclk (framework core)            <-- this crate
  ClkHwOps / ClkCore / ClkBuilder / ClkLookup / ClkTable / ClkHandle
        ^
consumer drivers (serial, SDMMC, ... through their controller drivers)
  the clock capability contract (driver-os-api `ClockOp`, landed with the
  first consumer drivers) resolves (provider id, index) and calls
  kclk::clk_get_by_index / clk_get_by_name
```

Keeping `kclk` free of device-model and firmware-parsing dependencies means
the framework, its basic types, and its semantics are testable in isolation,
and provider/consumer integration evolves independently of the core.

## Components

| File | Role |
|------|------|
| `src/ops.rs` | `ClkHwOps` provider trait (the `clk_ops` equivalent), with the two-phase `prepare`/`enable` gate and the `RoundResult`-returning `round_rate`. |
| `src/core.rs` | `ClkCore` tree node + `ClkBuilder`; rate/accuracy caching, reference-counted gating, rate/parent write propagation, global clock lock, and rate-change notifiers. |
| `src/handle.rs` | `ClkHandle`, the consumer-side reference. |
| `src/provider.rs` | `ClkLookup` trait + global provider *list* keyed by `ProviderId` (`register_provider`, `clk_get_by_*`). |
| `src/lookup.rs` | `ClkTable`, a ready name- and index-keyed `ClkLookup`. |
| `src/reg.rs` | `ClkRegAccess` trait (register access, with an optional window bound) + `NullClkReg`; the MMIO implementation lives in the host adapter. |
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

- `prepare` / `unprepare` carry the slow, controller-global setup and run under
  the global **sleepable mutex**, so they **may sleep** (a PLL lock wait can
  block);
- `enable` / `disable` must be atomic (toggle a gate bit only) and run under the
  global IRQ-saving `CLK_ENABLE_LOCK`, so they **must not sleep**.

Enabling a clock whose `prepare_count` is zero is rejected in every build
profile with `ClkError::BadState`, and the `enable()` wrapper logs the failure;
Linux only warns from `__clk_enable`, so this is deliberately stricter.

`round_rate` returns a [`RoundResult`] carrying both the closest achievable
rate *and* an optional `parent_target`: when a node cannot satisfy a request
on its own (a mux, or a fixed-factor node), it asks the framework to change
its parent's rate. The framework honors `parent_target` only when the node
carries [`ClkFlags::SET_RATE_PARENT`], then re-rounds the node against the new
parent rate — so a divider behind a mux can be reached by switching the mux's
upstream source.

`determine_rate` is the multi-parent analogue of `round_rate` (mirroring Linux
`clk_ops.determine_rate`): it receives a [`ClkRateRequest`] seeded with the
clamped request and the current parent, plus the full candidate parent list, and
may select a `best_parent_index` to switch sources (a mux choosing the closest
input). The core honors a parent switch before programming the rate. The default
implementation delegates to `round_rate`, so existing nodes keep the legacy
upward-propagation contract unchanged.

### `ClkCore` / `ClkBuilder`

A node caches its rate and accuracy and carries a parent table (entries may be
`None` for an orphan not yet registered). `ClkBuilder` queries the hardware
(`recalc_rate` / `recalc_accuracy`) at registration to seed the cache. An
orphan slot declared via `ClkBuilder::parent_name(name)` is filled later when a
clock named `name` registers, at which point the orphan's cached rate is
refreshed and all active prepare/enable references are migrated to the new
parent. Descendant caches are then refreshed top-down. If any migration or
recalculation step fails, the child remains an orphan and the provider can be
retried without leaked references.

Reference-counted gating propagates up the tree: `prepare`/`enable` act on the
parent first, then on this node, and roll the parent back if this node's
hardware op fails. Rate and parent changes refresh the cache and, when
[`ClkFlags::SET_RATE_PARENT`] is set, forward the request to the parent.

Sleepable mutations (`prepare`/`unprepare`/`set_rate`/`set_parent`/
`round_rate`) are serialized by `CLK_LOCK`; the IRQ-safe `enable`/`disable`
path uses `CLK_ENABLE_LOCK` instead (see below). `get_rate` reads the per-node
cache and does not take either global lock.

[`RoundResult`]: ../src/rate.rs
[`ClkRateRequest`]: ../src/rate.rs
[`ClkFlags::SET_RATE_PARENT`]: ../src/flags.rs

### `ClkLookup` / `ClkTable` / registry

A SoC has several clock controllers (CCU, PMUCRU, video CCU, ...). Each
registers its own `ClkLookup` under a `ProviderId` (its DT phandle) via
`register_provider`; the registry keeps a *list* of providers, not a single
one. Consumers resolve a clock by `(provider_id, index)` — the two cells of a
DT `clocks = <&ccu 5>` specifier — through `clk_get_by_index`. This is the
canonical DT path: a consumer's `clock-names` connection name (consumer-local,
not unique) is resolved by the platform into that `(phandle, index)` pair before
reaching kclk. `clk_get_by_name` is a separate convenience that looks a clock up
by its SoC name (the provider output name, globally unique), not by a
`clock-names` entry. Two controllers may both publish index 0 without
colliding, because the registry keys on the provider id first. This mirrors
Linux's `of_clk_providers` list.

`ClkTable` is the recommended helper: it stores both a name map and an index
vector behind a spin lock, is `const`-constructible for a `static`, and answers
either query.

`register_table` publishes the provider table only after the complete graph is
built, parent references are resolved, cycles are rejected, and cached rates
are refreshed. A late failure removes exactly the batch nodes, restores any
pre-existing orphan slots adopted by the batch, and releases every migrated
prepare/enable reference. The restored orphan can therefore be adopted by a
later retry.

### Global clock locks

The sleepable mutex (`CLK_LOCK` in `core.rs`) serializes every slow mutation.
This mirrors Linux's `clk_prepare_lock`: a rate or topology change (which may
recurse up the tree and reprogram a PLL) cannot interleave with another slow
change. Lockless cache readers may observe the old or new value while a
multi-node transition is in progress; callers that require a transaction
boundary must coordinate at a higher level.

`CLK_LOCK` is **sleepable**: providers may sleep in `prepare`/`unprepare` and
`set_rate` callbacks that run only under it. The public slow paths must never be
entered from atomic or interrupt context, and the mutex is not re-entrant —
re-entering the framework while holding it panics. `set_parent`, rate/accuracy
recalculation and enable/disable callbacks must remain non-sleeping because a
topology commit also invokes them while holding `CLK_ENABLE_LOCK`.

The IRQ-saving spinlock `CLK_ENABLE_LOCK` serializes `enable`/`disable` and the
atomic portion of parent attachment/reparenting. A topology transaction always
takes locks in `CLK_LOCK` → `CLK_ENABLE_LOCK` order and holds the spinlock from
the enable-count snapshot through new-parent bring-up, mux programming,
parent-index/cache commit, back-edge update, and old-parent disable. Slow
unprepare callbacks run after dropping the spinlock while `CLK_LOCK` remains
held.

### Rate-change notifiers

A driver registers a [`ClkNotifier`] on a clock to learn when its rate
changes. `set_rate` delivers [`ClkEvent::PreRateChange`] before the change
(a subscriber may veto by returning `NotifierResult::Stop`, which aborts with
`ClkError::Aborted`) and [`ClkEvent::PostRateChange`] after. Notifiers fire on
**every** clock whose rate is actually reprogrammed — the node `set_rate` was
called on and each parent reprogrammed on its behalf during upward
`SET_RATE_PARENT` propagation. A `Pre` veto on any node in the chain aborts
the whole change.

Registration returns an id (`ClkNotifierId`) that can be passed to
`unregister_notifier` to remove the subscriber.

Notifier callbacks run while the global sleepable mutex is held; they may sleep
but must be quick and must not re-enter the framework (re-entrancy panics).

### Rate range and unused-clock gate-down

A consumer can constrain a clock's rate via `set_rate_range(min, max)`; later
`set_rate` / `round_rate` requests are clamped into that range (an inverted
range is rejected with `InvalidRate`). The current rate is not retroactively
changed.

Every registered clock is kept in a global list; the late-init
[`clk_disable_unused`] pass gates off any clock whose hardware reports enabled
but which holds no consumer references, skipping clocks marked
[`ClkFlags::IS_CRITICAL`] or [`ClkFlags::IGNORE_UNUSED`]. [`GET_RATE_NOCACHE`]
makes `get_rate` re-read the hardware rate instead of the cache.

[`clk_disable_unused`]: ../src/core.rs
[`ClkFlags::IS_CRITICAL`]: ../src/flags.rs
[`ClkFlags::IGNORE_UNUSED`]: ../src/flags.rs
[`GET_RATE_NOCACHE`]: ../src/flags.rs

[`ClkNotifier`]: ../src/event.rs
[`ClkEvent::PreRateChange`]: ../src/event.rs
[`ClkEvent::PostRateChange`]: ../src/event.rs

## Data Flow

### Registration (provider side, platform-driver probe)

```text
DT node (compatible = "fixed-clock" or SoC-specific controller)
   \-- platform backend enumerates nodes whose compatible matches a
   |   registered driver's firmware spec (FirmwareMatchSpec)
   \-- driver.probe_device(ctx):
         |-- reads the node's firmware properties (#clock-cells,
         |   clock-output-names, clock-frequency) via fwnode
         |-- builds ClkHwOps (basic type or custom PLL) with ClkBuilder
         \-- kclk::register_provider(lookup)
               (the lookup reports the provider id itself — `ClkLookup::
                provider_id()`, the node's firmware identity — and consumers
                resolve their `clocks` specifiers against the same id)
```

### Resolution (consumer side, driver probe)

A consumer never resolves a DT `clock-names` connection name through kclk —
that name is consumer-local (many devices can have one named `"bus"`). The
consumer resolves the connection itself: it reads its firmware node's
`clocks` property, dereferences the phandle to the provider node, and turns
each specifier into `(provider id, index)`. The clock capability contract
(`driver-os-api` `ClockOp`) maps that pair onto kclk:

```text
driver: clk consumer helper (connection name or index)
   -> ClockOp::clk_get((provider id, index))
   -> kclk::clk_get_by_index(provider id, index)
   -> ClkLookup::get_by_index -> Arc<ClkCore>
   -> enabled-clock guard returned to the driver
```

Positional access (`clk_at(index)`) mirrors the `clocks` declaration order;
the provider output name (registered via `ClkTable::register`) is a
provider-side identity, distinct from the consumer-local connection names.

> **Ordering.** The provider must be registered before a consumer resolves
> its clocks. Device-tree order does not guarantee this on every board
> (several RK3588 boards enumerate controllers before the CRU). A
> deferred-probe/wake mechanism at the framework boundary is planned as a
> follow-up; until then, consumers that resolve before their provider fail
> their probe and the device stays unclaimed.

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
- `prepare`/`unprepare`/`set_rate`/`set_parent`/`round_rate` public operations
  are serialized by the sleepable `CLK_LOCK`; only the provider callbacks that
  never nest `CLK_ENABLE_LOCK` (`prepare`/`unprepare`/`set_rate`) may actually
  sleep. These paths must not run in interrupt context.
- `enable`/`disable` are serialized by a separate `CLK_ENABLE_LOCK` — an
  IRQ-saving spinlock (Linux's `clk_enable_lock`) — so they stay usable from
  interrupt context while remaining mutually exclusive with `set_parent`'s
  reference migration. The provider `enable`/`disable` callback runs with IRQs
  disabled and must be quick and non-sleeping.
- A provider's other callbacks (`set_parent`, `recalc_rate`, `is_enabled`,
  `init`, ...) may be invoked without the sleepable lock or with IRQs disabled
  and must not sleep. Callbacks must not
  re-enter the framework — re-entrancy panics (see the `Re-entrancy` note on
  `ClkHwOps`).

## Concurrency

- Each node's mutable state is one `SpinNoIrq<ClkState>`, and the parent table
  is a `SpinNoIrq` too: both are read on the `enable_locked`/`disable_locked`
  fast path, which is reachable from interrupt context, so they keep IRQ
  masking.
- Everything reached only from the sleepable `CLK_LOCK` mutations is a
  `SpinNoPreempt`: the children list, `self_ref`, the notifier list, the global
  clock list, the orphan list, the provider registry, and the `ClkTable`
  index/name maps. Preemption off is all they need — masking IRQs there would
  be masking without an interrupt-context contender.
- All sleepable mutations are serialized by the global `CLK_LOCK` (a
  `ksync::Mutex`), so a rate change — including parent recursion and
  re-rounding — is atomic w.r.t. other changes. `get_rate` reads the per-node
  cache without this lock.
- The atomic gate (`enable`/`disable`) takes a separate `SpinNoIrq`
  `CLK_ENABLE_LOCK` (Linux's two-lock model), so it neither races a concurrent
  `set_parent` migration nor loses interrupt-context usability. Lock order is
  `CLK_LOCK` → `CLK_ENABLE_LOCK`; the reparent migration nests them in exactly
  that order. The concurrency regressions exercise the migration against
  task-context `prepare_enable`/`disable_unprepare` (which take both locks).
- Parent recursion clones the parent `Arc` and releases the child state lock
  first, so propagation acquires locks strictly parent-before-child — no
  lock-ordering deadlock across the tree.

## Design Decisions

- **Two provider/consumer traits.** Mirrors `struct clk_hw`/`struct clk`:
  providers implement `ClkHwOps`, consumers only ever hold `ClkHandle` and never
  touch hardware details.
- **`ClkOps` as a trait object.** Clock consumer operations outnumber IRQ's, so
  dispatching through `Arc<dyn ClkHwOps>` is simpler than a token table, and the
  capability-contract layer needs no concrete framework type.
- **Register access behind `ClkRegAccess`.** Basic types that touch registers
  (gate/mux/divider) never depend on a host MMIO mechanism: this crate owns none.
  The host supplies the accessor (`IoMapClkReg` in the X-Kernel provider adapter,
  built over the validated `driver_os_api::mmio::IoMap`), tests inject an array-backed
  mock, and `NullClkReg` serves purely computational nodes. Implementations that
  know their window report it through `window_bytes()`, and node constructors
  reject a descriptor that would read or write outside it (see "Register
  windows" below).
- **`RATE` is `u64` Hz**, matching Linux's `unsigned long`.
- **Two global locks, mirroring Linux.** `CLK_LOCK` (a sleepable
  `ksync::Mutex`) serializes the slow mutations — `prepare`/`unprepare`/
  `set_rate`/`set_parent`/`round_rate` — so rate changes are atomic w.r.t.
  each other. Only `prepare`/`unprepare`/`set_rate` provider callbacks may
  sleep; topology callbacks also execute under `CLK_ENABLE_LOCK`.
  `CLK_ENABLE_LOCK` (an IRQ-saving spinlock) serializes `enable`/`disable`
  propagation, keeping the atomic fast path usable from interrupt context
  (console/polling drivers). Lock order is `CLK_LOCK` → `CLK_ENABLE_LOCK`
  (the reparent ref-migration takes both); the reverse never happens.
  Per-node spinlocks protect individual fields beneath the globals, and parent
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

Acquire the controller's MMIO window the same way any driver does, and wrap the
resulting `IoMap` in the host adapter's `ClkRegAccess` implementation. Each node
is described by a byte `offset_bytes` within the window plus a bit field, so a
controller shares one `Arc<dyn ClkRegAccess>` across all its nodes:

```rust
use alloc::sync::Arc;
use driver_os_api::IoMap;
use driver_os_api_xkernel::IoMapClkReg;
use kclk::ClkRegAccess;

// `map_with` bounds-checks and align-checks every access, and reports the
// region size as the register window the clock framework validates against.
let map = IoMap::map_with(&XKernelResourceProvider, region, "ccu")?;
let reg: Arc<dyn ClkRegAccess> = Arc::new(IoMapClkReg::new(map));
```

### Register windows

`ClkRegAccess::window_bytes` reports the size of the window an accessor can
reach, and every node constructor (`Gate`, `Mux`, `Divider`) rejects a
descriptor whose word (`offset_bytes .. offset_bytes + 4`) leaves it, or that is
not word-aligned, with `ClkError::InvalidValue`. A table-authoring mistake is
therefore reported by `register_table` during probe instead of faulting on the
first access. Accessors that cannot express a window (test doubles,
`NullClkReg`) return `None` and keep their descriptors unchecked.

### Step 3 — build the tree bottom-up with `ClkBuilder`

Parents must exist before children. Register each node, keeping the `Arc<ClkCore>`
so children can reference it:

```rust
use kclk::{ClkBuilder, ClkFlags, ClkTable, Divider, DividerFlags, FixedRate, Gate, GateFlags, Mux};

// The provider id is the controller node's firmware id; `ClkTable::provider_id`
// reports it to the registry, so consumers resolve `clocks` phandles to it.
static CCU: ClkTable = ClkTable::new(0x1 /* controller phandle */);

pub fn init_ccu(ccu_vaddr: usize) -> kclk::ClkResult<()> {
    let reg: Arc<dyn kclk::ClkRegAccess> = Arc::new(IoMapClkReg::new(ccu_map));

    // Root oscillator.
    let osc24m = ClkBuilder::new("osc24m", FixedRate::new(24_000_000).into_ops()).build()?;
    CCU.register(0, "osc24m", osc24m.clone());

    // Fixed ×50 PLL on top of the crystal.
    let pll = ClkBuilder::new("pll1200", FixedFactor::new(50, 1)?.into_ops())
        .parent(osc24m)
        .build()?;
    CCU.register(1, "pll1200", pll.clone());

    // A UART clock: zero-based divider (bits[7:4], raw 0 = ÷1) + gate
    // (bit 11), one parent. DividerFlags semantics match Linux:
    // `ONE_BASED` sets `divider = raw` (CLK_DIVIDER_ONE_BASED); the
    // default is zero-based, `divider = raw + 1`. Every basic-type
    // constructor validates its register description and returns
    // `ClkError::InvalidValue` for a width or parent count the framework
    // cannot represent, so a malformed description never panics.
    let uart = ClkBuilder::new(
        "uart_clk",
        kclk::Composite::new(
            None,
            Some(Divider::into_ops(
                reg.clone(),
                0x30,
                4,
                4,
                None,
                DividerFlags::empty(),
            )?),
            Some(Gate::into_ops(reg.clone(), 0x30, 11, GateFlags::empty())),
        )
        .into_ops(),
    )
    .parent(pll)
    .build()?;
    CCU.register(5, "uart_clk", uart);
    Ok(())
}
```

### Step 4 — register the provider from a platform driver

A clock controller is a platform device like any other: its DT node is
enumerated by the platform backend (matched through the driver's
`FirmwareMatchSpec`), and its driver registers the provider in `probe` —
the same lifecycle as serial/block/net drivers. A node with
`compatible = "fixed-clock"` is handled by the `clk-fixed-clock` provider
driver; SoC-specific controllers follow the same pattern with their own
compatible and MMIO window.

```rust
// A fixed-clock provider driver (shape):
impl DeviceDriver for FixedClockDriver {
    type Data = FixedClockData;

    fn probe_device(&self, ctx: &ProbeContext<'_>) -> DriverResult<Self::Data> {
        let node = ctx.device().firmware_node()
            .ok_or(DriverError::InvalidInput)?;
        let frequency = of::clock_frequency(&node)?;       // fwnode helper
        let core = ClkBuilder::new(
            node.name().unwrap_or("fixed-clock"),
            FixedRate::new(frequency).into_ops(),
        ).build().map_err(|_| DriverError::Io)?;

        let lookup: &'static FixedClockLookup = Box::leak(Box::new(
            FixedClockLookup { provider_id: node.id().raw(), core: core.clone() }));
        // One argument: the provider id comes from `lookup.provider_id()`.
        // Registering the same id twice replaces nothing — the registry is
        // keyed by id, and a duplicate registration is a driver bug.
        kclk::register_provider(lookup);
        Ok(FixedClockData { _core: core })
    }
}

// An SoC controller driver (e.g. rk3588-cru) — same shape, plus MMIO:
impl DeviceDriver for Rk3588CruDriver {
    type Data = CruData;
    fn probe_device(&self, ctx: &ProbeContext<'_>) -> DriverResult<Self::Data> {
        let map = /* devm-iomap the controller's register window */;
        let reg: Arc<dyn ClkRegAccess> = Arc::new(IoMapClkReg::new(map));
        let ccu = build_ccu_tree(&reg)?;                    // kclk branches
        kclk::register_provider(&ccu);
        Ok(CruData { _regs: regs, _ccu: ccu })
    }
}
```

The `driver-init` composition registers every compiled-in provider driver
before enumeration, exactly like other platform drivers.
### Step 5 — consume a clock from a driver

Consumers go through the clock capability contract (`driver-os-api` `ClockOp`,
implemented by the host adapter over kclk), not through kclk directly:

```rust
// From a consumer driver probe:
fn probe_device(&self, ctx: &ProbeContext<'_>) -> DriverResult<Self::Data> {
    let node = ctx.device().firmware_node().ok_or(DriverError::InvalidInput)?;
    let clk = ctx.clk_by_name(&node, "uartclk")?;   // resolves (provider, index)
    clk.prepare_enable()?;
    let rate = clk.get_rate();
    // ... program the UART divisor from `rate`; store the guard in Data ...
}
```

The enabled-clock guard released on drop; the binding-retirement cleanup
registered through `ProbeContext::add_cleanup` disables and unprepares the
clock when the binding is torn down, so a driver cannot leak prepare/enable
counts across probe failure or removal.

### Step 6 — custom PLL (when basic types are not enough)

Implement `ClkHwOps` directly. Override only the methods the hardware supports;
leave the rest at the pass-through default:

```rust
use alloc::sync::Arc;
use kclk::{ClkError, ClkHwOps, ClkResult, Rate, RoundResult};

pub struct MyPll { reg: Arc<dyn kclk::ClkRegAccess> }

impl ClkHwOps for MyPll {
    fn prepare(&self) -> ClkResult {
        // Power up, then wait for the LOCK status bit. `prepare` runs under the
        // global sleepable mutex, so it may block (scheduler-sleep) here.
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

- **Topology and gate callbacks must not sleep.** `enable`/`disable`,
  `set_parent`, and reparent-time `recalc_rate`/`recalc_accuracy` run under
  the IRQ-saving `CLK_ENABLE_LOCK` spinlock (framework `enable`/`disable` are
  interrupt-context usable). The otherwise lock-free callbacks (`recalc_rate`,
  `is_enabled`, `init`, ...) must also stay non-sleeping. Only the `CLK_LOCK`-
  only provider callbacks (`prepare`/`unprepare`/`set_rate`) may sleep.
- **Notifiers fire on every reprogrammed clock.** A `set_rate` that propagates
  up the tree delivers Pre/Post on the entry node and on each parent
  reprogrammed during propagation (matching Linux `clk_change_rate`); a `Pre`
  veto anywhere aborts the whole change.
- **Firmware clock specifiers currently carry zero or one provider cell.** DT
  parsing supports `#clock-cells = <0>` and `<1>`. Providers with two or more
  cells require an extensible/raw specifier representation before they can be
  modeled without losing provider-defined arguments.
- **Register writes are not atomic across shared words.** The default
  `ClkRegAccess::reg_modify` is a non-atomic read-modify-write. The atomic
  `enable`/`disable` path bypasses `CLK_LOCK`, so gate and rate fields that
  share a register word must use a controller with an atomic set/clear register
  pair (override `reg_modify`), or live in separate words.
- **No `clk_save_context` / `clk_restore_context`** for suspend/resume yet; the
  hooks would be straightforward but x-kernel has no PM framework to drive them.
- **No phase / duty-cycle ops** (`clk_set_phase`, `clk_set_duty_cycle`); those
  are only needed by specific device classes (SDIO/audio/backlight) and can be
  added as additive hooks when a driver requires them.

## Testing

`ut-tests/lib.rs` and `ut-tests/invariants.rs` exercise, under `--cfg unittest`:

- fixed-rate immutability and `round_rate`;
- fixed-factor scaling and zero-factor rejection;
- a three-level tree (osc → factor → divider) rate caching;
- divider `set_rate` field programming and `round_rate` rounding;
- gate enable/disable bit polarity (normal and inverted);
- mux parent selection and field masking;
- `ClkCore::set_parent` rate recomputation and out-of-range rejection;
- two-phase gating propagation with reference counting and failure roll-back;
- split `prepare`/`enable` consumer APIs;
- `get_parent`, `set_rate_range` clamping, and `ClkTable::unregister`;
- rate-change notifiers: `Pre`/`Post` delivery around `set_rate`, veto abort,
  and unregister;
- the `init` hook running at registration;
- `determine_rate` multi-parent selection (a mux switching to the closest source);
- orphan adoption via `parent_name` (a parent registered after its child);
- active orphan adoption and failed table-registration rollback, including
  reference migration and later retry;
- `clk_disable_unused` gating idle hardware-on clocks and honoring
  `IS_CRITICAL`/`IGNORE_UNUSED`;
- `Composite` dispatch to sub-nodes;
- provider name/index resolution and missing-clock handling;
- **multi-provider disambiguation**: two controllers publishing index 0;
- `SET_RATE_PARENT` pass-through propagation (mux → divider);
- transactional rollback when rate selection reparents before programming
  fails;
- divider-aware upward propagation: a `FixedFactor` reprogramming its divider
  parent via `SET_RATE_PARENT`.

Run them with:

```bash
make unittest UNITTEST_CRATE=kclk
```
