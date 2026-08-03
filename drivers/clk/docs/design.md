# `drivers/clk` — OS-neutral clock subsystem

Status: **Tier 0 (rate query)**. This crate gives drivers a platform-neutral
way to ask "what is the rate of my clock?" instead of hard-coding it. It is the
first concrete step of the clock roadmap laid out in
`docs/跨内核驱动框架v3.md` (Layer 1 capability domain extension, the
"Desc + Op Trait + RAII Handle + Provider" pattern already proven by
`drivers/device-res`).

## Why clocks are not just another `device-res` provider

`device-res` installs **one** global provider per capability (MMIO, IRQ, DMA).
A clock tree is different: it has **many producers** — one per clock-output DT
node. An RK3588 board has at least three: the CRU (`rockchip,rk3588-cru`), an
SCMI clock protocol (`arm,scmi-smc`), and every `fixed-clock` crystal
(`xin24m`, `xin32k`, ...). Each owns a namespace of clock ids.

So the registry is **keyed by DT phandle**, not a single global. A consumer
resolves a clock in two steps:

1. Parse its own DT `clocks` / `clock-names` into a `(provider phandle,
   specifier cells)` pair. The specifier is the `#clock-cells` u32 words after
   the phandle (`<1>` on RK3588, `<0>` for a `fixed-clock`).
2. Look the provider up by phandle and call `get_rate(specifier)`.

## Layering

```text
drivers/clk/                        OS-neutral framework (this crate)
  src/lib.rs          ClkProvider trait, Clk handle, re-exports, tests
  src/registry.rs     phandle-keyed multi-provider registry
  src/consumer.rs     get_rate / clk_get + clocks/clock-names parsing
  src/providers.rs    FixedClockProvider + register_fixed_clocks()
drivers/kdriver/.../clk/<soc>.rs    SoC-specific provider glue (Tier 1+, TBD)
```

- **Neutral framework** (this crate): knows no SoC. Always compiled.
- **Standard providers** (also this crate): `fixed-clock` is a DT binding every
  platform uses, so it lives here and serves all of them at once. A future SCMI
  clock client belongs here too (SCMI is an ARM standard, not RK3588-specific).
- **SoC providers** (per-SoC glue, gated by `KFEAT_DRIVER_CLK_<SOC>`): a
  Rockchip CRU driver, a bcm2711 clock driver, ... These register the clocks
  only their platform needs.

The bus pre-parses each device's `clocks`/`clock-names` into `ResourceDesc::Clock`
resources at enumeration (just like `reg`/`interrupts`), so the consumer call is a
`devm_*` helper against `&dyn DeviceResource` — identical on every platform:

```rust
let ciu = clk::devm_clk_get(device, "ciu")?;   // -> Clk (RK3588: SCMI, bcm2711: its clk, QEMU: fixed)
let hz = ciu.rate();                           // Option<u64>
```

## Tier 0 scope — read-only rate query

Only `ClkProvider::get_rate` exists. This retires the hard-coded clock-rate
gaps recorded in driver bring-up notes:

- `drivers/kdriver/.../block/sdmmc.rs` — the `SDMMC_BUS_HZ = 50_000_000`
  constant is now a **fallback**; `ciu` is first queried via `clk::devm_clk_get`.
- Future: UART baud divisors (`clk::devm_clk_get(device, "baudclk")`).

Tier 0 deliberately does **not** include clock enable/disable, `set_rate`,
reparenting, or rate-change propagation. The hooks are sketched as comments on
`ClkProvider` so the consumer API does not change shape when they arrive.

## SCMI clock provider (RK3588) — rate query

The RK3588 SD-card `ciu` clock is SCMI-owned (`mmc@fe2c0000`,
`clocks = <&scmi_clk 9>`). A minimal SCMI-over-SMC client now resolves it:

- `platforms/kplat-aarch64/src/peripherals/scmi.rs` — `ScmiClockProvider`
  implements `ClkProvider::get_rate` by framing a `CLOCK_RATE_GET` (clock
  protocol `0x14`, message `0x06`) in the shared SRAM buffer, ringing the
  `arm,smc-id` (`0x82000010`) doorbell via `smccc::smc_call`, and parsing the
  synchronous reply. Transport params (`arm,smc-id`, `shmem`) and the provider
  phandle (`protocol@14`) are read from the DT at `early_driver_init`, so nothing
  is hard-coded. The shared buffer is serialised by a spinlock.
- Registered under the `protocol@14` phandle, so the neutral consumer lookup
  `clk::devm_clk_get(device, "ciu")` now hits it instead of the 50 MHz fallback.

SCMI-over-SMC is inherently AArch64, so the provider lives in `kplat-aarch64`,
not the neutral crate. It is read-only (`CLOCK_RATE_GET`); `CLOCK_RATE_SET` and
the reset protocol (`protocol@16`) are deferred.

> **P6 status (Orange Pi 5 Plus):** the transport is correct (`base VERSION`
> returns `0x20000`). An early on-hardware failure — every command except
> `VERSION` returned `PROTOCOL_ERROR (-10)` — was misread as a firmware stub.
> The real cause was a **message-header bit-layout bug** here:
> `message_id`/`token` positions were swapped. `VERSION` masked it (its
> `message_id=0`/`token=0` produce the same header under either layout), so
> `VERSION` worked while every `message_id != 0` command silently became
> `VERSION` to TF-A. The correct layout is `message_id[7:0] | message_type[9:8]
> | protocol_id[17:10] | token[27:18]` (ARM SCMI spec / Linux `pack_scmi_header`
> / the upstream Rust `arm-scmi-rs`). With it fixed (plus `dsb sy` barriers
> around the SMC and `RATE_GET` used directly as the ciu reference, no `÷2`),
> `clk::devm_clk_get(device, "ciu")` resolves to the true ciu rate and the sdmmc
> driver uses it instead of the 50 MHz fallback. Lesson + diagnostic method:
> `docs/ai/skills/linux-driver-port/knowledge/clk-subsystem.md`.

## Tier 1 / Tier 2 (roadmap, not implemented)

- **Tier 1 — gate/divider/mux primitives.** Port the six generic Linux
  primitives (`clk-gate`, `clk-divider`, `clk-mux`, `clk-fixed-factor`,
  `clk-fixed-rate`, `clk-composite`) with a shared HIWORD-mask register helper,
  and add `enable`/`disable`/`set_rate` to the trait. Enables real `set_rate`
  on CRU-owned clocks (eMMC/SDIO dividers). PLLs treated as fixed-rate sources
  read from hardware. ~1–2k lines.
- **Tier 2 — full CCF.** Rate-change tree walk, prepare/enable refcounting
  across parents, `clk-pll.c` (rk3588 families), notifiers, the full
  `clk-rk3588.c` branch table, the reset controller. Only justified for CPU
  frequency scaling / aggressive runtime PM. ~10–15k lines.

## Adjacent gap: reset controller

The same CRU DT node exposes `#reset-cells = <1>` and several device nodes
carry `resets`/`reset-names`; x-kernel has no reset framework either. A minimal
`ResetOp` (assert/deassert) should be designed alongside Tier 1 clk and share
the phandle-keyed registry, since both live in the same CRU provider.

## Testing

Unit tests (`#[cfg(unittest)]` in `lib.rs`) pin the framework contract:
providers route by phandle, the specifier reaches the provider, re-registration
shadows, and `Clk::rate` delegates. The DT-parsing path is not unit-tested
(needs a real DTB) and is validated on hardware at probe time.
