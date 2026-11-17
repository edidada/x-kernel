# x2apic-rs

A Rust interface to the x2apic interrupt architecture.

This crate is in its early stages and has only been tested in QEMU; code
contributions and bug reports are welcome.

It will use x2APIC mode if supported by the CPU, otherwise it
falls back to xAPIC mode.

## Usage

The local APIC is initialized like so:

```rust
use x2apic::lapic::{LocalApic, LocalApicBuilder, xapic_base};

let apic_physical_address: u64 = unsafe { xapic_base() };
let apic_virtual_address: u64 = <convert from physical address>

let lapic = LocalApicBuilder::new()
    .timer_vector(timer_index)
    .error_vector(error_index)
    .spurious_vector(spurious_index)
    .set_xapic_base(apic_virtual_address)
    .build()
    .unwrap_or_else(|err| panic!("{}", err));

unsafe {
    lapic.enable();
}
```

This initializes and enables the local APIC timer with a default configuration.
The timer can be configured with the builder or directly on the APIC.

The IOAPIC is initialized like so:

```rust
use x2apic::ioapic::{IoApic, IrqFlags, IrqMode, RedirectionTableEntry};

// !!! Map the IOAPIC's MMIO address `addr` here !!!

unsafe {
    let ioapic = IoApic::new(addr);

    ioapic.init(irq_offset);

    let mut entry = RedirectionTableEntry::default();
    entry.set_mode(IrqMode::Fixed);
    entry.set_flags(IrqFlags::LEVEL_TRIGGERED | IrqFlags::LOW_ACTIVE | IrqFlags::MASKED);
    entry.set_dest(dest); // CPU(s)
    ioapic.set_table_entry(irq_number, entry);

    ioapic.enable_irq(irq_number);
}
```

Please refer to the documentation for more details.


## Prepared runtime handles

`LocalApic::into_runtime` consumes the exclusive startup object. The resulting
`RuntimeLocalApic` keeps immutable mode/register metadata and is neither Send
nor Sync; its owner pins operations to the prepared CPU. Source-specific timer,
error and performance-LVT operations use shared references with explicit unsafe
hardware-ownership contracts. They do not update builder configuration caches.

Every runtime IPI kind uses `try_send_ipi`. A non-waiting atomic claim spans both
xAPIC destination/command writes or the single x2APIC MSR write. A nested NMI or
hardware-busy rejection writes no ICR state; callers choose their retry policy.
Success confirms submission only. Entry/target readiness and PMU ownership are
not supplied by this hardware interface.

The runtime x2APIC command path publishes earlier memory stores with
`MFENCE; LFENCE` before its non-serializing ICR WRMSR, following Linux's
`weak_wrmsr_fence`. The command tests construct x2APIC metadata independently
of host CPUID and perform no privileged MSR access: they cover 32-bit targets,
NMI encoding and a nested sender's pre-register Busy rejection.
