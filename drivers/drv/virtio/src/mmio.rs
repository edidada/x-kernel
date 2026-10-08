// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! virtio-mmio transport driver on the driver-core model.
//!
//! Claims DT nodes with `compatible = "virtio,mmio"`, maps the register
//! window, reads the device type from the transport header (the type is
//! *not* in the compatible string — this is the one bus where the driver
//! must probe the hardware to know what it is), and dispatches to the
//! per-family activation entry points.
//!
//! # Ownership contract
//!
//! `MmioActivation` owns **everything** the device needs, in one object:
//! the class publication (transport inside), the IRQ guard, and the MMIO
//! register mapping. Its `close()` runs the Linux remove order —
//!
//! ```text
//! quiesce hardware -> withdraw class publication -> release IRQ
//!     -> drop transport -> unmap MMIO registers
//! ```
//!
//! — so nothing outlives the mapping it dereferences. Field order alone
//! never carries this contract; the explicit close sequence does.

use driver_core::{
    BoundDevice, DeviceCtx, DriverDesc, DriverOps, IoMap, ProbeError, RuntimeError,
    platform::{OfMatch, PlatformBus, PlatformId},
};

static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("virtio,mmio"))];

/// One activated virtio-mmio device: publication + guards, closed in the
/// Linux remove order above. Held by `MmioData`; families construct it via
/// [`ActivationBuilder`].
pub trait MmioActivation: Send + Sync {
    /// Tear the device down: quiesce, unpublish, release IRQ, drop
    /// transport, unmap registers — in that order.
    fn close(self: alloc::boxed::Box<Self>);
}

/// Constructor helper handed to family activations: owns the register
/// mapping, so the transport's raw pointer stays valid for exactly as long
/// as the activation object.
pub struct ActivationBuilder {
    regs: IoMap,
}

impl ActivationBuilder {
    pub fn new(regs: IoMap) -> Self {
        Self { regs }
    }

    /// Wrap a family activation: publication guard first (withdrawn at
    /// close), then IRQ, then the register mapping last.
    pub fn build<A: MmioActivation + 'static>(self, inner: A) -> MmioData {
        MmioData {
            activation: Some(alloc::boxed::Box::new(inner)),
            regs: Some(self.regs),
        }
    }
}

/// Driver-private data: the family activation (publication + IRQ guards +
/// transport) and the register mapping, one ownership chain.
pub struct MmioData {
    activation: Option<alloc::boxed::Box<dyn MmioActivation>>,
    /// Register mapping; drops LAST (after the activation has closed and
    /// nothing dereferences the window anymore).
    regs: Option<IoMap>,
}

impl Drop for MmioData {
    fn drop(&mut self) {
        // Field order would do this, but the explicit sequence documents
        // the contract and guards against future field reordering.
        if let Some(activation) = self.activation.take() {
            activation.close();
        }
        drop(self.regs.take());
    }
}

pub struct MmioDriver;

impl DriverOps<PlatformBus> for MmioDriver {
    type Data = MmioData;

    fn probe(&self, dev: &mut DeviceCtx<'_, PlatformBus>) -> Result<MmioData, ProbeError> {
        let mmio = dev.mmio(0)?;
        let regs = dev.iomap(0)?;

        // SAFETY: `regs` covers the firmware-declared virtio-mmio register
        // window; the probe only reads the header.
        let Some((kind, transport)) =
            (unsafe { virtio::probe_mmio_device(regs.as_ptr(), mmio.size) })
        else {
            log::debug!("virtio-mmio: slot at {:#x} is empty", mmio.base);
            return Err(ProbeError::Fatal(RuntimeError::BadState));
        };

        let builder = ActivationBuilder::new(regs);
        dispatch(dev, kind, transport, builder)
    }

    fn remove(&self, dev: &mut BoundDevice<'_, PlatformBus, MmioData>) {
        // The explicit teardown runs from `MmioData::drop`; `remove` is the
        // quiesce hook for families that need ordered stopping beyond it.
        let _ = dev;
    }
}

