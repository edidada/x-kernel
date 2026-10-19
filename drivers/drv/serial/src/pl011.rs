// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! PL011 serial driver on the driver-core model — the golden
//! template for **consumer drivers**.
//!
//! Translated from the Linux `drivers/tty/serial/amba-pl011.c` shape and the
//! golden template, it demonstrates every framework facility in
//! one file:
//!
//! * id table (`of_device_id` → [`IDS`])
//! * named resources (`reg`, `interrupts`, `clocks`/`clock-names`)
//! * early-console adoption via `take_early_port` (printk keeps flowing)
//! * clock consumption with automatic `-EPROBE_DEFER` (`dev.clk`)
//! * interrupt registration with the `dev_id` capture pattern
//! * guards-in-`Data` ownership (drop order is the release order)
//!
//! Runtime console/char publication goes through the class layer
//! (`dev.publish(&CHAR, …)`): the stdout port
//! registers as the console subsystem's device, auxiliary ports publish
//! as char devices. Console output never stops — the early-stdout port
//! this driver adopts is the same hardware instance printk keeps using.

use alloc::{boxed::Box, sync::Arc};

use driver_base::{
    ClkHandle, DevIrq, Device, DriverOps, IoMap, IrqAction, OfMatch, PlatformBus, PlatformId,
    ProbeError,
};
use memaddr::{PhysAddr, VirtAddr};
use serial_core::{SerialIdent, SerialPort, SerialRole, take_early_port};
use subsys_char::class::CHAR;
use subsys_char_console::{adopt_stdout_guards, register_stdout_console};

use crate::serial_char_dev::SerialCharDev;

/// `static const struct of_device_id pl011_of_match[]`.
static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("arm,pl011"))];

/// Driver-private data (`struct uart_amba_port`).
///
/// Field order is the release order: the interrupt is freed first, then the
/// port is quiesced, then the (cold-start) mapping is torn down, and the
/// clock reference goes last.
pub struct Pl011Data {
    /// The Data-owned char publication (auxiliary ports): dropping
    /// unpublishes. FIRST field — before every guard below. The stdout
    /// port deliberately holds none (console continuity across rebind).
    _publication: Option<driver_base::Publication<subsys_char::CharDeviceImpl>>,
    _irq: Option<DevIrq>,
    _port: Arc<SerialPort>,
    _regs: Option<IoMap>,
    _clk: Option<ClkHandle>,
    /// The enabled-state token for auxiliary ports: dropping it balances
    /// the prepare/enable pair (disable + unprepare) — the structural
    /// counterpart of the stdout port's `keep_running` escape hatch. The
    /// stdout path stores `None` (its token was consumed at probe).
    _enabled_clk: Option<driver_base::EnabledClock>,
}

/// The driver ops (`struct amba_pl011_driver`).
pub struct Pl011Driver;

/// Acquire and enable the port's baud clock. Returns the handle plus the
/// ENABLED-state token, which the caller keeps alive through every fallible
/// probe step (IRQ registration, publication) — a failure drops it and the
/// pair rolls back. See `Pl011Data::_enabled_clk` for the success policy.
fn enable_baud_clock(
    dev: &mut Device<'_, PlatformBus>,
) -> Result<Option<(ClkHandle, driver_base::EnabledClock)>, ProbeError> {
    match dev
        .res()
        .map()
        .clk_by_name("uartclk")
        .or_else(|| dev.res().map().clk_at(0))
    {
        Some(spec) => {
            let clk = dev.res().clk_spec(spec)?;
            let enabled = clk
                .prepare_enable()
                .map_err(ProbeError::from)
                .inspect_err(|_| log::warn!("pl011: failed to enable clock"))?;
            Ok(Some((clk, enabled)))
        }
        None => {
            log::debug!("pl011: no clock input described by firmware");
            Ok(None)
        }
    }
}

