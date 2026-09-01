// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-instance UART port.
//!
//! Replaces the per-backend global singletons: one [`SerialPort`] per physical
//! UART. The stdout port is constructed during platform `early_driver_init` and
//! reused at runtime, so printk and the driver-model serial driver share the
//! same hardware instance. Auxiliary ports (additional UARTs) are created later
//! by the serial driver and exposed as standalone character devices.
//!
//! This is introduced in stages. Phase 1 only builds the stdout port and the
//! legacy free-function backends delegate to it, leaving behavior unchanged.

use alloc::sync::Arc;

#[cfg(feature = "pl011")]
use arm_pl011::Pl011Uart;
use khal::mem::PhysAddr;
#[cfg(any(feature = "pl011", feature = "ns16550-mmio"))]
use khal::mem::VirtAddr;
use kirq::IrqDesc;
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
#[cfg(all(feature = "ns16550-ioport", target_arch = "x86_64"))]
use uart_16550::SerialPort as SerialPort16550;

#[cfg(feature = "ns16550-mmio")]
use crate::ns16550_mmio::{Port as Ns16550MmioPort, SerialRegWidth};

/// Physical identity of a UART.
///
/// Used to match an early-boot stdout instance against a device-model node so
/// the runtime serial driver can adopt the same hardware without remapping it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerialIdent {
    /// Memory-mapped register window.
    Mmio { paddr: PhysAddr, size: usize },
    /// I/O-port register window (e.g. an x86 ISA COM port).
    IoPort { port: u16 },
}

/// Functional role of a port within the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerialRole {
    /// The kernel stdout console (early printk target).
    Stdout,
    /// An auxiliary serial port exposed as a standalone char device.
    Auxiliary,
}

/// Underlying UART hardware, fixed at construction time. More than one variant
/// may be compiled in so a single image can carry several UART families.
enum Backend {
    #[cfg(feature = "pl011")]
    Pl011(Pl011Uart),
    #[cfg(feature = "ns16550-mmio")]
    Ns16550Mmio(Ns16550MmioPort),
    #[cfg(all(feature = "ns16550-ioport", target_arch = "x86_64"))]
    Ns16550IoPort(SerialPort16550),
}

impl Backend {
    /// Send one byte, applying the per-backend line discipline. PL011 and the
    /// NS16550 MMIO backend translate `\n` to `\r\n`; the NS16550 ioport backend
    /// preserves its historical raw behavior.
    fn send_byte(&mut self, c: u8) {
        match self {
            #[cfg(feature = "pl011")]
            Backend::Pl011(uart) => pl011_putchar(uart, c),
            #[cfg(feature = "ns16550-mmio")]
            Backend::Ns16550Mmio(uart) => ns16550_mmio_putchar(uart, c),
            #[cfg(all(feature = "ns16550-ioport", target_arch = "x86_64"))]
            Backend::Ns16550IoPort(uart) => uart.send(c),
        }
    }

    fn try_receive(&mut self) -> Option<u8> {
        match self {
            #[cfg(feature = "pl011")]
            Backend::Pl011(uart) => uart.getchar(),
            #[cfg(feature = "ns16550-mmio")]
            Backend::Ns16550Mmio(uart) => uart.try_receive().ok(),
            #[cfg(all(feature = "ns16550-ioport", target_arch = "x86_64"))]
            Backend::Ns16550IoPort(uart) => uart.try_receive().ok(),
        }
    }

    fn ack_interrupt(&mut self) {
        match self {
            #[cfg(feature = "pl011")]
            Backend::Pl011(uart) => {
                if uart.is_receive_interrupt() {
                    uart.ack_interrupts();
                }
            }
            // The NS16550 backends have no explicit ack in the existing code.
            #[cfg(feature = "ns16550-mmio")]
            Backend::Ns16550Mmio(_) => {}
            #[cfg(all(feature = "ns16550-ioport", target_arch = "x86_64"))]
            Backend::Ns16550IoPort(_) => {}
        }
    }
}

#[cfg(feature = "pl011")]
fn pl011_putchar(uart: &mut Pl011Uart, c: u8) {
    match c {
        b'\n' => {
            uart.putchar(b'\r');
            uart.putchar(b'\n');
        }
        c => uart.putchar(c),
    }
}

#[cfg(feature = "ns16550-mmio")]
fn ns16550_mmio_putchar(uart: &mut Ns16550MmioPort, c: u8) {
    match c {
        b'\n' => {
            uart.send(b'\r');
            uart.send(b'\n');
        }
        c => uart.send(c),
    }
}

/// One UART instance.
pub struct SerialPort {
    inner: SpinNoIrq<Backend>,
    ident: SerialIdent,
    role: SerialRole,
    /// Mapped PL011 register base (a raw vaddr), kept so `set_baud_pl011`
    /// can program IBRD/FBRD — the `arm_pl011` crate does not expose them.
    /// `None` for non-PL011 backends.
    #[cfg(feature = "pl011")]
    pl011_base: Option<usize>,
}

