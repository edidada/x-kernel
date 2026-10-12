// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO PCI transport integration.

use alloc::vec::Vec;

use device_traits::DeviceKind;
use driver_core::provider::{IrqOp, IrqRouteDesc};
use kirq::IrqController;
use pci::MmioCam;

#[cfg(target_arch = "x86_64")]
mod x86_64;
use virtio_drivers::{
    PhysAddr, Result as VirtIoResult,
    transport::{
        DeviceStatus, DeviceType, InterruptStatus, Transport,
        pci::{
            PciTransport as RawPciTransport, VirtioPciError,
            bus::{Command, DeviceFunction, DeviceFunctionInfo, PciRoot},
            virtio_device_type,
        },
    },
};
use zerocopy::{FromBytes, Immutable, IntoBytes};

#[cfg(target_arch = "x86_64")]
use self::x86_64::{PciIrqState, PciMsixState, setup_msix};
use crate::{VirtIoHal, as_device_kind};

/// PCI transport wrapper with x86 MSI-X queue-vector setup.
pub struct PciTransport {
    inner: RawPciTransport,
    #[cfg(target_arch = "x86_64")]
    irq_state: PciIrqState,
    /// MMIO leases for every region the vendored constructor mapped through
    /// the HAL (declared last: the vendored transport drops before the
    /// mappings are released). The mapping lives exactly as long as this
    /// transport — and therefore as long as any class publication still
    /// holding the device.
    mmio_leases: Vec<driver_core::IoMap>,
}

#[cfg(target_arch = "x86_64")]
type PciMsixStateOption = Option<PciMsixState>;

#[cfg(not(target_arch = "x86_64"))]
type PciMsixStateOption = Option<()>;

impl PciTransport {
    /// Construct a new PCI VirtIO transport.
    pub fn new<H: VirtIoHal>(
        root: &mut PciRoot<MmioCam<'static>>,
        bdf: DeviceFunction,
    ) -> Result<Self, VirtioPciError> {
        Self::new_with_msix::<H>(root, bdf, None)
    }

    fn new_with_msix<H: VirtIoHal>(
        root: &mut PciRoot<MmioCam<'static>>,
        bdf: DeviceFunction,
        #[cfg_attr(
            not(target_arch = "x86_64"),
            expect(
                unused_variables,
                reason = "CG-024-lint-exceptions: unused_variables: MSI-X transport setup is \
                          implemented only on x86_64"
            )
        )]
        msix: PciMsixStateOption,
    ) -> Result<Self, VirtioPciError> {
        let inner = match RawPciTransport::new::<H, MmioCam>(root, bdf) {
            Ok(inner) => inner,
            Err(err) => {
                #[cfg(target_arch = "x86_64")]
                if let Some(msix) = msix {
                    msix.release();
                }
                return Err(err);
            }
        };
        Ok(Self {
            inner,
            #[cfg(target_arch = "x86_64")]
            irq_state: PciIrqState::new(msix),
            mmio_leases: Vec::new(),
        })
    }

    /// Take ownership of the MMIO mapping leases gathered during the
    /// probe scope. Called once by the runtime right after construction.
    pub fn retain_mappings(&mut self, leases: Vec<driver_core::IoMap>) {
        debug_assert!(self.mmio_leases.is_empty(), "mappings retained twice");
        self.mmio_leases = leases;
    }

    #[cfg(target_arch = "x86_64")]
    fn fail_msix(&mut self) {
        self.irq_state.fail();
        let failed_status =
            (self.inner.get_status() | DeviceStatus::FAILED) & !DeviceStatus::DRIVER_OK;
        self.inner.set_status(failed_status);
    }
}

impl Transport for PciTransport {
    fn device_type(&self) -> DeviceType {
        self.inner.device_type()
    }

    fn read_device_features(&mut self) -> u64 {
        self.inner.read_device_features()
    }

    fn write_driver_features(&mut self, driver_features: u64) {
        self.inner.write_driver_features(driver_features);
    }

    fn max_queue_size(&mut self, queue: u16) -> u32 {
        #[cfg(target_arch = "x86_64")]
        let max_size = {
            if self.irq_state.is_failed() {
                return 0;
            }

            let max_size = self.inner.max_queue_size(queue);
            if max_size == 0 {
                return 0;
            }

            let msix_failed = if let Some(msix) = self.irq_state.msix_mut()
                && !msix.set_queue_vector(queue)
            {
                Some(msix.bdf())
            } else {
                None
            };
            if let Some(bdf) = msix_failed {
                log::warn!(
                    "PCI virtio device at {:?}: MSI-X queue {} vector setup failed",
                    bdf,
                    queue
                );
                self.fail_msix();
                return 0;
            }

            max_size
        };

        #[cfg(not(target_arch = "x86_64"))]
        let max_size = self.inner.max_queue_size(queue);

        max_size
    }

    fn notify(&mut self, queue: u16) {
        self.inner.notify(queue);
    }

    fn get_status(&self) -> DeviceStatus {
        self.inner.get_status()
    }

