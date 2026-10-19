// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Boot-stage console: the printk sink and the early stdout bring-up.
//!
//! This crate owns the console path that runs **before** the driver model
//! exists. The platform `early_driver_init` resolves the firmware stdout
//! node, maps its window, registers the early stdout port, and wires the
//! input interrupt; from then on [`khal::console::ConsoleIf`] routes kernel
//! output to that port, and the model's serial driver later adopts the same
//! hardware instance through `serial_core::take_early_port`.
//!
//! The split with its two neighbours:
//!
//! * `serial-core` owns the serial-family parts of earlycon — `compatible`
//!   → family, `reg-shift`/`reg-io-width` → layout, and port construction;
//! * `subsys-char-console` owns the model-side half — the adopted stdout
//!   publication, its transferred guards, and the console view over `CHAR`.
//!
//! This crate keeps the boot policy: *which* node is the console, mapping
//! it (`memspace`), translating its interrupt (`kirq`), and providing the
//! one `ConsoleIf` implementation (`khal`). In Linux terms it is the
//! `earlycon` selection plus the registered `struct console`, which is why
//! it is a host-side boot tree rather than a device driver.

#![no_std]

extern crate alloc;

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use kirq::{IrqActionToken, IrqDesc};
use kspin::{SpinNoIrq, static_lock};
#[cfg(all(feature = "ns16550-ioport", target_arch = "x86_64"))]
use serial_core::SerialPort;
#[cfg(any(
    feature = "pl011",
    feature = "ns16550-mmio",
    all(feature = "ns16550-ioport", target_arch = "x86_64")
))]
use serial_core::SerialRole;
#[cfg(any(feature = "pl011", feature = "ns16550-mmio"))]
use serial_core::earlycon::EarlyMmioPort;

#[cfg(not(any(
    feature = "pl011",
    feature = "ns16550-mmio",
    feature = "ns16550-ioport"
)))]
compile_error!(
    "boot-console requires at least one backend feature: `pl011`, `ns16550-mmio`, or \
     `ns16550-ioport`"
);

static INPUT_IRQ_REGISTERED: AtomicBool = AtomicBool::new(false);
static_lock! {
    /// Keeps the console input interrupt registration identity for diagnostics.
    static INPUT_IRQ: SpinNoIrq<Option<IrqActionToken>> = SpinNoIrq::new(None);
}

/// Fixed I/O-port base for the NS16550 ISA COM port used as stdout on x86.
#[cfg(feature = "ns16550-ioport")]
pub fn boot_console_io_port() -> u16 {
    0x3f8
}

// The single `ConsoleIf` implementation. `kiface` permits exactly one
// implementation per interface, so it lives here at the crate root and routes
// printk to the early stdout [`SerialPort`]. The stdout port is brought up in
// platform `early_driver_init`, before the driver model exists, so this is the
// one console path that must not depend on the driver framework.
#[kiface::provide]
impl khal::console::ConsoleIf {
    fn write_data(buf: &[u8]) {
        if let Some(port) = serial_core::stdout_port() {
            port.write_data(buf);
        }
    }

    fn write_data_atomic(buf: &[u8]) {
        if let Some(port) = serial_core::stdout_port() {
            port.write_data_atomic(buf);
        }
    }

    fn read_data(buf: &mut [u8]) -> usize {
        serial_core::stdout_port()
            .map(|port| port.read_data(buf))
            .unwrap_or(0)
    }

    fn interrupt_id() -> Option<usize> {
        serial_core::stdout_irq().and_then(IrqDesc::logical_irq)
    }
}

/// Read one byte from the stdout console, if it has been brought up.
pub fn getchar() -> Option<u8> {
    serial_core::stdout_port().and_then(|port| port.getchar())
}

#[cfg(any(feature = "pl011", feature = "ns16550-mmio"))]
fn device_tree_irq_desc(info: of::InterruptInfo) -> IrqDesc {
    let desc = match info.controller {
        of::InterruptControllerKind::Gic => match info.trigger {
            of::InterruptTrigger::EdgeRising => kirq::gic_edge_irq_desc(info.irq),
            of::InterruptTrigger::EdgeFalling => {
                kirq::gic_irq_desc(info.irq, kirq::IrqTrigger::EdgeFalling)
            }
            of::InterruptTrigger::LevelHigh => kirq::gic_level_irq_desc(info.irq),
            of::InterruptTrigger::LevelLow => {
                kirq::gic_irq_desc(info.irq, kirq::IrqTrigger::LevelLow)
            }
            of::InterruptTrigger::Unknown(flags) => {
                kirq::gic_irq_desc(info.irq, kirq::IrqTrigger::Unknown(flags))
            }
        },
        of::InterruptControllerKind::Plic => kirq::plic_irq_desc(info.irq),
        of::InterruptControllerKind::Unknown => kirq::IrqDesc::from_hwirq(info.irq),
    };

    desc.with_source(kirq::IrqSource::DeviceTree)
}

