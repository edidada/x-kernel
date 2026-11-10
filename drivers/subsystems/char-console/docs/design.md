# subsys-char-console — Design

## Purpose and scope

The model-side console half. The boot console (`drivers/boot-console`)
brings the stdout UART up before the driver model exists and keeps printk on
it; this crate starts when the adopted stdout port is published into the
char class and owns:

* the **console view** over `CHAR` — which publications are console devices
  (`runtime::is_runtime_console` + the adopted stdout id);
* the **stdout identity** — `stdout_console()` (available handle for the
  tty) and `stdout_console_id()` (identity for devfs deduplication);
* **guard ownership** — `adopt_stdout_guards` takes the binding's `DevIrq`
  and `IoMap` so the console publication never outlives its input IRQ or
  register mapping.

It exposes no console-selection API: there is one console, established at
boot.

## Architecture

```text
drv/serial probe (stdout port)
    ├─ dev.publish(&CHAR, …) ──► Publication<CharDeviceImpl>
    ├─ register_stdout_console(publication)   ← replaces any previous stdout
    └─ adopt_stdout_guards(irq, regs)         ← transfer, kernel lifetime

CHAR.subscribe_available/removed
    └─ console view (ConsoleSubsystem.devices)

consumers: io/ktty (attach runtime handle), fs/devfs (skip /dev/serialN)
```

The adopted stdout publication is **owned by this service**: a same-port
rebind (driver reprobe) replaces it, which withdraws the previous identity
(projection and class teardown included). Transferred guards are replaced in
the same step, so the pairing "publication ↔ irq ↔ mapping" can never be
half-replaced.

## Concurrency and lifecycle

- All state is behind one `SpinNoPreempt` (`ConsoleSubsystem`); callbacks
  run outside the lock (the class layer invokes them after releasing its own
  registry lock).
- `register_stdout_console` runs inside the driver's probe, before the
  device is active; a publication that is already available is added to the
  view immediately (`CHAR.find`).
- Removal clears both the view entry and the stdout association; the
  publication's drop is what triggers withdrawal, so no `Publication` is
  dropped under the console lock.
- The service holds the guards but never touches hardware: they exist for
  their `Drop` (rebind replacement or teardown), not for reads.

## Why not `subsystems/char`

The char class is a user-visible byte-device class in the discover → bind →
activate pipeline; the console's boot half is printk infrastructure that
predates the model, and its runtime half is a *consumer* of the class
(the publication is created by the serial driver). The service therefore
lives beside the class (`subsys-block` / `subsys-block-host` is the same
shape), not inside it.