impl SerialPort {
    /// Build a PL011 port over an already-mapped MMIO window.
    ///
    /// `role` records whether this port is the kernel stdout console or an
    /// auxiliary char device; it is later read back through [`Self::role`].
    #[cfg(feature = "pl011")]
    pub fn new_mmio_pl011(
        uart_base: VirtAddr,
        paddr: PhysAddr,
        size: usize,
        role: SerialRole,
    ) -> Self {
        let mut uart = Pl011Uart::new(uart_base.as_mut_ptr());
        uart.init();
        Self::_new(
            Backend::Pl011(uart),
            SerialIdent::Mmio { paddr, size },
            role,
            Some(uart_base.as_usize()),
        )
    }

    /// Build an NS16550 MMIO port over an already-mapped window.
    ///
    /// `reg_shift` is the device-tree `reg-shift` value; each register is
    /// `1 << reg_shift` bytes apart (RK3588's DesignWare UART uses 2). The baud
    /// divisor is left untouched — firmware already programmed it.
    ///
    /// `reg_width` is the device-tree `reg-io-width` value decoded as a typed
    /// MMIO access width. Rockchip DesignWare UARTs use 32-bit accesses.
    ///
    /// # Safety
    ///
    /// `uart_base` must name a valid, exclusively-mapped NS16550 MMIO register
    /// window for the lifetime of this port.
    #[cfg(feature = "ns16550-mmio")]
    pub unsafe fn new_mmio_ns16550(
        uart_base: VirtAddr,
        paddr: PhysAddr,
        size: usize,
        role: SerialRole,
        reg_shift: u32,
        reg_width: SerialRegWidth,
    ) -> Self {
        let stride = 1usize << reg_shift;
        // SAFETY: caller guarantees `uart_base` is an exclusive NS16550 MMIO
        // map. The decoded DT layout supplies the register stride and access
        // width used for all volatile register operations.
        let uart = unsafe { Ns16550MmioPort::new(uart_base.as_usize(), stride, reg_width) };
        // SAFETY: the port was constructed from the same exclusive MMIO window;
        // line/FIFO/interrupt setup is programmed without touching the baud
        // divisor configured by firmware.
        unsafe { uart.init_preserve_baud() };
        Self::new(
            Backend::Ns16550Mmio(uart),
            SerialIdent::Mmio { paddr, size },
            role,
        )
    }

    /// Build an NS16550 I/O-port port.
    ///
    /// # Safety
    ///
    /// `port` must name a valid NS16550 I/O-port base.
    #[cfg(all(feature = "ns16550-ioport", target_arch = "x86_64"))]
    pub unsafe fn new_ioport_ns16550(port: u16, role: SerialRole) -> Self {
        // SAFETY: caller guarantees `port` is a valid NS16550 I/O-port base.
        let mut uart = unsafe { SerialPort16550::new(port) };
        uart.init();
        Self::new(
            Backend::Ns16550IoPort(uart),
            SerialIdent::IoPort { port },
            role,
        )
    }

    #[allow(dead_code)]
    fn new(backend: Backend, ident: SerialIdent, role: SerialRole) -> Self {
        Self::_new(backend, ident, role, None)
    }

    fn _new(
        backend: Backend,
        ident: SerialIdent,
        role: SerialRole,
        #[cfg(feature = "pl011")] pl011_base: Option<usize>,
    ) -> Self {
        Self {
            inner: SpinNoIrq::new(backend),
            ident,
            role,
            #[cfg(feature = "pl011")]
            pl011_base,
        }
    }

    /// Program the PL011 baud divisor from a reference clock rate.
    ///
    /// Computes IBRD/FBRD for `baud` given `ref_clk_hz` and writes them with
    /// the UART briefly disabled (the documented PL011 divisor-change
    /// sequence). No-op for non-PL011 backends and for `None` (the caller
    /// keeps the firmware-programmed divisor — the `earlycon` fallback).
    #[cfg(feature = "pl011")]
    pub fn set_baud_pl011(&self, ref_clk_hz: Option<u64>, baud: u32) {
        let Some(hz) = ref_clk_hz else { return };
        let Some(base) = self.pl011_base else { return };
        let denom = 16u64 * u64::from(baud);
        let ibrd = (hz / denom).min(0xffff) as u32;
        let rem = hz % denom;
        let fbrd = ((rem * 64 + denom / 2) / denom).min(0x3f) as u32;

        let base = base as *mut u8;
        // Hold the port lock so printk cannot interleave a character mid-
        // divisor-change. Do NOT log from inside this critical section: the
        // console prints through this same port and would self-deadlock on
        // the non-reentrant spinlock.
        let _backend = self.inner.lock();
        // SAFETY: `base` is the exclusively-mapped PL011 register window this
        // port was constructed from; the offsets are architecturally fixed
        // (IBRD=0x24, FBRD=0x28). Programming the divisors with the UART
        // live is safe per the TRM (a running UART latches the new divisor
        // on the next frame); disabling CR first can wedge QEMU's model.
        unsafe {
            core::ptr::write_volatile(base.add(0x24) as *mut u32, ibrd);
            core::ptr::write_volatile(base.add(0x28) as *mut u32, fbrd);
        }
    }

