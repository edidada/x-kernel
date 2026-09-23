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

#![no_std]

use driver_core::{
    BoundDevice, DeviceCtx, DriverDesc, DriverOps, IoMap, ProbeError, RuntimeError,
    platform::{OfMatch, PlatformBus, PlatformId},
};

static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("virtio,mmio"))];

/// Driver-private data: the mapped transport registers (the transport is
/// moved into the family's device on dispatch; the mapping lives for the
/// binding, mirroring the HAL ownership note in the porting map).
pub struct MmioData {
    #[allow(dead_code)]
    regs: Option<IoMap>,
}

pub struct VirtioMmioDriver;

impl DriverOps<PlatformBus> for VirtioMmioDriver {
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

        // The transport borrows the mapped registers; the family takes
        // ownership of the device, and the mapping stays with this binding.
        // Split the guard: keep it alive in Data while the transport holds
        // the raw pointer — the classic vendored-transport ownership note.
        let result = dispatch(dev, kind, transport);
        result?;

        Ok(MmioData { regs: Some(regs) })
    }

    fn remove(&self, _dev: &mut BoundDevice<'_, PlatformBus, MmioData>) {
        // Families own their devices; the mapping releases with `Data`
        // (boot-static devices are not removed in practice).
    }
}

#[allow(unused_variables)]
fn dispatch<T: virtio::Transport + 'static>(
    dev: &mut DeviceCtx<'_, PlatformBus>,
    kind: driver_base::DeviceKind,
    transport: T,
) -> Result<(), ProbeError> {
    use driver_base::DeviceKind;
    match kind {
        #[cfg(feature = "virtio-rng")]
        DeviceKind::Char => virtio_rng_core::activate(dev, transport),
        #[cfg(feature = "virtio-net")]
        DeviceKind::Net => {
            virtio_net_core::activate(dev, transport)?;
            Ok(())
        }
        #[cfg(feature = "virtio-blk")]
        DeviceKind::Block => {
            let irq = dev
                .irq(0)
                .map(|spec| spec.number)
                .map_err(|_| ProbeError::Fatal(RuntimeError::Invalid))?;
            virtio_blk_core::activate(dev, transport, irq)?;
            Ok(())
        }
        #[cfg(feature = "virtio-gpu")]
        DeviceKind::Display => virtio_gpu_core::activate(dev, transport),
        #[cfg(feature = "virtio-input")]
        DeviceKind::Input => virtio_input_core::activate(dev, transport),
        #[cfg(feature = "virtio-vsock")]
        DeviceKind::Vsock => virtio_vsock_core::activate(dev, transport),
        #[cfg(feature = "virtio-9p")]
        DeviceKind::Fs9p => virtio_9p_core::activate(dev, transport),
        _ => {
            log::warn!("virtio-mmio: unsupported device kind {kind:?}");
            Err(ProbeError::Fatal(RuntimeError::Unsupported))
        }
    }
}

/// `module_platform_driver()` equivalent.
pub fn register() {
    driver_core::global_domain().register_driver(DriverDesc::new(
        "virtio-mmio",
        IDS,
        VirtioMmioDriver,
    ));
}
