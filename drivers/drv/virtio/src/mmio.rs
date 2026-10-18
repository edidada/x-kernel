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
//! `Activation` owns **everything** the device needs, in one object:
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

use driver_base::{
    Device, DriverOps, IoMap, OfMatch, PlatformBus, PlatformId, ProbeError, RuntimeError,
};
use virtio_devices::VirtioDeviceKind;

static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("virtio,mmio"))];

/// Constructor helper handed to family activations: owns the register
/// mapping, so the transport's raw pointer stays valid for exactly as long
/// as the activation object.
pub struct ActivationBuilder {
    regs: Option<IoMap>,
}

impl ActivationBuilder {
    pub fn new(regs: IoMap) -> Self {
        Self { regs: Some(regs) }
    }

    /// Hand the register mapping to the device object being constructed.
    ///
    /// The mapping's lifetime follows the RUNTIME (the published device
    /// and every consumer clone), not the driver Data — the Linux BAR
    /// model (pci_release_regions runs after the last put_device, not at
    /// driver unbind). Families call this before constructing their device
    /// object, then [`Self::build`] the activation without the mapping.
    pub fn take_regs(&mut self) -> Option<IoMap> {
        self.regs.take()
    }

    /// Wrap a family activation: publication guard first (withdrawn at
    /// close), then IRQ. The register mapping is NOT held here — the
    /// family moved it into the device object via [`Self::take_regs`].
    pub fn build<A: crate::common::Activation + 'static>(self, inner: A) -> MmioData {
        MmioData {
            activation: Some(alloc::boxed::Box::new(inner)),
        }
    }
}

/// Driver-private data: the family activation (publication + IRQ guards +
/// transport). The register mapping is NOT part of this chain — it follows
/// the device object (see []).
pub struct MmioData {
    activation: Option<alloc::boxed::Box<dyn crate::common::Activation>>,
}

impl Drop for MmioData {
    fn drop(&mut self) {
        // The register mapping is NOT dropped here: the family moved it
        // into the device object (ActivationBuilder::take_regs), so it
        // lives as long as any Publication clone referencing the device —
        // unpublish cannot leave a consumer clone with a dangling pointer.
        if let Some(activation) = self.activation.take() {
            activation.close();
        }
    }
}

pub struct MmioDriver;

impl DriverOps<PlatformBus> for MmioDriver {
    type Data = MmioData;

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<MmioData, ProbeError> {
        let mmio = dev.res().mmio(0)?;
        let regs = dev.res().iomap(0)?;

        // SAFETY: `regs` covers the firmware-declared virtio-mmio register
        // window; the probe only reads the header.
        let Some((kind, transport)) =
            (unsafe { virtio_devices::probe_mmio_device(regs.as_ptr(), mmio.size) })
        else {
            log::debug!("virtio-mmio: slot at {:#x} is empty", mmio.base);
            return Err(ProbeError::Fatal(RuntimeError::BadState));
        };

        let builder = ActivationBuilder::new(regs);
        dispatch(dev, kind, transport, builder)
    }

    fn remove(&self, dev: &mut Device<'_, PlatformBus>, _data: &mut MmioData) {
        // The explicit teardown runs from `MmioData::drop`; `remove` is the
        // quiesce hook for families that need ordered stopping beyond it.
        let _ = dev;
    }
}

fn dispatch<T: virtio_devices::Transport + Send + 'static>(
    dev: &mut Device<'_, PlatformBus>,
    kind: VirtioDeviceKind,
    transport: T,
    builder: ActivationBuilder,
) -> Result<MmioData, ProbeError> {
    match kind {
        VirtioDeviceKind::Net => {
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
        VirtioDeviceKind::Block => {
            #[cfg(feature = "blk")]
            {
                let irq = dev
                    .res()
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
        VirtioDeviceKind::Hwrng => dispatch_hwrng(dev, transport, builder),
        VirtioDeviceKind::Display => dispatch_simple_display(dev, transport, builder),
        VirtioDeviceKind::Input => dispatch_simple_input(dev, transport, builder),
        VirtioDeviceKind::Vsock => dispatch_simple_vsock(dev, transport, builder),
        VirtioDeviceKind::Fs9p => dispatch_simple_9p(dev, transport, builder),
    }
}

/// Hwrng-family dispatch: one family per build (virtio-rng today).
fn dispatch_hwrng<T: virtio_devices::Transport + Send + 'static>(
    dev: &mut Device<'_, PlatformBus>,
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
        fn $fn_name<T: virtio_devices::Transport + Send + 'static>(
            dev: &mut Device<'_, PlatformBus>,
            transport: T,
            builder: ActivationBuilder,
        ) -> Result<MmioData, ProbeError> {
            #[cfg(feature = $feature)]
            {
                crate::$module::activate_mmio(dev, transport, builder)
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

driver_base::declare_driver!(PlatformBus, MmioDriver, "virtio-mmio", IDS);