    pub fn write_data(&self, bytes: &[u8]) {
        let mut backend = self.inner.lock();
        for &c in bytes {
            backend.send_byte(c);
        }
    }

    pub fn getchar(&self) -> Option<u8> {
        self.inner.lock().try_receive()
    }

    pub fn read_data(&self, bytes: &mut [u8]) -> usize {
        let mut backend = self.inner.lock();
        let mut read_len = 0;
        while read_len < bytes.len() {
            if let Some(c) = backend.try_receive() {
                bytes[read_len] = c;
                read_len += 1;
            } else {
                break;
            }
        }
        read_len
    }

    pub fn ack_interrupt(&self) {
        self.inner.lock().ack_interrupt();
    }

    /// Physical identity, for matching against a device-model node.
    pub fn ident(&self) -> SerialIdent {
        self.ident
    }

    /// Functional role of this port.
    pub fn role(&self) -> SerialRole {
        self.role
    }
}

struct EarlyStdout {
    port: Arc<SerialPort>,
    irq: Option<IrqDesc>,
}

static STDOUT: LazyInit<EarlyStdout> = LazyInit::new();

/// Record the early-boot stdout UART (and its interrupt descriptor) so printk
/// and the runtime serial driver share the same instance. Called once from
/// platform `early_driver_init`.
pub fn register_early_stdout(port: Arc<SerialPort>, irq: Option<IrqDesc>) {
    STDOUT.init_once(EarlyStdout { port, irq });
}

/// The early-boot stdout port, once [`register_early_stdout`] has run.
pub fn stdout_port() -> Option<Arc<SerialPort>> {
    STDOUT.get().map(|stdout| stdout.port.clone())
}

/// The interrupt descriptor of the stdout port, if registered.
pub fn stdout_irq() -> Option<IrqDesc> {
    STDOUT.get().and_then(|stdout| stdout.irq)
}

/// Hand the early stdout port to the runtime serial driver if its identity
/// matches `ident`.
///
/// The port stays registered (printk still reaches it through [`stdout_port`]);
/// this returns a shared clone so the serial driver can publish the same
/// hardware as a character device without remapping it.
pub fn take_early_port(ident: &SerialIdent) -> Option<Arc<SerialPort>> {
    let stdout = STDOUT.get()?;
    (stdout.port.ident() == *ident).then(|| stdout.port.clone())
}

#[cfg(unittest)]
mod tests {
    use khal::mem::PhysAddr;
    use unittest::{assert, assert_eq, def_test};

    use super::*;

    #[def_test]
    fn stdout_port_registered_at_boot() {
        // The platform `early_driver_init` registers the stdout UART before
        // unit tests run, so the registry must be populated.
        assert!(stdout_port().is_some());
    }

    /// The adoption guard: `take_early_port` must match only the real stdout's
    /// identity and reject any other. This is what prevents the runtime serial
    /// driver from adopting the wrong UART (e.g. an auxiliary "safe serial").
    #[def_test]
    fn take_early_port_discriminates_by_ident() {
        let port = stdout_port().expect("boot registered a stdout");
        let ident = port.ident();
        // The stdout's own identity matches.
        assert!(take_early_port(&ident).is_some());
        // A different identity must not match.
        let wrong = match ident {
            SerialIdent::Mmio { .. } => SerialIdent::IoPort { port: 0xffff },
            SerialIdent::IoPort { .. } => SerialIdent::Mmio {
                paddr: PhysAddr::from_usize(0xdead_beef),
                size: 0x1000,
            },
        };
        assert!(take_early_port(&wrong).is_none());
    }

    #[def_test]
    fn serial_ident_equality() {
        let a = SerialIdent::Mmio {
            paddr: PhysAddr::from_usize(0x9000000),
            size: 0x1000,
        };
        let eq = SerialIdent::Mmio {
            paddr: PhysAddr::from_usize(0x9000000),
            size: 0x1000,
        };
        assert_eq!(a, eq);
        let other_mmio = SerialIdent::Mmio {
            paddr: PhysAddr::from_usize(0x9040000),
            size: 0x1000,
        };
        assert!(a != other_mmio);
        assert!(a != SerialIdent::IoPort { port: 0x3f8 });
    }
}
