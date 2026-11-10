// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Firmware-described early console ports — the Linux `earlycon` role.
//!
//! The module owns the part of early-console bring-up that is specific to
//! the serial family: `compatible` → family selection, the device-tree
//! layout properties (`reg-shift`, `reg-io-width`) → typed layout, and
//! construction of a [`SerialPort`] from an already-mapped window.
//!
//! Mapping the window, translating the interrupt description into a `kirq`
//! `IrqDesc`, and *choosing* the stdout node stay in the boot-side console
//! crate: those are `khal`/`memspace`/`kirq` concerns that must not reach a
//! model driver, and they run before the driver model exists.
//!
//! Compiled only with the `earlycon` feature so model-only serial builds do
//! not carry the firmware-parsing (`of`) dependency.

use memaddr::{PhysAddr, VirtAddr};

#[cfg(feature = "ns16550-mmio")]
use crate::SerialMmioLayout;
use crate::{SerialPort, SerialRole};
/// UART family a firmware console node describes.
///
/// Variants exist only for the family backends compiled into this build:
/// a node whose family has no enabled backend is not claimable by this
/// library and the boot console crate reports "no recognized stdout".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerialFamily {
    /// ARM PrimeCell PL011 (`arm,pl011`).
    #[cfg(feature = "pl011")]
    Pl011,
    /// NS16550-compatible MMIO (`ns16550a`, `snps,dw-apb-uart`, ...).
    #[cfg(feature = "ns16550-mmio")]
    Ns16550Mmio,
}

/// A firmware-described MMIO UART: everything needed to build a
/// [`SerialPort`] once its window is mapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EarlyMmioPort {
    /// UART family selected from the node's `compatible` list.
    pub family: SerialFamily,
    /// Physical base address of the register window (`reg`).
    pub paddr: PhysAddr,
    /// Size of the register window in bytes (`reg`).
    pub size: usize,
    /// Register stride and access width from `reg-shift` / `reg-io-width`.
    #[cfg(feature = "ns16550-mmio")]
    pub layout: SerialMmioLayout,
}

impl EarlyMmioPort {
    /// Decode a firmware console node into early-port facts.
    ///
    /// Returns `None` when the node has no `reg`, or when its `compatible`
    /// list names a family this build does not support (a build with no
    /// backend, or a UART belonging to another family). The node's `reg`
    /// entry is the first one, matching the single-window console case.
    pub fn from_firmware_node(node: &of::FdtNode<'static, 'static>) -> Option<Self> {
        let reg = node.reg()?.next()?;
        let paddr = PhysAddr::from_usize(reg.starting_address as usize);
        let size = reg.size;

        #[cfg(feature = "pl011")]
        if node
            .compatibles()
            .any(|compatible| compatible == "arm,pl011")
        {
            return Some(Self {
                family: SerialFamily::Pl011,
                paddr,
                size,
                // PL011 ignores the NS16550 layout fields; the defaults keep
                // one struct shape instead of a variant-specific one.
                #[cfg(feature = "ns16550-mmio")]
                layout: SerialMmioLayout::hw_default(),
            });
        }

        #[cfg(feature = "ns16550-mmio")]
        if node.compatibles().any(|compatible| {
            matches!(
                compatible,
                "ns16550a" | "ns16550" | "snps,dw-apb-uart" | "mrvl,mmp-uart"
            )
        }) {
            return Some(Self {
                family: SerialFamily::Ns16550Mmio,
                paddr,
                size,
                layout: SerialMmioLayout::from_props(
                    node.property_u32("reg-shift"),
                    node.property_u32("reg-io-width"),
                )?,
            });
        }

        None
    }

    /// Resolve the firmware `chosen` stdout node, if the device tree
    /// describes one.
    ///
    /// The caller keeps the node so it can also translate the node's
    /// interrupt (`earlycon` deliberately does not depend on `kirq`).
    pub fn stdout_firmware_node() -> Option<of::FdtNode<'static, 'static>> {
        let stdout_path = of::chosen_stdout_path()?;
        of::resolve_node(stdout_path)
    }

    /// Build the early [`SerialPort`] over `vaddr`.
    ///
    /// # Safety
    ///
    /// `vaddr` must be the kernel virtual address of an exclusively-owned
    /// mapping of exactly `self.size` bytes at `self.paddr`, established by
    /// the caller (`memspace::iomap_device`) and kept alive for the port's
    /// lifetime. The register layout must match the described hardware.
    pub unsafe fn build(&self, vaddr: VirtAddr, role: SerialRole) -> SerialPort {
        match self.family {
            #[cfg(feature = "pl011")]
            SerialFamily::Pl011 => {
                // SAFETY: the caller's mapping contract; PL011 ignores the
                // firmware layout fields.
                unsafe { SerialPort::new_mmio_pl011(vaddr, self.paddr, self.size, role) }
            }
            #[cfg(feature = "ns16550-mmio")]
            SerialFamily::Ns16550Mmio => {
                // SAFETY: the caller's mapping contract; `layout` carries the
                // node's `reg-shift` / `reg-io-width`.
                unsafe {
                    SerialPort::new_mmio_ns16550(
                        vaddr,
                        self.paddr,
                        self.size,
                        role,
                        self.layout.reg_shift,
                        self.layout.reg_width,
                    )
                }
            }
        }
    }
}
