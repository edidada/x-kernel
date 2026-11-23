// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Rockchip IOMMU (v1/v2 register file) driver.
//!
//! One probe brings up one IOMMU instance: it maps the `reg` window,
//! registers the fault interrupt, builds the instance's [`IommuDomain`] over
//! a [`RockchipV2`](io_pgtable::RockchipV2) two-level page table and
//! publishes the instance to the [`kiommu`] core under its firmware node
//! identity, so `iommus = <&this-node>` references on media devices resolve.
//!
//! # Register protocol
//!
//! Commands are written to `MMU_COMMAND` and confirmed against `MMU_STATUS`
//! with a bounded poll. TLB maintenance runs between
//! `ENABLE_STALL`/`DISABLE_STALL`, and faults are cleared with `INT_CLEAR`
//! followed by `ZAP_CACHE` and `PAGE_FAULT_DONE` — the sequence that
//! terminates the stalled transaction and prevents an interrupt storm.
//!
//! The v2 descriptor format stretches page-table and page addresses to 40
//! bits; the page-table layout itself lives in `io_pgtable::rockchip_v2`.
//!
//! This crate is a platform integration driver: like the interrupt and timer
//! drivers it consumes host capabilities (`driver-os-api-xkernel`, `kdma`)
//! while the reusable logic (register protocol, page-table format) stays in
//! OS-neutral crates.

#![no_std]

extern crate alloc;

use alloc::{boxed::Box, sync::Arc};
use core::ptr::NonNull;

use driver_base::{DeviceKind, DriverError, DriverResult};
use driver_os_api::{IrqEvent, IrqHandler as DevIrqHandler, ResError};
use driver_os_api_xkernel::XKernelResourceProvider;
use io_pgtable::{RockchipV2, TableAllocator};
use kiommu::{InstanceId, IommuDomain, IommuError, IommuFault, IommuHardware};

static PROVIDER: XKernelResourceProvider = XKernelResourceProvider::new();

/// MMU register offsets (shared by the v1 and v2 register files).
mod reg {
    pub(super) const DTE_ADDR: usize = 0x00;
    pub(super) const STATUS: usize = 0x04;
    pub(super) const COMMAND: usize = 0x08;
    pub(super) const PAGE_FAULT_ADDR: usize = 0x0c;
    pub(super) const INT_CLEAR: usize = 0x18;
    pub(super) const INT_STATUS: usize = 0x20;
}

/// `MMU_STATUS` bits.
mod status {
    pub(super) const PAGING_ENABLED: u32 = 1 << 0;
    pub(super) const STALL_ACTIVE: u32 = 1 << 2;
    pub(super) const PAGE_FAULT_IS_WRITE: u32 = 1 << 5;
}

/// `MMU_COMMAND` values.
mod command {
    pub(super) const ENABLE_PAGING: u32 = 0;
    pub(super) const DISABLE_PAGING: u32 = 1;
    pub(super) const ENABLE_STALL: u32 = 2;
    pub(super) const DISABLE_STALL: u32 = 3;
    pub(super) const ZAP_CACHE: u32 = 4;
    pub(super) const PAGE_FAULT_DONE: u32 = 5;
}

/// `MMU_INT_*` bits.
mod irq {
    pub(super) const PAGE_FAULT: u32 = 0x01;
    pub(super) const BUS_ERROR: u32 = 0x02;
    pub(super) const MASK: u32 = PAGE_FAULT | BUS_ERROR;
}

/// Bounded poll iterations for a status transition (~1 ms at the documented
/// 100 µs poll period, run as a tight spin because bring-up happens in task
/// context before the device may issue transactions).
const POLL_STEPS: u32 = 1_000;

/// One 4 KiB table page, as a DMA request.
const TABLE_PAGE_LEN: usize = 4096;
const TABLE_PAGE_ALIGN: usize = 4096;

/// DMA-coherent backing for IOMMU table pages.
///
/// Table pages must be readable by the IOMMU walk at a known physical
/// address; the host DMA coherent allocator guarantees both. This adapter
/// plugs it into the OS-neutral [`TableAllocator`] contract.
struct DmaTableAllocator;

