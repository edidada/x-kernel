// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! PL011 serial driver on the driver-core model — the Phase 0 golden
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
//! Runtime console/char publication (the old `kclass` path) is intentionally
//! not done here: the class layer migrates in Phase 1. Console output keeps
//! working through the early-stdout port this driver adopts.

#![no_std]

extern crate alloc;

use alloc::{boxed::Box, sync::Arc};

use char_driver::CharDevice;
use console_driver::{
    runtime::register_console_parented,
    serial::{SerialIdent, SerialPort, SerialRole, take_early_port},
};
use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use driver_core::{
    BoundDevice, ClkHandle, DevIrq, DeviceCtx, DriverDesc, DriverOps, IoMap, IrqAction, ProbeError,
    RuntimeError,
    platform::{OfMatch, PlatformBus, PlatformId},
};
use kclass::{ClassParent, publish_char};
use khal::mem::{PhysAddr, VirtAddr};

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
                clk.prepare_enable()
                    .map_err(ProbeError::from)
                    .inspect_err(|_| log::warn!("pl011: failed to enable clock"))?;
                Some(clk)
            }
            None => {
                log::debug!("pl011: no clock input described by firmware");
                None
            }
        };

        // Interrupt: the handler captures the port (the `dev_id` pattern).
        let irq = match dev.irq(0) {
            Ok(spec) => {
                let handler_port = port.clone();
                Some(dev.request_irq(spec, "pl011", move || {
                    handler_port.ack_interrupt();
                    IrqAction::Handled
                })?)
            }
            Err(_) => None,
        };

        // Publish the runtime capability: the stdout port joins the console
        // subsystem (/dev/console backing), auxiliary ports become char
        // devices. Both use a detached class parent — the class layer is
        // model-agnostic since the Phase 1 decoupling.
        let location = crate::common::legacy_location(dev.descriptor().location);
        let char_dev: kclass::CharDeviceImpl = Box::new(SerialCharDev(port.clone()));
        match port.role() {
            SerialRole::Stdout => {
                register_console_parented(
                    ClassParent::detached("pl011-serial", location),
                    char_dev,
                )
                .map_err(crate::common::map_driver_error)?;
            }
            SerialRole::Auxiliary => {
                publish_char(ClassParent::detached("pl011-serial", location), char_dev)
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
            irq.as_ref().map(|i| i.number()).unwrap_or(usize::MAX),
        );
        let _ = RuntimeError::NotFound; // keep error vocabulary imported for future use
        Ok(Pl011Data {
            _irq: irq,
            _port: port,
            _regs: regs,
            _clk: clk,
        })
    }

    fn remove(&self, _dev: &mut BoundDevice<'_, PlatformBus, Pl011Data>) {
        // Guards in `Data` release after this in field order; nothing extra
        // to quiesce (the adopted stdout port must keep printing).
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
