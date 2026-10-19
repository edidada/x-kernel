# subsys-rtc — Security and reliability

## Trust model and external boundaries

The class consumes samples produced by RTC drivers. A wall-clock sample is
untrusted input: it may be absent, stale, or malformed (a torn read across a
counter update). Drivers own hardware validation; this crate owns the
policy that an invalid or failing device must not seed the clock. The crate
touches no MMIO, DMA, user pointers, or firmware data directly, and contains
no `unsafe` code.

## Memory and thread safety

The contract requires `RtcDevice: Send + Sync`; the registry is a
`Class<RtcDeviceImpl>` static whose payload is `Arc<dyn RtcDevice + Send + Sync>`.
The class layer serializes registry mutation and gates in-flight reads with
the `Publication` usage count, so withdrawal waits for a read that already
started and rejects new ones. `hctosys` runs in boot context (single-threaded
`driver-init`); `read_time` must not sleep, so it is safe there.

## Threats and failure handling

| Failure | Effect | Control |
| --- | --- | --- |
| No RTC driver in the build | Wall clock stays at the epoch base | `hctosys` returns `None`; the composition root logs a warning |
| Torn or invalid sample | Wrong wall-clock correlation for the whole boot | Drivers reject torn samples before publishing a value; `hctosys` only accepts `Ok` |
| Duplicate/multiple clocks | A wrong clock could win | The first successful sample establishes the correlation; `initialize_realtime` is idempotent and ignores later samples |
| Driver panics mid-read | Boot stops in `driver-init` | Drivers must map invalid input to `KError`; the crate never calls `unwrap`/`expect` on driver results |

`hctosys` never logs sample contents beyond unix seconds and the claiming
driver name, and the class holds no secrets. An adversarial RTC can only
skew the wall clock; it cannot move it before `CLOCK_MONOTONIC`
(`initialize_realtime` records the correlation rather than an absolute set).

## Audit checklist

- `RtcDevice::read_time` implementations reject torn/invalid samples and
  return errors instead of panicking.
- A driver's `Publication` lives in `DriverOps::Data` and is dropped before
  release of the register mapping it backs.
- `hctosys` stays after the probe storm and never turns a missing clock into
  a boot panic.