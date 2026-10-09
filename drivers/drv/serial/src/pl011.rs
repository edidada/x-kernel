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
//! (`kclass::publish_*` with a detached `ClassParent`): the stdout port
//! registers as the console subsystem's device, auxiliary ports publish
//! as char devices. Console output never stops — the early-stdout port
//! this driver adopts is the same hardware instance printk keeps using.

use alloc::{boxed::Box, sync::Arc};

use char_driver::CharDevice;
use console_driver::{
    runtime::register_console_parented,
    serial::{SerialIdent, SerialPort, SerialRole, take_early_port},
};
use driver_base::{Device, DeviceKind, DriverResult};
use driver_core::{
    BoundDevice, ClkHandle, DevIrq, DeviceCtx, DriverDesc, DriverOps, IoMap, IrqAction, ProbeError,
    platform::{OfMatch, PlatformBus, PlatformId},
};
use kclass::{ClassParent, publish_char};
use memaddr::{PhysAddr, VirtAddr};

/// `static const struct of_device_id pl011_of_match[]`.
static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("arm,pl011"))];

/// Driver-private data (`struct uart_amba_port`).
///
/// Field order is the release order: the interrupt is freed first, then the
/// port is quiesced, then the (cold-start) mapping is torn down, and the
/// clock reference goes last.
pub struct Pl011Data {
    _irq: Option<DevIrq>,
    _port: Arc<SerialPort>,
    _regs: Option<IoMap>,
    _clk: Option<ClkHandle>,
}

/// The driver ops (`struct amba_pl011_driver`).
pub struct Pl011Driver;

impl DriverOps<PlatformBus> for Pl011Driver {
    type Data = Pl011Data;

    fn probe(&self, dev: &mut DeviceCtx<'_, PlatformBus>) -> Result<Pl011Data, ProbeError> {
        let mmio = dev.mmio(0)?;
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
                let regs = dev.iomap(0)?;
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
        // Defers cleanly while the provider has not probed yet.
        let clk = match dev
            .resources()
            .clk_by_name("uartclk")
            .or_else(|| dev.resources().clk_at(0))
        {
            Some(spec) => {
                let clk = dev.clk_spec(spec)?;
                // The EnabledClock token owns the prepare+enable pair: a
                // failure in ANYTHING below (irq registration, publication)
                // drops it and rolls the clock back automatically — the
                // review's "success enable, later probe failure" leak is
                // closed. The token cannot outlive this borrow into Data,
                // so at the end of a successful probe it either transfers
                // to keep_running (stdout console continuity) or the
                // enabled state is recorded for remove-time balance.
                let enabled = clk
                    .prepare_enable()
                    .map_err(ProbeError::from)
                    .inspect_err(|_| log::warn!("pl011: failed to enable clock"))?;
                if matches!(port.role(), SerialRole::Stdout) {
                    // Console continuity across rebind: the adopted stdout
                    // port keeps its clock past removal. Explicit policy.
                    enabled.keep_running();
                } else {
                    core::mem::forget(enabled);
                }
                Some(clk)
            }
            None => {
                log::debug!("pl011: no clock input described by firmware");
                None
            }
        };

        // Interrupt: !920 semantics — the port's handler acknowledges and
        // collects input internally and wakes the attached tty waker
        // (ack_interrupt no longer exists as a public API).
        let irq = match dev.irq(0) {
            Ok(spec) => {
                let handler_port = port.clone();
                Some(dev.request_irq(spec, "pl011", move || {
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
        let location = dev.descriptor().location;
        let char_dev: kclass::CharDeviceImpl = Box::new(SerialCharDev(port.clone()));
        match port.role() {
            SerialRole::Stdout => {
                register_console_parented(
                    ClassParent::detached(dev.id().raw(), "pl011-serial", location),
                    char_dev,
                )
                .map_err(crate::common::map_driver_error)?;
            }
            SerialRole::Auxiliary => {
                publish_char(
                    ClassParent::detached(dev.id().raw(), "pl011-serial", location),
                    char_dev,
                )
                .map_err(crate::common::map_driver_error)?;
            }
        }

        log::info!(
            "pl011: probed {} (paddr {:#x}, clk {}, irq {})",
            if regs.is_none() {
                "[early-console adopted]"
            } else {
                ""
            },
            mmio.base,
            clk.as_ref().map(|c| c.get_rate()).unwrap_or(0),
            // Log the hardware line (the descriptor's hwirq): for
            // route-style specs the DevIrq number is 0 until the domain
            // mapping resolves it.
            dev.irq(0)
                .ok()
                .and_then(|spec| spec.hwirq)
                .unwrap_or(usize::MAX),
        );
        Ok(Pl011Data {
            _irq: irq,
            _port: port,
            _regs: regs,
            _clk: clk,
        })
    }

    fn remove(&self, dev: &mut BoundDevice<'_, PlatformBus, Pl011Data>) {
        // Guards in `Data` release after this in field order. The clock
        // needs an explicit balance first: its Drop releases the reference
        // but does NOT disable — a prepared+enabled clock left behind keeps
        // the provider on. The adopted stdout port deliberately stays
        // running (console continuity across rebind); auxiliary ports
        // balance their enable.
        if let Some(clk) = dev.data_mut()._clk.take()
            && clk.ops().is_enabled()
        {
            let ops = clk.ops();
            ops.disable();
            ops.unprepare();
        }
    }
}

/// `module_platform_driver()` equivalent.
pub fn register() {
    driver_core::global_domain().register_driver(DriverDesc::new("pl011-serial", IDS, Pl011Driver));
}

/// Char-device runtime wrapper over a `SerialPort` (the `/dev` view).
struct SerialCharDev(Arc<SerialPort>);

impl Device for SerialCharDev {
    fn name(&self) -> &str {
        "serial"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn irq(&self) -> Option<usize> {
        None
    }
}

impl CharDevice for SerialCharDev {
    fn read(&self, buf: &mut [u8]) -> DriverResult<usize> {
        Ok(self.0.read_data(buf))
    }

    fn write(&self, buf: &[u8]) -> DriverResult<usize> {
        self.0.write_data(buf);
        Ok(buf.len())
    }
}