/// Map the stdout MMIO window described by firmware.
#[cfg(any(feature = "pl011", feature = "ns16550-mmio"))]
fn map_stdout_mmio(port: &EarlyMmioPort) -> khal::mem::VirtAddr {
    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the firmware-described stdout \
                  UART window cannot be mapped; console initialization has no error return or \
                  recovery"
    )]
    memspace::iomap_device(port.paddr, port.size, "console")
        .unwrap_or_else(|err| panic!("failed to iomap console: {err:?}"))
}

#[cfg(any(
    feature = "pl011",
    feature = "ns16550-mmio",
    all(feature = "ns16550-ioport", target_arch = "x86_64")
))]
fn map_console_irq(desc: IrqDesc) -> Option<IrqDesc> {
    match kirq::try_map(desc) {
        Ok(virq) => Some(desc.with_virq(virq)),
        Err(err) => {
            log::warn!("failed to map console IRQ {desc:?}: {err:?}");
            None
        }
    }
}

/// Bring up the stdout console from the device tree.
///
/// Resolves the `chosen` stdout node, decodes it through the serial family
/// library, maps the window, and registers the port as the early stdout so
/// printk and the runtime serial driver share one instance. Returns `None`
/// when the device tree does not describe a console any enabled backend
/// recognizes.
#[cfg(any(feature = "pl011", feature = "ns16550-mmio"))]
pub fn init_stdout_from_device_tree() -> Option<()> {
    let node = EarlyMmioPort::stdout_firmware_node()?;
    let desc = EarlyMmioPort::from_firmware_node(&node)?;
    let irq = of::first_interrupt_desc(node).map(device_tree_irq_desc);
    let vaddr = map_stdout_mmio(&desc);
    // SAFETY: `vaddr` is the freshly established, exclusively-owned mapping
    // of exactly the firmware-described window; the port owns it for the
    // kernel's lifetime as the early stdout.
    let port = unsafe { desc.build(vaddr, SerialRole::Stdout) };
    let irq = irq.and_then(map_console_irq);
    serial_core::register_early_stdout(Arc::new(port), irq);
    Some(())
}

/// Bring up a fixed I/O-port stdout console (x86 ISA COM).
#[cfg(all(feature = "ns16550-ioport", target_arch = "x86_64"))]
pub fn init_stdout_ioport(port: u16, irq: Option<IrqDesc>) {
    // SAFETY: `port` is the platform-configured NS16550 I/O-port base.
    let uart = unsafe { SerialPort::new_ioport_ns16550(port, SerialRole::Stdout) };
    let irq = irq.and_then(map_console_irq);
    serial_core::register_early_stdout(Arc::new(uart), irq);
}

/// Register the stdout console's input interrupt handler.
///
/// Must be called after the platform IRQ provider is up; platforms differ on
/// whether that is `early_driver_init` or `final_init`.
pub fn register_input_irq_handler() {
    let Some(desc) = serial_core::stdout_irq() else {
        return;
    };
    let Some(virq) = desc.logical_irq() else {
        return;
    };
    if INPUT_IRQ_REGISTERED.load(Ordering::Acquire) {
        return;
    }
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: The resolved console IRQ belongs to the \
                  stdout port installed before IRQ input initialization"
    )]
    let port = serial_core::stdout_port().expect("console IRQ requires a stdout port");
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the console input buffer \
                  cannot be prepared; console IRQ initialization has no error return or recovery"
    )]
    port.prepare_irq_input()
        .expect("failed to allocate console input buffer");
    if INPUT_IRQ_REGISTERED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    match kirq::try_register_shared(virq, Arc::new(move |_| port.handle_input_irq())) {
        Ok(Some(token)) => *INPUT_IRQ.lock() = Some(token),
        #[expect(
            clippy::panic,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: kirq cannot install a \
                      shared action on the stdout input IRQ (unresolved line, teardown or a \
                      non-shared action); console IRQ initialization has no error return or \
                      recovery"
        )]
        Ok(None) => {
            INPUT_IRQ_REGISTERED.store(false, Ordering::Release);
            panic!("failed to register console input IRQ handler: {desc:?}");
        }
        #[expect(
            clippy::panic,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: kirq rejects the stdout \
                      input IRQ registration; console IRQ initialization has no error return or \
                      recovery"
        )]
        Err(err) => {
            INPUT_IRQ_REGISTERED.store(false, Ordering::Release);
            panic!("failed to register console input IRQ handler: {desc:?} ({err:?})");
        }
    }
}

#[cfg(any(feature = "pl011", feature = "ns16550-mmio"))]
unittest::path_mod_test!(irq_desc_unittest, "../ut-tests/irq_desc_unittest.rs");
