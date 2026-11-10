# boot-console — Security and reliability

## Trust model and external boundaries

The device tree is trusted to describe *where* the console is, not to be
correct: a declared stdout window is mapped and its properties are applied
verbatim. The crate performs the first MMIO mapping of the boot process
through `memspace`, registers the one shared input IRQ, and provides the
`ConsoleIf` sink. It contains no `unsafe` beyond calling the `unsafe`
`SerialPort` constructors with a freshly established mapping.

## Unsafe inventory

| Site | Unsafe operation | Invariant |
| --- | --- | --- |
| `map_stdout_mmio` | `memspace::iomap_device` + `EarlyMmioPort::build` | The returned window is exclusively owned by the early stdout port for the kernel's lifetime; size and layout come from the same node. |
| `init_stdout_ioport` | `SerialPort::new_ioport_ns16550` | The port base is the platform-constant ISA COM base (0x3f8); no firmware input. |

Register access, emergency output and the input ring are `serial-core`'s
unsafe surface, documented there.

## Threats and failure handling

| Failure | Effect | Control |
| --- | --- | --- |
| Hostile/broken DT stdout node | Wrong window mapped, boot stops | The node must resolve and carry `reg`; mapping failure panics loudly instead of continuing without output |
| `reg-io-width` naming an unsupported width | Wrong register accesses | `EarlyMmioPort` decoding rejects it (`None`), treated as "no recognized console" |
| Console IRQ cannot be mapped | No early input | Logged; output is unaffected |
| Double `register_input_irq_handler` | Duplicate action | `INPUT_IRQ_REGISTERED` makes it idempotent |
| NMI during normal console I/O | Deadlock if the same lock were taken | `write_data_atomic` bypasses the normal lock by construction; bounded polling in `serial-core` |

The console input buffer is prepared before the IRQ action is published, so
a handler never observes unprepared storage. No device or scheduler lock is
held across a panic path.

## Known limitations

- One console: there is no selection API, no hot-unplug, and no console
  takeover protocol. See `drivers/drv/serial/docs/design.md` for the
  ownership handoff to the model driver.
- Boot-time panics on an unmappable declared console are intentional; use a
  defconfig whose stdout node is real for the target machine.