impl TableAllocator for DmaTableAllocator {
    fn alloc_zeroed_table_page(&self) -> Option<(*mut u8, u64)> {
        let layout = core::alloc::Layout::from_size_align(TABLE_PAGE_LEN, TABLE_PAGE_ALIGN).ok()?;
        // SAFETY: the layout is a non-zero page-aligned request and the
        // returned buffer stays owned by the page-table backend until freed.
        let info = unsafe { kdma::allocate_dma_memory(layout) }.ok()?;
        Some((info.cpu_addr.as_ptr(), info.bus_addr.as_u64()))
    }

    /// # Safety
    ///
    /// `(ptr, bus)` must be a live allocation previously returned by this
    /// allocator and must not be referenced by any page table.
    unsafe fn free_table_page(&self, ptr: *mut u8, bus: u64) {
        let Ok(layout) = core::alloc::Layout::from_size_align(TABLE_PAGE_LEN, TABLE_PAGE_ALIGN)
        else {
            return;
        };
        let Some(cpu_addr) = NonNull::new(ptr) else {
            return;
        };
        let info = kdma::DmaInfo {
            cpu_addr,
            bus_addr: kdma::DmaBusAddress::new(bus),
        };
        // SAFETY: the caller's contract guarantees single ownership.
        unsafe { kdma::deallocate_dma_memory(info, layout) };
    }
}

/// One MMU register interface.
struct MmuBase {
    ptr: NonNull<u8>,
}

// SAFETY: the pointer is only dereferenced through the register constants;
// volatile accesses are serialized by the domain lock and the IRQ context.
unsafe impl Send for MmuBase {}
// SAFETY: register accesses are volatile and serialized by the domain lock
// plus the IRQ-context discipline, so shared references are sound.
unsafe impl Sync for MmuBase {}

impl MmuBase {
    /// # Safety
    ///
    /// `ptr` must be the mapped base of one MMU register window that stays
    /// mapped for this object's lifetime.
    unsafe fn new(ptr: NonNull<u8>) -> Self {
        Self { ptr }
    }

    fn read(&self, offset: usize) -> u32 {
        // SAFETY: offset is one of the module's register constants, inside
        // the window mapped at probe.
        let slot = unsafe { self.ptr.add(offset).cast::<u32>() };
        // SAFETY: `slot` addresses a live register of the mapped window.
        unsafe { slot.read_volatile() }
    }

    fn write(&self, offset: usize, value: u32) {
        // SAFETY: offset is one of the module's register constants, inside
        // the window mapped at probe.
        let slot = unsafe { self.ptr.add(offset).cast::<u32>() };
        // SAFETY: `slot` addresses a live register of the mapped window.
        unsafe { slot.write_volatile(value) }
    }

    fn command(&self, value: u32) {
        self.write(reg::COMMAND, value);
    }
}

/// Hardware state shared by the fault handler and the [`IommuHardware`] ops.
pub struct RkIommuHw {
    instance: InstanceId,
    bases: Arc<[MmuBase]>,
}

impl RkIommuHw {
    fn is_stall_active(&self) -> bool {
        self.bases
            .iter()
            .all(|base| base.read(reg::STATUS) & status::STALL_ACTIVE != 0)
    }

    fn is_paging_enabled(&self) -> bool {
        self.bases
            .iter()
            .all(|base| base.read(reg::STATUS) & status::PAGING_ENABLED != 0)
    }

    /// Polls `want` for [`POLL_STEPS`] spin iterations.
    fn poll(&self, want: impl Fn(&Self) -> bool) -> bool {
        for _ in 0..POLL_STEPS {
            if want(self) {
                return true;
            }
            core::hint::spin_loop();
        }
        false
    }
}