#[allow(unused_variables)]
fn dispatch<T: virtio::Transport + Send + 'static>(
    dev: &mut DeviceCtx<'_, PlatformBus>,
    kind: driver_base::DeviceKind,
    transport: T,
    builder: ActivationBuilder,
) -> Result<MmioData, ProbeError> {
    use driver_base::DeviceKind;
    match kind {
        DeviceKind::Net => {
            #[cfg(feature = "net")]
            {
                crate::net::activate_mmio(dev, transport, builder)
            }
            #[cfg(not(feature = "net"))]
            {
                let _ = (dev, transport, builder);
                Err(ProbeError::Fatal(RuntimeError::Unsupported))
            }
        }
        DeviceKind::Block => {
            #[cfg(feature = "blk")]
            {
                let irq = dev
                    .irq(0)
                    .map_err(|_| ProbeError::Fatal(RuntimeError::Invalid))?;
                crate::blk::activate_mmio(dev, transport, irq, builder)
            }
            #[cfg(not(feature = "blk"))]
            {
                let _ = (dev, transport, builder);
                Err(ProbeError::Fatal(RuntimeError::Unsupported))
            }
        }
        DeviceKind::Char | DeviceKind::Hwrng => dispatch_char(dev, transport, builder),
        DeviceKind::Display => dispatch_simple_display(dev, transport, builder),
        DeviceKind::Input => dispatch_simple_input(dev, transport, builder),
        DeviceKind::Vsock => dispatch_simple_vsock(dev, transport, builder),
        DeviceKind::Fs9p => dispatch_simple_9p(dev, transport, builder),
        DeviceKind::Bus => {
            log::warn!("virtio-mmio: unexpected bus device kind");
            Err(ProbeError::Fatal(RuntimeError::Unsupported))
        }
    }
}

/// Char-family dispatch: exactly one char family is wired per build
/// (virtio-rng today); future char families get their own kind-checked arm.
#[allow(unused_variables)]
fn dispatch_char<T: virtio::Transport + Send + 'static>(
    dev: &mut DeviceCtx<'_, PlatformBus>,
    transport: T,
    builder: ActivationBuilder,
) -> Result<MmioData, ProbeError> {
    #[cfg(feature = "rng")]
    {
        let _ = dev;
        crate::rng::activate_mmio(dev, transport, builder)
    }
    #[cfg(not(feature = "rng"))]
    {
        let _ = (dev, transport, builder);
        Err(ProbeError::Fatal(RuntimeError::Unsupported))
    }
}

macro_rules! simple_family_dispatch {
    ($fn_name:ident, $feature:literal, $module:ident) => {
        #[allow(unused_variables)]
        fn $fn_name<T: virtio::Transport + Send + 'static>(
            dev: &mut DeviceCtx<'_, PlatformBus>,
            transport: T,
            builder: ActivationBuilder,
        ) -> Result<MmioData, ProbeError> {
            #[cfg(feature = $feature)]
            {
                crate::$module::activate_mmio(
                    dev.id(),
                    dev.descriptor().location,
                    transport,
                    builder,
                )
            }
            #[cfg(not(feature = $feature))]
            {
                let _ = (dev, transport, builder);
                Err(ProbeError::Fatal(RuntimeError::Unsupported))
            }
        }
    };
}

simple_family_dispatch!(dispatch_simple_display, "gpu", gpu);
simple_family_dispatch!(dispatch_simple_input, "input", input);
simple_family_dispatch!(dispatch_simple_vsock, "vsock", vsock);
simple_family_dispatch!(dispatch_simple_9p, "fs9p", virtio_9p);

/// `module_platform_driver()` equivalent.
pub fn register() {
    driver_core::global_domain().register_driver(DriverDesc::new("virtio-mmio", IDS, MmioDriver));
}