impl DriverOps<PlatformBus> for Pl011Driver {
    type Data = Pl011Data;

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<Pl011Data, ProbeError> {
        let mmio = dev.res().mmio(0)?;
        let paddr = PhysAddr::from_usize(mmio.base);
        let ident = SerialIdent::Mmio {
            paddr,
            size: mmio.size,
        };

        // Adopt the early-boot stdout instance when this node is it: printk
        // keeps writing through the shared `Arc<SerialPort>`, so console
        // output never stops. Cold-start ports get a fresh mapping instead.
        let (port, regs) = match take_early_port(&ident) {
            Some(port) => (port, None),
            None => {
                let regs = dev.res().iomap(0)?;
                // SAFETY: `regs` covers exactly the `mmio` window reported by
                // firmware, is 32-bit aligned per the PL011 spec, and stays
                // alive for the port's lifetime inside `Pl011Data`.
                let port = unsafe {
                    SerialPort::new_mmio_pl011(
                        VirtAddr::from_usize(regs.as_ptr() as usize),
                        paddr,
                        mmio.size,
                        SerialRole::Auxiliary,
                    )
                };
                (Arc::new(port), Some(regs))
            }
        };

        // Clock inputs (`clocks = <&phandle>`, `clock-names = "uartclk" ...`).
        // Defers cleanly while the provider has not probed yet. The token
        // stays alive through every fallible step below; only a fully
        // successful probe hands the enabled state onward.
        let clk = enable_baud_clock(dev)?;

        // Interrupt: !920 semantics — the port's handler acknowledges and
        // collects input internally and wakes the attached tty waker
        // (ack_interrupt no longer exists as a public API).
        let irq = match dev.res().irq(0) {
            Ok(spec) => {
                let handler_port = port.clone();
                Some(dev.res().request_irq(spec, "pl011", move || {
                    if handler_port.handle_input_irq().handled() {
                        IrqAction::Handled
                    } else {
                        IrqAction::NotHandled
                    }
                })?)
            }
            Err(_) => None,
        };

        // Publish the runtime capability: the stdout port joins the console
        // subsystem (/dev/console backing), auxiliary ports become char
        // devices. Both use a detached class parent — the class layer is
        // model-agnostic since the class-layer decoupling.
        let char_dev: subsys_char::CharDeviceImpl = Box::new(SerialCharDev(port.clone()));
        let mut publication: Option<driver_base::Publication<subsys_char::CharDeviceImpl>> = None;
        let published = dev
            .publish(&CHAR, char_dev, "serial")
            .map_err(crate::probe_error::map_driver_error)?;
        match port.role() {
            // The stdout publication outlives this binding: the console
            // subsystem owns it (and withdraws it on the next rebind), so the
            // driver keeps no guard for it.
            SerialRole::Stdout => register_stdout_console(published),
            SerialRole::Auxiliary => publication = Some(published),
        }

        log::info!(
            "pl011: probed {} (paddr {:#x}, clk {}, irq {})",
            if regs.is_none() {
                "[early-console adopted]"
            } else {
                ""
            },
            mmio.base,
            clk.as_ref().map(|(c, _)| c.get_rate()).unwrap_or(0),
            // Log the hardware line (the descriptor's hwirq): for
            // route-style specs the DevIrq number is 0 until the domain
            // mapping resolves it.
            dev.res()
                .irq(0)
                .ok()
                .and_then(|spec| spec.hwirq())
                .unwrap_or(usize::MAX),
        );
        // The enabled-clock policy resolves only now, after EVERY fallible
        // step has succeeded: stdout consumes the token (the adopted
        // console keeps its clock past removal — explicit continuity
        // policy); auxiliary ports carry the token in Data, and its Drop
        // balances the prepare/enable pair at remove.
        let (clk, enabled_clk) = match clk {
            Some((handle, token)) => {
                if matches!(port.role(), SerialRole::Stdout) {
                    token.keep_running();
                    (Some(handle), None)
                } else {
                    (Some(handle), Some(token))
                }
            }
            None => (None, None),
        };
        // Console continuity, ownership side: the stdout publication
        // survives this binding (no withdrawal guard), so its runtime
        // instance must survive with it — the input IRQ and (for a
        // cold-start port) the register mapping transfer to the console
        // subsystem here instead of dying at Data drop. A publication
        // that outlives its IRQ would read as an available console that
        // can never receive input.
        let is_stdout = matches!(port.role(), SerialRole::Stdout);
        let (mut irq, mut regs) = (irq, regs);
        if is_stdout {
            adopt_stdout_guards(irq.take(), regs.take());
        }
        Ok(Pl011Data {
            _publication: publication,
            _irq: irq,
            _port: port,
            _regs: regs,
            _clk: clk,
            _enabled_clk: enabled_clk,
        })
    }

    fn remove(&self, _dev: &mut Device<'_, PlatformBus>, _data: &mut Pl011Data) {
        // Guards in `Data` release after this in field order. The clock
        // balance is structural: `_enabled_clk`'s Drop disables + unprepares
        // auxiliary ports; the adopted stdout port deliberately keeps
        // running (its token was consumed via `keep_running` at probe —
        // console continuity across rebind).
    }
}

driver_base::declare_driver!(PlatformBus, Pl011Driver, "pl011-serial", IDS);