impl IommuHardware for RkIommuHw {
    /// Points every MMU interface at `domain`'s directory table and starts
    /// translation.
    ///
    /// A Rockchip IOMMU serves exactly one master (its media IP), so the
    /// master cells carry no stream identity; the argument exists for
    /// symmetry with the SMMU binding contract.
    fn attach(&self, _master: &[u32], domain: &IommuDomain) -> Result<(), IommuError> {
        let root = domain.root_table_address();
        let root_low = u32::try_from(root & 0xffff_ffff).map_err(|_| IommuError::HardwareBusy)?;
        for base in self.bases.iter() {
            base.write(reg::DTE_ADDR, root_low);
            // The hardware must latch the directory address; a read-back
            // mismatch means the interface is not responding.
            if base.read(reg::DTE_ADDR) != root_low {
                return Err(IommuError::HardwareBusy);
            }
            base.command(command::ZAP_CACHE);
            base.command(command::ENABLE_PAGING);
            if !self.poll(Self::is_paging_enabled) {
                log::error!(
                    "rk-iommu: enable paging timed out (instance {instance:?})",
                    instance = self.instance
                );
                return Err(IommuError::HardwareBusy);
            }
        }
        Ok(())
    }

    fn detach(&self, _master: &[u32]) -> Result<(), IommuError> {
        for base in self.bases.iter() {
            base.command(command::DISABLE_PAGING);
            if !self.poll(|hardware| !hardware.is_paging_enabled()) {
                log::warn!(
                    "rk-iommu: disable paging timed out (instance {instance:?})",
                    instance = self.instance
                );
            }
            base.write(reg::DTE_ADDR, 0);
        }
        Ok(())
    }

    /// Shoots down the entire IOTLB: stall, zap, unstall, confirming each
    /// step against `MMU_STATUS` with a bounded poll.
    fn flush_iotlb_all(&self) {
        if !self.is_paging_enabled() {
            return;
        }
        for base in self.bases.iter() {
            if !self.is_stall_active() {
                base.command(command::ENABLE_STALL);
                if !self.poll(Self::is_stall_active) {
                    log::warn!("rk-iommu: enable stall timed out; zapping anyway");
                }
            }
            base.command(command::ZAP_CACHE);
            base.command(command::DISABLE_STALL);
            if !self.poll(|hardware| !hardware.is_stall_active()) {
                log::warn!("rk-iommu: disable stall timed out");
            }
        }
    }
}

/// Fault interrupt handler: report observable state, then clear status and
/// terminate the stalled transaction to stop the interrupt storm.
struct RkIommuFaultHandler {
    hardware: Arc<RkIommuHw>,
}

impl DevIrqHandler for RkIommuFaultHandler {
    fn handle(&self, _irq: usize) -> IrqEvent {
        let hardware = &*self.hardware;
        let mut claimed = false;
        for (index, base) in hardware.bases.iter().enumerate() {
            let int_status = base.read(reg::INT_STATUS);
            if int_status == 0 {
                continue;
            }
            claimed = true;
            let iova = base.read(reg::PAGE_FAULT_ADDR);
            if int_status & irq::PAGE_FAULT != 0 {
                let is_write = base.read(reg::STATUS) & status::PAGE_FAULT_IS_WRITE != 0;
                // Report through the shared core format first: the identity
                // line must print even if later cleanup wedges.
                kiommu::report_fault(&IommuFault {
                    instance: hardware.instance,
                    master: index as u32,
                    iova: kiommu::Iova::new(u64::from(iova)),
                    is_write,
                });
            }
            if int_status & irq::BUS_ERROR != 0 {
                log::error!("rk-iommu: bus error on interface {index} at iova {iova:#x}");
            }
            // Close the loop: acknowledge the interrupt, zap the poisoned
            // IOTLB entry and terminate the replay so the master resumes.
            base.write(reg::INT_CLEAR, int_status & irq::MASK);
            base.command(command::ZAP_CACHE);
            base.command(command::PAGE_FAULT_DONE);
            if int_status & !irq::MASK != 0 {
                log::error!(
                    "rk-iommu: unhandled interrupt bits {:#x} on interface {index}",
                    int_status & !irq::MASK
                );
                base.write(reg::INT_CLEAR, int_status & !irq::MASK);
            }
        }
        if claimed {
            IrqEvent::HANDLED
        } else {
            IrqEvent::NOT_HANDLED
        }
    }
}

