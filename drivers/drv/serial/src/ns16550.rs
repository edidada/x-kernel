// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! NS16550 serial driver on the driver-core model.
//!
//! Two variants, one crate:
//! * `mmio` — device-tree UART nodes (`ns16550a`, `snps,dw-apb-uart`, ...),
//!   matching the serial-pl011 template with the NS16550 register layout
//!   decoded from the early-console layout table;
//! * `ioport` — x86 ISA COM ports (platform-static alias, no firmware).

#![no_std]

extern crate alloc;

use alloc::{boxed::Box, sync::Arc};

use char_driver::CharDevice;
use console_driver::serial::{SerialIdent, SerialPort, SerialRole, take_early_port};
use driver_base::{Device, DeviceKind, DriverResult};
use driver_core::{
    BoundDevice, DeviceCtx, DriverDesc, DriverOps, ProbeError, RuntimeError,
    platform::{PlatformBus, PlatformId},
};
use kclass::{ClassParent, publish_char};

/// `struct of_device_id ns16550_of_match[]` (MMIO variant).
#[cfg(feature = "mmio")]
static MMIO_IDS: &[PlatformId] = &[
    PlatformId::Of(driver_core::platform::OfMatch::compatible("ns16550a")),
    PlatformId::Of(driver_core::platform::OfMatch::compatible("ns16550")),
    PlatformId::Of(driver_core::platform::OfMatch::compatible(
        "snps,dw-apb-uart",
    )),
    PlatformId::Of(driver_core::platform::OfMatch::compatible("mrvl,mmp-uart")),
];

/// `struct platform_device_id` (x86 static alias).
#[cfg(feature = "ioport")]
static IOPORT_IDS: &[PlatformId] = &[PlatformId::Alias(driver_core::platform::AliasMatch::new(
    "serial-ns16550-ioport",
))];

/// Driver-private data: the published char device is owned by the class
/// registry; the mapping guard (MMIO variant, cold-start path) stays here so
/// the mapping lives exactly as long as the binding.
pub struct SerialData {
    #[allow(dead_code)]
    regs: Option<driver_core::IoMap>,
}

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

/// Publish the resolved port: stdout joins the console subsystem, auxiliary
/// ports become char devices.
fn publish(dev: &mut DeviceCtx<'_, PlatformBus>, port: Arc<SerialPort>) -> Result<(), ProbeError> {
    let location = crate::common::legacy_location(dev.descriptor().location);
    let char_dev: kclass::CharDeviceImpl = Box::new(SerialCharDev(port.clone()));
    match port.role() {
        SerialRole::Stdout => console_driver::runtime::register_console_parented(
            ClassParent::detached("serial-ns16550", location),
            char_dev,
        )
        .map_err(map_driver_error),
        SerialRole::Auxiliary => {
            publish_char(ClassParent::detached("serial-ns16550", location), char_dev)
                .map(drop)
                .map_err(map_driver_error)
        }
    }
}

// ---------------------------------------------------------------------------
// MMIO variant
// ---------------------------------------------------------------------------

#[cfg(feature = "mmio")]
pub struct Ns16550MmioDriver;

#[cfg(feature = "mmio")]
impl DriverOps<PlatformBus> for Ns16550MmioDriver {
    type Data = SerialData;

    fn probe(&self, dev: &mut DeviceCtx<'_, PlatformBus>) -> Result<SerialData, ProbeError> {
        let mmio = dev.mmio(0)?;
        let paddr = khal::mem::PhysAddr::from_usize(mmio.base);
        let ident = SerialIdent::Mmio {
            paddr,
            size: mmio.size,
        };

        // Adopt the early stdout instance when this node is it; cold-start
        // ports map their own window (the guard lives in `Data`).
        let (port, regs): (Arc<SerialPort>, Option<driver_core::IoMap>) =
            match take_early_port(&ident) {
                Some(port) => (port, None),
                None => {
                    let regs = dev.iomap(0)?;
                    let layout = console_driver::ns16550_mmio_layout_for_paddr(mmio.base)
                        .ok_or(ProbeError::Fatal(RuntimeError::Invalid))?;
                    // SAFETY: `regs` covers the firmware-declared window and
                    // stays alive in `Data` for the binding's lifetime.
                    let port = unsafe {
                        SerialPort::new_mmio_ns16550(
                            khal::mem::VirtAddr::from_usize(regs.as_ptr() as usize),
                            paddr,
                            mmio.size,
                            SerialRole::Auxiliary,
                            layout.reg_shift,
                            layout.reg_width,
                        )
                    };
                    (Arc::new(port), Some(regs))
                }
            };
        publish(dev, port)?;
        Ok(SerialData { regs })
    }

    fn remove(&self, _dev: &mut BoundDevice<'_, PlatformBus, SerialData>) {}
}

// ---------------------------------------------------------------------------
// I/O-port variant (x86 ISA COM)
// ---------------------------------------------------------------------------

#[cfg(feature = "ioport")]
pub struct Ns16550IoPortDriver;

#[cfg(feature = "ioport")]
impl DriverOps<PlatformBus> for Ns16550IoPortDriver {
    type Data = SerialData;

    fn probe(&self, dev: &mut DeviceCtx<'_, PlatformBus>) -> Result<SerialData, ProbeError> {
        use driver_core::ResourceKind;

        // x86 port I/O needs no mapping: read the range from resources.
        let port_base = dev
            .resources()
            .entries()
            .iter()
            .find_map(|entry| match entry.kind {
                ResourceKind::IoPort(range) => Some(range.base),
                _ => None,
            })
            .ok_or(ProbeError::Fatal(RuntimeError::Invalid))?;

        let ident = SerialIdent::IoPort { port: port_base };
        let port = match take_early_port(&ident) {
            Some(port) => port,
            None => {
                // SAFETY: the port range comes from the platform-static
                // descriptor and is exclusively owned by this device.
                unsafe { SerialPort::new_ioport_ns16550(port_base, SerialRole::Auxiliary) }
            }
        };
        publish(dev, Arc::new(port))?;
        Ok(SerialData)
    }

    fn remove(&self, _dev: &mut BoundDevice<'_, PlatformBus, SerialData>) {}
}

/// `module_platform_driver()` equivalents.
#[cfg(feature = "mmio")]
pub fn register_mmio() {
    driver_core::global_domain().register_driver(DriverDesc::new(
        "serial-ns16550-mmio",
        MMIO_IDS,
        Ns16550MmioDriver,
    ));
}

#[cfg(feature = "ioport")]
pub fn register_ioport() {
    driver_core::global_domain().register_driver(DriverDesc::new(
        "serial-ns16550-ioport",
        IOPORT_IDS,
        Ns16550IoPortDriver,
    ));
}
