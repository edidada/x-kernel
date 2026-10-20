# drv-rtc — Design

## Purpose and scope

One crate per device family, one module per driver, one feature per backend —
the same shape as `drv-serial` and `drv-virtio`. Each module is a
`DriverOps<PlatformBus>` implementation that publishes a
`subsys_rtc::RtcDeviceImpl` into the `RTC` class.

| Module | Device | Discovery | Feature |
| --- | --- | --- | --- |
| `pl031` | ARM PrimeCell PL031 | DT `arm,pl031`; generic `arm,primecell` + periphid `0x41030` | `pl031` |
| `goldfish` | Goldfish virtual RTC | DT `google,goldfish-rtc` | `goldfish` |
| `cmos` | PC CMOS clock | board-static alias `rtc-cmos` | `cmos` (x86_64) |
| `ls7a` | Loongson LS7A TOY | DT `loongson,ls7a-rtc`; board-static alias `rtc-ls7a` | `ls7a` |

`common.rs` holds the error mapping shared by all four modules.

## Resource acquisition and teardown

Every MMIO backend maps its window through the one resource door
(`dev.res().iomap(0)`) and stores the returned `IoMap` in `DriverOps::Data`.
Field order is the release order — the class `Publication` comes first, the
mapping second, so a consumer that already holds the device sees a live
mapping until the publication drains.

The PL031 module matches the generic `arm,primecell` compatible because some
device trees describe the RTC only by peripheral ID. A node that does not
identify as a PL031 makes the probe return `ProbeError::Fatal`, which the
domain records per driver and hands to the next candidate: the PL011 UART
and other PrimeCell devices keep flowing to their own drivers.

## CMOS fixed-port decision

The CMOS command/data pair is architecturally fixed at `0x70`/`0x71` on
x86; the board-static descriptor states that window as an `IoPort` resource,
and the driver refuses to probe if the descriptor disagrees. The register
access itself is delegated to `x86_rtc`, which performs the port I/O and the
BCD/12-hour decoding but takes no port base parameter. Passing the base
through would mean re-implementing that decoding locally for no hardware
that uses another address; the validation keeps the descriptor truthful
without a second code path.

## LS7A protocol split

`ls7a_toy.rs` holds the transport-agnostic TOY protocol
(`RtcToyAccess` + year-boundary consistency check). It is compiled whenever
the driver is selected *or* the unittest cfg is active, so the tearing
regression tests run on every architecture, not only on LoongArch. `ls7a.rs`
owns the MMIO accessor, discovery and publication.

`ut-tests/common.rs` checks timestamp range validation;
`ut-tests/ls7a_toy.rs` checks scripted LS7A samples. Each is mounted as
`tests` from its source module with `unittest::path_mod_test!` under
`cfg(unittest)`. The LS7A cases also require the existing `ls7a` feature.
These are guest tests run through the configured kernel unittest runner.

## Failures

Probe failures (`missing resource`, `iomap error`, `class publish error`) are
mapped through `common::map_driver_error`/`ProbeError`. A read that cannot
produce a trustworthy sample returns `KError::Io`; `hctosys` logs it and
tries the next device.