/// Device-driver shell: discovers one `rockchip,*-iommu` node and owns the
/// probe-time resources (MMIO mapping, fault IRQ, domain, registration).
struct RkIommuDriver;

impl kdevice::DeviceDriver for RkIommuDriver {
    type Data = Arc<RkIommuHw>;

    fn name(&self) -> &'static str {
        "rk-iommu"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Bus
    }

    fn bus_types(&self) -> &'static [kdevice::BusTypeId] {
        &[kdevice::BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn kdevice::DeviceMatcher {
        static SPEC: kdevice::FirmwareMatchSpec = kdevice::FirmwareMatchSpec {
            alias: "rk-iommu",
            dt_compatibles: &[
                "rockchip,rk3588-iommu",
                "rockchip,rk3568-iommu",
                "rockchip,rk3288-iommu",
                "rockchip,iommu",
            ],
            acpi_ids: &[],
        };
        &SPEC
    }

    fn probe_device(&self, ctx: &kdevice::ProbeContext<'_>) -> DriverResult<Self::Data> {
        let device = ctx.device();
        let Some(node) = device.firmware_node() else {
            return Err(DriverError::InvalidInput);
        };
        let instance = InstanceId(u64::from(node.id().raw()));

        let Some(mmio) = device.first_mmio() else {
            return Err(DriverError::InvalidInput);
        };
        let mmio_ptr =
            kdevice::device::managed::devm_iomap_with_provider(&PROVIDER, ctx, mmio, "rk-iommu")
                .map_err(map_res_err)?;
        // SAFETY: the mapping lives as long as the binding; the hardware
        // guard stores only the raw base.
        let bases: Arc<[MmuBase]> = Arc::from(alloc::vec![unsafe { MmuBase::new(mmio_ptr) }]);

        let hardware = Arc::new(RkIommuHw { instance, bases });

        // Fault interrupt: optional in device trees but required for the
        // observable fault path; a node without one stays usable (its faults
        // surface as bus hangs, not storms).
        if let Some(irq) = device.first_irq() {
            let handler: Arc<dyn DevIrqHandler> = Arc::new(RkIommuFaultHandler {
                hardware: hardware.clone(),
            });
            kdevice::device::managed::devm_request_irq_with_provider(&PROVIDER, ctx, irq, handler)
                .map_err(map_res_err)?;
        }

        // Build the instance domain: the hardware's full 4 GiB IOVA reach
        // over a v2 two-level table.
        let page_table =
            RockchipV2::new(Arc::new(DmaTableAllocator)).map_err(|_| DriverError::NoMemory)?;
        let domain = Arc::new(IommuDomain::new(Box::new(page_table), 1 << 32, 4096));
        let registered = kiommu::register_instance(instance, hardware.clone(), domain);
        log::info!(
            "rk-iommu: instance {instance:?} registered (domain root {:#x})",
            registered.root_table_address()
        );
        Ok(hardware)
    }

    fn remove(
        &self,
        _ctx: &kdevice::RemoveContext<'_>,
        data: Option<&mut Self::Data>,
    ) -> DriverResult<()> {
        // Disable translation on every interface before the mappings go
        // away; the domain itself stays in the kiommu registry.
        if let Some(data) = data {
            let _ = data.detach(&[]);
        }
        Ok(())
    }
}

fn map_res_err(error: ResError) -> DriverError {
    match error {
        ResError::InvalidResource => DriverError::InvalidInput,
        ResError::MappingFailed
        | ResError::Io
        | ResError::BadState
        | ResError::NotFound
        | ResError::Interrupted => DriverError::Io,
        ResError::NoMemory => DriverError::NoMemory,
        ResError::Busy | ResError::AlreadyExists => DriverError::ResourceBusy,
        ResError::Unsupported => DriverError::Unsupported,
    }
}

/// Constructs the driver descriptor registered by the kdriver facade.
pub fn driver_descriptor() -> kdevice::BoxedDriver {
    kdevice::BoxedDriver::new(RkIommuDriver)
}
