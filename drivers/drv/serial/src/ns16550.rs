// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! NS16550 serial driver on the driver-core model.
//!
//! Two variants, one crate:
//! * `mmio` — device-tree UART nodes (`ns16550a`, `snps,dw-apb-uart`, ...),
//!   matching the serial-pl011 template with the NS16550 register layout
//!   decoded from the device's own firmware snapshot;
//! * `ioport` — x86 ISA COM ports (platform-static alias, no firmware).

use alloc::{boxed::Box, sync::Arc};

use driver_base::{Device, DriverOps, PlatformBus, PlatformId, ProbeError, RuntimeError};
#[cfg(feature = "ns16550-mmio")]
use serial_core::SerialMmioLayout;
use serial_core::{SerialIdent, SerialPort, SerialRole, take_early_port};
use subsys_char::class::CHAR;
use subsys_char_console::{adopt_stdout_guards, register_stdout_console};

use crate::serial_char_dev::SerialCharDev;

/// `struct of_device_id ns16550_of_match[]` (MMIO variant).
#[cfg(feature = "ns16550-mmio")]
static MMIO_IDS: &[PlatformId] = &[
    PlatformId::Of(driver_base::OfMatch::compatible("ns16550a")),
    PlatformId::Of(driver_base::OfMatch::compatible("ns16550")),
    PlatformId::Of(driver_base::OfMatch::compatible("snps,dw-apb-uart")),
    PlatformId::Of(driver_base::OfMatch::compatible("mrvl,mmp-uart")),
];

/// `struct platform_device_id` (x86 static alias).
#[cfg(feature = "ns16550-ioport")]
static IOPORT_IDS: &[PlatformId] = &[PlatformId::Alias(driver_base::AliasMatch::new(
    "serial-ns16550-ioport",
))];

/// Driver-private data: the published char device is owned by the class
/// registry; the mapping guard (MMIO variant, cold-start path) stays here so
/// the mapping lives exactly as long as the binding.
pub struct SerialData {
    /// The Data-owned char publication (auxiliary ports only): dropping
    /// unpublishes. FIRST field — before the mapping guard below, so
    /// consumer detach sees a live device. The stdout port deliberately
    /// holds none (console continuity across rebind).
    _publication: Option<driver_base::Publication<subsys_char::CharDeviceImpl>>,
    /// Kept alive for the binding: dropping it unmaps the cold-start UART
    /// window. Field placement (after the publication) encodes release order.
    // Ownership-only field: read access is intentionally impossible.
    #[expect(
        dead_code,
        reason = "CG-024-lint-exceptions: the guard exists for its Drop, never for reads"
    )]
    regs: Option<driver_base::IoMap>,
}

/// Publish the resolved port: stdout joins the console subsystem, auxiliary
/// ports become char devices.
fn publish(
    dev: &mut Device<'_, PlatformBus>,
    port: Arc<SerialPort>,
) -> Result<Option<driver_base::Publication<subsys_char::CharDeviceImpl>>, ProbeError> {
    let char_dev: subsys_char::CharDeviceImpl = Box::new(SerialCharDev(port.clone()));
    let published = dev
        .publish(&CHAR, char_dev, "serial")
        .map_err(crate::probe_error::map_driver_error)?;
    Ok(match port.role() {
        // The console subsystem owns the stdout publication (withdrawn on the
        // next rebind); auxiliary ports carry their own guard in `Data`.
        SerialRole::Stdout => {
            register_stdout_console(published);
            None
        }
        SerialRole::Auxiliary => Some(published),
    })
}

// ---------------------------------------------------------------------------
// MMIO variant
// ---------------------------------------------------------------------------

#[cfg(feature = "ns16550-mmio")]
pub struct Ns16550MmioDriver;

#[cfg(feature = "ns16550-mmio")]
impl DriverOps<PlatformBus> for Ns16550MmioDriver {
    type Data = SerialData;

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<SerialData, ProbeError> {
        let mmio = dev.res().mmio(0)?;
        let paddr = memaddr::PhysAddr::from_usize(mmio.base);
        let ident = SerialIdent::Mmio {
            paddr,
            size: mmio.size,
        };

        // Adopt the early stdout instance when this node is it; cold-start
        // ports map their own window (the guard lives in `Data`).
        let (port, regs): (Arc<SerialPort>, Option<driver_base::IoMap>) =
            match take_early_port(&ident) {
                Some(port) => (port, None),
                None => {
                    let regs = dev.res().iomap(0)?;
                    // Layout from the device's own firmware snapshot (the
                    // model already parsed the node); Linux 8250 defaults
                    // apply when the properties are absent.
                    let layout = SerialMmioLayout::from_props(
                        dev.fw().u32("reg-shift"),
                        dev.fw().u32("reg-io-width"),
                    )
                    .ok_or(ProbeError::Fatal(RuntimeError::Invalid))?;
                    // SAFETY: `regs` covers the firmware-declared window and
                    // stays alive in `Data` for the binding's lifetime.
                    let port = unsafe {
                        SerialPort::new_mmio_ns16550(
                            memaddr::VirtAddr::from_usize(regs.as_ptr() as usize),
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
        let publication = publish(dev, port.clone())?;
        // Console continuity, ownership side (see pl011): the stdout
        // binding's guards transfer to the console subsystem — the
        // publication outlives the binding, so the mapping (cold-start
        // ports) must survive with it. The adopted early stdout carries
        // no mapping (regs already None).
        let mut regs = regs;
        if matches!(port.role(), SerialRole::Stdout) {
            adopt_stdout_guards(None, regs.take());
        }
        Ok(SerialData {
            _publication: publication,
            regs,
        })
    }

    fn remove(&self, _dev: &mut Device<'_, PlatformBus>, _data: &mut SerialData) {}
}

// ---------------------------------------------------------------------------
// I/O-port variant (x86 ISA COM)
// ---------------------------------------------------------------------------

#[cfg(feature = "ns16550-ioport")]
pub struct Ns16550IoPortDriver;

#[cfg(feature = "ns16550-ioport")]
impl DriverOps<PlatformBus> for Ns16550IoPortDriver {
    type Data = SerialData;

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<SerialData, ProbeError> {
        use driver_base::ResourceKind;

        // x86 port I/O needs no mapping: read the range from resources.
        let port_base = dev
            .res()
            .map()
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
                Arc::new(unsafe {
                    SerialPort::new_ioport_ns16550(port_base, SerialRole::Auxiliary)
                })
            }
        };
        let publication = publish(dev, port.clone())?;
        // The ioport variant owns no MMIO mapping (cold-start guard is the
        // MMIO path's); regs stays empty. Stdout continuity still applies
        // to whatever guards this binding DOES own (none today — ioport
        // stdout is the adopted early port with no binding-owned guards).
        if matches!(port.role(), SerialRole::Stdout) {
            adopt_stdout_guards(None, None);
        }
        Ok(SerialData {
            _publication: publication,
            regs: None,
        })
    }

    fn remove(&self, _dev: &mut Device<'_, PlatformBus>, _data: &mut SerialData) {}
}

// `module_platform_driver()` equivalents: one hardware family, two buses.
#[cfg(feature = "ns16550-mmio")]
driver_base::declare_driver!(
    PlatformBus,
    Ns16550MmioDriver,
    "serial-ns16550-mmio",
    MMIO_IDS => register_mmio
);

#[cfg(feature = "ns16550-ioport")]
driver_base::declare_driver!(
    PlatformBus,
    Ns16550IoPortDriver,
    "serial-ns16550-ioport",
    IOPORT_IDS => register_ioport
);
