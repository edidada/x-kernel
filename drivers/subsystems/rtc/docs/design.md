# subsys-rtc — Design

## Purpose and scope

`src/lib.rs` defines the real-time-clock class contract and the boot
wall-clock service; `src/class.rs` owns the const-constructed class registry.
The crate does not drive hardware, map MMIO, or own discovery: drivers
publish [`RtcDevice`](../src/lib.rs) implementations into the class, and the
composition root reads the class after the probe storm.

## Architecture

The crate follows the L0-b contract/service split used by the other
subsystems:

| Part | Kind | Host-free |
| --- | --- | --- |
| `RtcDevice` trait, `RtcDeviceImpl` payload, `RTC` registry | contract | yes |
| `hctosys()` | service | uses `ktime` |

A class instance is the Linux `struct class rtc_class` analogue: one static
registry, one publish/find/subscribe/withdraw protocol from `driver-base`.
Drivers publish with `dev.publish(&RTC, device, name)` and keep the returned
`Publication` in their `DriverOps::Data`; dropping it withdraws the entry
after the last in-flight read.

## Boot flow

1. `driver-init` registers the per-backend RTC drivers at initcall level L3.
2. L4 enumeration turns firmware nodes (DT `arm,pl031`,
   `google,goldfish-rtc`, `loongson,ls7a-rtc`) and board-static devices
   (`rtc-cmos`, `rtc-ls7a`) into platform devices.
3. L5 probes them; each successful probe publishes into `RTC`.
4. `driver-init` calls `hctosys()`; the first device that yields a sample
   seeds `ktime::initialize_realtime`.

This is the model's `late_initcall(rtc_hctosys)`: the wall clock is seeded
after every driver has probed, not from a per-platform boot hook. The kernel
banner in `kruntime` therefore prints after `driver-init`. A platform that
seeds the clock earlier keeps its sample, because `initialize_realtime` is
idempotent and keeps the first established correlation.

## Design decisions

- **Read-only trait.** The trait samples the wall clock and nothing else.
  Alarm/wakeup/periodic-interrupt surfaces are added per driver when a
  consumer needs them; `clock_settime` still updates the kernel correlation
  in `ktime`, not the hardware.
- **`Arc` payload.** Unlike the char class (`Box`), an RTC driver keeps its
  register handle alive in `Data` and may later share the instance with an
  alarm IRQ handler.
- **No devnode projection yet.** There is no userland RTC ABI (`/dev/rtcN`,
  `RTC_RD_TIME` ioctls) in the tree, so the class publishes no node and
  carries no `ClassHooks`. The projection is added together with that ABI.
- **Fallback across devices.** `hctosys` tries every published device until
  one succeeds; a single faulty clock does not block a working one on the
  same board.

## Failures

`hctosys` returns the accepted sample or `None`. `None` — an empty class or
all devices failing — is not a boot failure: the caller logs it and the wall
clock keeps its epoch base. Driver read errors are logged per device; a
device whose read fails is not removed from the class, so a later consumer
can retry it.