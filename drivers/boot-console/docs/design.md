# boot-console — Design

## Purpose and scope

`src/lib.rs` is the whole crate: the boot-stage console path that must work
before the driver model exists.

* **printk sink** — the single `khal::console::ConsoleIf` implementation
  (`kiface` permits exactly one) routes kernel output to the early stdout
  `SerialPort`.
* **stdout selection and bring-up** — resolve the firmware `chosen`
  stdout node, decode it through the serial family library, map the window,
  build the port, register it as the early stdout.
* **early input** — register the stdout input IRQ and expose `getchar`
  before the tty exists.

It does **not** own: UART register access and port construction (that is
`serial-core`), the runtime console publication over the char class (that is
`subsys-char-console`), or console policy such as hot-unplug and switching
(not implemented).

## Architecture

```text
platform early_driver_init
    │  init_stdout_from_device_tree()            init_stdout_ioport()
    ▼
of chosen stdout node ──► serial_core::earlycon::EarlyMmioPort
    │                         (family, paddr/size, reg layout)
    ├─ memspace::iomap_device("console")
    ├─ kirq::try_map + IrqDesc conversion
    ▼
serial_core::register_early_stdout(Arc<SerialPort>, irq)
    │
    ├─► khal::console ConsoleIf  ──► port.write_data / write_data_atomic   (printk)
    ├─► boot_console::getchar    ──► port.getchar                          (early input)
    └─► kirq::try_register_shared(port.handle_input_irq)                   (input IRQ)
```

The model serial driver later adopts the same hardware instance through
`serial_core::take_early_port`; the stdout publication then transfers its
guards to `subsys-char-console`. Kernel output never switches sinks: printk
keeps using the early port through `ConsoleIf` for the kernel's lifetime.

## Execution context and ordering

- `init_stdout_from_device_tree` / `init_stdout_ioport` run in
  `early_driver_init`, after the allocator and `memspace` are ready, and
  before the scheduler publishes work.
- `register_input_irq_handler` runs after the platform IRQ provider is up
  (`early_driver_init` on RISC-V, `final_init` on x86).
- `ConsoleIf::write_data` may be called from any context; the underlying
  `SerialPort` owns the locking. `write_data_atomic` is the NMI path and
  must not allocate or take ordinary locks (implemented in `serial-core`).

## Failure policy

- A missing or unrecognized stdout node returns `None` from
  `init_stdout_from_device_tree`; the platform decides whether that is fatal.
- An unmappable stdout window panics: a machine that declared a console it
  cannot map has no usable early output, and `early_driver_init` has no
  error return.
- An unmappable console IRQ is logged and input stays unavailable; output
  still works.
- Registering the input IRQ twice is idempotent (`INPUT_IRQ_REGISTERED`).

## Split with the neighbours

| Concern | Home | Why |
| --- | --- | --- |
| compatible → family, layout decode, port construction | `serial_core::earlycon` (feature `earlycon`) | The serial family's own earlycon; model-only builds stay `of`-free. |
| mapping, IRQ description, `ConsoleIf` | this crate | `khal`/`memspace`/`kirq` boot integration, pinned before the model. |
| runtime publication identity and guards | `subsys-char-console` | It only exists once the device model published the port. |