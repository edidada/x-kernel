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
//! This replaces the legacy platform backend's enumeration-time transport
//! probing: the probe-on-probe now belongs to the driver that owns the
//! device, and the families activate through their shared `activate`
//! entry points (same code paths as their PCI drivers).

use driver_core::{
    BoundDevice, DeviceCtx, DriverDesc, DriverOps, IoMap, ProbeError, RuntimeError,
    platform::{OfMatch, PlatformBus, PlatformId},
};

static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("virtio,mmio"))];

/// Driver-private data: the per-family activation state (IRQ guard, block
/// activation — whatever the family's `activate` returns) plus the mapped
/// transport registers. Both live for the binding; release order is
/// activation drop (quiesce hardware) first, registers unmap last.
pub struct MmioData {
    state: MmioFamilyState,
    regs: Option<IoMap>,
}

/// The per-family runtime state returned by dispatch, held by `MmioData`.
enum MmioFamilyState {
    Net(Option<driver_core::DevIrq>),
    Block(alloc::boxed::Box<dyn crate::blk::Activation>),
    Other,
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

        // The family owns the device; the mapping stays with this binding.
        // The dispatch result (IRQ guard / block activation) is held by
        // `Data` so the hardware stays live for the binding.
        let state = dispatch(dev, kind, transport)?;
        Ok(MmioData {
            state,
            regs: Some(regs),
        })
    }

    fn remove(&self, _dev: &mut BoundDevice<'_, PlatformBus, MmioData>) {
        // Drop order from `Data`: family state first (IRQ release, block
        // activation close — hardware quiesce), then the register mapping.
        // Both happen via `Data`'s field order on drop.
    }
}

#[allow(unused_variables)]
fn dispatch<T: virtio::Transport + 'static>(
    dev: &mut DeviceCtx<'_, PlatformBus>,
    kind: driver_base::DeviceKind,
    transport: T,
) -> Result<MmioFamilyState, ProbeError> {
    use driver_base::DeviceKind;
    match kind {
        #[cfg(all(feature = "rng", not(feature = "net"), not(feature = "blk")))]
        DeviceKind::Char => {
            crate::rng::activate(dev, transport)?;
            Ok(MmioFamilyState::Other)
        }
        #[cfg(feature = "net")]
        DeviceKind::Net => {
            let irq = crate::net::activate(dev, transport)?;
            Ok(MmioFamilyState::Net(irq))
        }
        #[cfg(feature = "blk")]
        DeviceKind::Block => {
            let irq = dev
                .irq(0)
                .map(|spec| spec.number)
                .map_err(|_| ProbeError::Fatal(RuntimeError::Invalid))?;
            let activation = crate::blk::activate(dev, transport, irq)?;
            Ok(MmioFamilyState::Block(activation))
        }
        #[cfg(any(
            feature = "rng",
            feature = "gpu",
            feature = "input",
            feature = "vsock",
            feature = "fs9p"
        ))]
        DeviceKind::Char
        | DeviceKind::Display
        | DeviceKind::Input
        | DeviceKind::Vsock
        | DeviceKind::Fs9p => {
            // The families own their devices through the class registries;
            // no per-family runtime state beyond the register mapping.
            dispatch_simple(dev, kind, transport)?;
            Ok(MmioFamilyState::Other)
        }
        _ => {
            log::warn!("virtio-mmio: unsupported device kind {kind:?}");
            Err(ProbeError::Fatal(RuntimeError::Unsupported))
        }
    }
}

#[allow(unused_variables)]
fn dispatch_simple<T: virtio::Transport + 'static>(
    dev: &mut DeviceCtx<'_, PlatformBus>,
    kind: driver_base::DeviceKind,
    transport: T,
) -> Result<(), ProbeError> {
    use driver_base::DeviceKind;
    match kind {
        #[cfg(feature = "rng")]
        DeviceKind::Char => crate::rng::activate(dev, transport),
        #[cfg(feature = "gpu")]
        DeviceKind::Display => crate::gpu::activate(dev, transport),
        #[cfg(feature = "input")]
        DeviceKind::Input => crate::input::activate(dev, transport),
        #[cfg(feature = "vsock")]
        DeviceKind::Vsock => crate::vsock::activate(dev, transport),
        #[cfg(feature = "fs9p")]
        DeviceKind::Fs9p => crate::virtio_9p::activate(dev, transport),
        _ => Err(ProbeError::Fatal(RuntimeError::Unsupported)),
    }
}

/// `module_platform_driver()` equivalent.
pub fn register() {
    driver_core::global_domain().register_driver(DriverDesc::new("virtio-mmio", IDS, MmioDriver));
}