    fn set_status(&mut self, status: DeviceStatus) {
        #[cfg(target_arch = "x86_64")]
        if self.irq_state.is_failed() && status.contains(DeviceStatus::DRIVER_OK) {
            let failed_status = (status | DeviceStatus::FAILED) & !DeviceStatus::DRIVER_OK;
            self.inner.set_status(failed_status);
            return;
        }

        self.inner.set_status(status);
        #[cfg(target_arch = "x86_64")]
        if status.contains(DeviceStatus::DRIVER_OK)
            && let Some(msix) = self.irq_state.msix_mut()
        {
            msix.activate();
        }

        #[cfg(target_arch = "x86_64")]
        let config_msix_failed = if status.contains(DeviceStatus::FEATURES_OK)
            && !status.contains(DeviceStatus::DRIVER_OK)
            && let Some(msix) = self.irq_state.msix_mut()
            && !msix.set_config_vector()
        {
            Some(msix.bdf())
        } else {
            None
        };
        #[cfg(target_arch = "x86_64")]
        if let Some(bdf) = config_msix_failed {
            log::warn!(
                "PCI virtio device at {:?}: MSI-X config vector setup failed",
                bdf
            );
            self.fail_msix();
        }
    }

    fn set_guest_page_size(&mut self, guest_page_size: u32) {
        self.inner.set_guest_page_size(guest_page_size);
    }

    fn requires_legacy_layout(&self) -> bool {
        self.inner.requires_legacy_layout()
    }

    fn queue_set(
        &mut self,
        queue: u16,
        size: u32,
        descriptors: PhysAddr,
        driver_area: PhysAddr,
        device_area: PhysAddr,
    ) {
        self.inner
            .queue_set(queue, size, descriptors, driver_area, device_area);
    }

    fn queue_unset(&mut self, queue: u16) {
        self.inner.queue_unset(queue);
    }

    fn queue_used(&mut self, queue: u16) -> bool {
        self.inner.queue_used(queue)
    }

    fn ack_interrupt(&mut self) -> InterruptStatus {
        self.inner.ack_interrupt()
    }

    fn read_config_generation(&self) -> u32 {
        self.inner.read_config_generation()
    }

    fn read_config_space<T: FromBytes + IntoBytes>(&self, offset: usize) -> VirtIoResult<T> {
        self.inner.read_config_space(offset)
    }

    fn write_config_space<T: IntoBytes + Immutable>(
        &mut self,
        offset: usize,
        value: T,
    ) -> VirtIoResult<()> {
        self.inner.write_config_space(offset, value)
    }
}

/// Try to probe a VirtIO PCI device from the given PCI address.
///
/// If the device is recognized, returns the device type and a transport object
/// for later operations. Otherwise, returns [`None`].
pub fn probe_pci_transport_only<H: VirtIoHal>(
    root: &mut PciRoot<MmioCam<'static>>,
    bdf: DeviceFunction,
    dev_info: &DeviceFunctionInfo,
    irq_provider: &'static dyn IrqOp,
) -> Option<(DeviceKind, PciTransport, Option<usize>)> {
    let dev_kind = virtio_device_type(dev_info).and_then(as_device_kind)?;

    // x86_64: MSI-X setup is limited to network devices for the virtio-net
    // latency fix. Other virtio device kinds keep legacy IRQs (resolved from
    // the descriptor's resource table by the caller) until their interrupt
    // behavior is audited with the shared queue-vector setup.
    // The transport-bound irq (MSI-X config vector) when MSI-X is active;
    // the caller falls back to the descriptor's legacy route otherwise.
    #[cfg(target_arch = "x86_64")]
    let (msix, transport_irq) = if dev_kind == DeviceKind::Net {
        match setup_msix::<H>(root, bdf, irq_provider) {
            Some((irq, msix)) => (Some(msix), Some(irq)),
            None => (None, None),
        }
    } else {
        (None, None)
    };

    #[cfg(not(target_arch = "x86_64"))]
    let _ = irq_provider;
    #[cfg(not(target_arch = "x86_64"))]
    let transport_irq = None;

    #[cfg(not(target_arch = "x86_64"))]
    let msix = None;

    let transport = PciTransport::new_with_msix::<H>(root, bdf, msix).ok()?;
    if matches!(dev_kind, DeviceKind::Vsock | DeviceKind::Input) {
        let (_, command) = root.get_status_command(bdf);
        root.set_command(bdf, command | Command::INTERRUPT_DISABLE);
    }
    Some((dev_kind, transport, transport_irq))
}

/// Resolve a legacy INTx irq from the descriptor's resource table entry.
///
/// The bus resolved the firmware route once at discovery (hwirq + trigger +
/// controller); this maps it to the OS-visible virtual irq. The static
/// per-arch fallback covers platforms without firmware routing.
pub fn resolve_legacy_irq(spec: &driver_core::IrqSpec, irq_provider: &dyn IrqOp) -> Option<usize> {
    // Only a firmware route needs mapping here; a resolved virq passes
    // through unchanged.
    let driver_core::IrqSpec::FirmwareRoute {
        controller,
        domain,
        hwirq,
        trigger,
    } = *spec
    else {
        return match *spec {
            driver_core::IrqSpec::ResolvedVirq { number, .. } => Some(number),
            driver_core::IrqSpec::FirmwareRoute { .. } => None,
        };
    };
    let desc = IrqRouteDesc {
        hwirq,
        trigger,
        controller: match controller {
            driver_core::IrqController::Gic => IrqController::Gic,
            driver_core::IrqController::Plic => IrqController::Plic,
            driver_core::IrqController::IoApic => IrqController::IoApic,
            driver_core::IrqController::LoongArchExtioi => IrqController::LoongArchExtioi,
            driver_core::IrqController::Msi | driver_core::IrqController::Unknown => {
                IrqController::Unknown
            }
        },
        domain,
    };
    irq_provider.map_irq(desc).ok().map(|irq| irq.number())
}
