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

use alloc::{boxed::Box, sync::Arc, vec::Vec};
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

/// Dummy value written to DTE_ADDR at probe and read back: the v2 register
/// file latches address bits [39:4], so this value survives the round trip
/// when the window answers (Linux uses the same shape in its force_reset
/// path with the v2 `pt_address` encoding).
const DTE_ADDR_PROBE: u32 = 0xCAFE_BAB0 & 0xffff_fff0;

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

/// RK3588 CRU gate programming for the clocks one IOMMU instance needs.
///
/// Transcribed from the reference clock driver (see the reference-anchor
/// entry): `CLKGATE_CON(x)` lives at `CRU + 0x800 + 4 * x`, gate bit set is
/// clock *stopped*, and every write needs the hiword mask for the touched
/// bit. Clearing the bit ungates.
/// One gate position: `CLKGATE_CON(reg)` bit `bit`, in either the main CRU
/// window or the PMU-CRU window (the lookup table decides which).
#[derive(Clone, Copy)]
struct Gate {
    reg: usize,
    bit: u32,
}

const GATE_OFFSET: usize = 0x800;

/// Ungates one clock: clears the gate bit with a hiword-masked write.
///
/// # Safety
///
/// `cru` must be the mapped gate window the `Gate` names.
unsafe fn ungate(cru: *mut u32, gate: Gate) {
    // SAFETY: `add` stays inside the mapped window.
    let addr = unsafe { cru.add(GATE_OFFSET / 4 + gate.reg) };
    // SAFETY: `addr` addresses a live gate register; the hiword mask
    // confines the write to the one gate bit.
    unsafe {
        addr.write_volatile(1 << (gate.bit + 16));
    }
}

mod cru {
    use crate::Gate;

    /// RK3588 CRU base address (clock-controller@fd7c0000).
    pub(super) const CRU_BASE: usize = 0xfd7c_0000;

    /// DT clock-id -> gate position. Only the ids the board DT declares on
    /// IOMMU nodes are tabled; an unknown id is a probe error, not a guess.
    pub(super) const fn gate_for_clock_id(id: u32) -> Option<Gate> {
        match id {
            // NPU clock tree (the MMU's declared parents):
            289 => Some(Gate { reg: 29, bit: 0 }), // HCLK_NPU_ROOT
            290 => Some(Gate { reg: 29, bit: 1 }), // CLK_NPU_DSU0
            291 => Some(Gate { reg: 29, bit: 4 }), // PCLK_NPU_ROOT
            292 => Some(Gate { reg: 29, bit: 6 }), // PCLK_NPU_TIMER
            293 => Some(Gate { reg: 29, bit: 7 }), // CLK_NPUTIMER_ROOT
            // The MMU bus interfaces themselves (what the walk needs):
            287 => Some(Gate { reg: 30, bit: 6 }), // ACLK_NPU0
            288 => Some(Gate { reg: 30, bit: 8 }), // HCLK_NPU0
            // Declared (board DT quirk): eMMC bus/timer clocks.
            301 => Some(Gate { reg: 31, bit: 7 }), // BCLK_EMMC
            302 => Some(Gate { reg: 31, bit: 8 }), // TMCLK_EMMC
            _ => None,
        }
    }
}

/// RK3588 PMU-CRU gate table for the PMU-island MMU.
///
/// The PMU sub-controller's gates live in a separate window:
/// `PMU_CLKGATE_CON(x)` at `PMU_CRU_BASE + 0x800 + 4 * x` with
/// `PMU_CRU_BASE = 0xfd7c8000`.
mod pmu_cru {
    use crate::Gate;

    /// The PMU-CRU is an offset window inside the main CRU mapping
    /// (`RK3588_PMU_CRU_BASE = 0x30000`).
    pub(super) const PMU_CRU_BASE: usize = 0x3_0000;

    pub(super) const fn gate_for_clock_id(id: u32) -> Option<Gate> {
        match id {
            623 => Some(Gate { reg: 5, bit: 2 }), // PCLK_PMU0
            624 => Some(Gate { reg: 5, bit: 4 }), // PCLK_PMU0IOC
            _ => None,
        }
    }
}

/// RK3588 PMU power-domain programming for the NPU top island.
///
/// The board DTB ships the NPU MMU without its `power-domains` property, but
/// the hardware island state is unchanged: with PD_NPUTOP off the register
/// file does not answer and the first read aborts the CPU. The sequence below
/// is transcribed from the reference power-domain driver (see the
/// reference-anchor entry); PMU base is the `rockchip,rk3588-pmu` syscon at
/// `0xfd8d8000`, all writes hiword-masked.
mod pmu {
    pub(super) const PMU_BASE: usize = 0xfd8d_8000;

    // RK3588 PMU info offsets.
    const PWR_OFFSET: usize = 0x14c;
    const REQ_OFFSET: usize = 0x10c;
    const MEM_PWR_OFFSET: usize = 0x1a0;
    const ACK_OFFSET: usize = 0x118;
    const IDLE_OFFSET: usize = 0x120;
    const MEM_STATUS_OFFSET: usize = 0x1f8;
    const CHAIN_STATUS_OFFSET: usize = 0x1f0;
    const REPAIR_STATUS_OFFSET: usize = 0x290;

    // RK3588_PD_NPUTOP (id 9).
    const PWR_BIT: u32 = 1 << 3;
    const REQ_BIT: u32 = 1 << 1;
    const MEM_STATUS_BIT: u32 = 1 << 11;
    const REPAIR_STATUS_BIT: u32 = 1 << 2;

    /// Bounded poll iterations for PMU handshakes.
    const POLL_STEPS: u32 = 100_000;

    fn read(cru: *const u32, offset: usize) -> u32 {
        // SAFETY: offset is one of the module's register constants inside
        // the mapped PMU window.
        let slot = unsafe { cru.add(offset / 4) };
        // SAFETY: `slot` addresses a live PMU register.
        unsafe { slot.read_volatile() }
    }

    /// Writes `value` with the hiword mask for `mask` (Rockchip convention:
    /// high half enables the write of the low-half bits).
    fn write_masked(pmu: *mut u32, offset: usize, mask: u32, value: u32) {
        // SAFETY: offset is one of the module's register constants inside
        // the mapped PMU window.
        let slot = unsafe { pmu.add(offset / 4) };
        // SAFETY: `slot` addresses a live PMU register.
        unsafe { slot.write_volatile((mask << 16) | (value & mask)) }
    }

    fn poll(cru: *const u32, offset: usize, ready: impl Fn(u32) -> bool) -> bool {
        for _ in 0..POLL_STEPS {
            if ready(read(cru, offset)) {
                return true;
            }
            core::hint::spin_loop();
        }
        false
    }

    fn is_on(pmu: *const u32) -> bool {
        // NPUTOP carries a repair-status bit: 1 = powered.
        read(pmu, REPAIR_STATUS_OFFSET) & REPAIR_STATUS_BIT != 0
    }

    fn is_mem_on(pmu: *const u32) -> bool {
        read(pmu, MEM_STATUS_OFFSET) & MEM_STATUS_BIT == 0
    }

    /// Powers PD_NPUTOP on (no-op when the repair status already reports on).
    ///
    /// # Safety
    ///
    /// `pmu` must be the mapped RK3588 PMU window; callers must hold no
    /// expectation of PMU state across other cores (probe runs boot-time
    /// single-task before any PM user exists).
    pub(super) unsafe fn power_nputop_on(pmu: *mut u32) -> bool {
        if is_on(pmu) {
            return true;
        }

        // Power on: clear the pwr bit with a hiword-masked write.
        write_masked(pmu, PWR_OFFSET, PWR_BIT, 0);
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);

        // NPUTOP owns memory repair: when the memory array reports on, swing
        // it through an off->on reset while the chain status is stable.
        if is_mem_on(pmu) {
            let mut chain_on = false;
            for _ in 0..POLL_STEPS {
                // SAFETY: chain status lives inside the PMU window.
                if read(pmu, CHAIN_STATUS_OFFSET) & MEM_STATUS_BIT == 0 {
                    chain_on = true;
                    break;
                }
                core::hint::spin_loop();
            }
            if !chain_on {
                return false;
            }
            // Delay, drive the memory array through off, then back on.
            for _ in 0..20_000 {
                core::hint::spin_loop();
            }
            write_masked(pmu, MEM_PWR_OFFSET, PWR_BIT, PWR_BIT);
            core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
            if !poll(pmu, MEM_STATUS_OFFSET, |v| v & MEM_STATUS_BIT != 0) {
                return false;
            }
            write_masked(pmu, MEM_PWR_OFFSET, PWR_BIT, 0);
            core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
            if !poll(pmu, MEM_STATUS_OFFSET, |v| v & MEM_STATUS_BIT == 0) {
                return false;
            }
        }

        if !poll(pmu, REPAIR_STATUS_OFFSET, |v| v & REPAIR_STATUS_BIT != 0) {
            return false;
        }

        // Leave idle mode: clear the NIU request, wait ack clear, wait the
        // domain reports not idle.
        write_masked(pmu, REQ_OFFSET, REQ_BIT, 0);
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        if !poll(pmu, ACK_OFFSET, |v| v & REQ_BIT == 0) {
            return false;
        }
        poll(pmu, IDLE_OFFSET, |v| v & REQ_BIT == 0)
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
                "rockchip,iommu-v2",
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

        log::warn!("rk-iommu: probing instance {instance:?}");
        // An instance inside a power domain is unreachable until a PM-domain
        // driver powers the island on: its register file does not answer and
        // the first read would take a bus abort. Skip it explicitly instead
        // of panicking the boot.
        if matches!(node.property("power-domains"), Ok(Some(_))) {
            log::info!(
                "rk-iommu: instance {instance:?} declares power-domains; skipping until a                  power-domain driver exists"
            );
            return Err(DriverError::Unsupported);
        }

        // One node can expose several identical MMU interfaces (the NPU MMU
        // declares four `reg` windows sharing its fault IRQs): map them all.
        let mut bases: Vec<MmuBase> = Vec::new();
        for resource in device.resources().iter() {
            let mmio @ kdevice::MmioRegion { .. } = match resource {
                kdevice::ResourceDesc::Mmio(mmio) => *mmio,
                _ => continue,
            };
            let ptr = kdevice::device::managed::devm_iomap_with_provider(
                &PROVIDER, ctx, mmio, "rk-iommu",
            )
            .map_err(map_res_err)?;
            // SAFETY: the mapping lives as long as the binding; the hardware
            // guard stores only the raw base.
            bases.push(unsafe { MmuBase::new(ptr) });
        }
        if bases.is_empty() {
            return Err(DriverError::InvalidInput);
        }
        let bases: Arc<[MmuBase]> = Arc::from(bases);

        let hardware = Arc::new(RkIommuHw {
            instance,
            bases: bases.clone(),
        });

        // Power the NPU top island before touching any register: the board
        // DTB ships the NPU MMU without its power-domains property, but the
        // island state is unchanged and a powered-down register file aborts
        // on first read.
        let pmu = memspace::iomap_device(
            memaddr::PhysAddr::from(pmu::PMU_BASE),
            0x1000,
            "rk-iommu-pmu",
        )
        .map_err(|_| DriverError::Io)?
        .as_mut_ptr()
        .cast::<u32>();
        // SAFETY: `pmu` is the mapped PMU window; probe runs single-task at
        // boot with no concurrent PM user.
        if unsafe { pmu::power_nputop_on(pmu) } {
            log::warn!("rk-iommu: PD_NPUTOP powered on");
        } else {
            log::error!("rk-iommu: PD_NPUTOP power-on handshake failed; refusing to probe blind");
            return Err(DriverError::Io);
        }

        // Ungate the CRU clocks the instance declares (plus the MMU bus
        // gates): a gated instance does not answer MMIO and the first read
        // would abort the CPU. The DT `clocks` list is <&cru id> pairs; an
        // unknown id fails the probe loudly.
        match node.property("clocks") {
            Ok(Some(property)) => {
                let declared = clock_ids(&property.value).ok_or(DriverError::InvalidInput)?;
                if declared.is_empty() {
                    log::warn!("rk-iommu: instance {instance:?} declares no clocks");
                }
                let mut ids = declared.clone();
                // The board DTB declares only the NPU clock *tree* inputs;
                // the MMU instance's own bus gates (ACLK_NPU0/HCLK_NPU0) are
                // what the register file answers on -- upstream wires exactly
                // these two as the node's clocks. Ungate them with the
                // declared set.
                if ids.iter().any(|id| cru::gate_for_clock_id(*id).is_some()) {
                    for required in [287, 288] {
                        if !ids.contains(&required) {
                            ids.push(required);
                        }
                    }
                }
                // SAFETY: the CRU window is mapped below and both gate
                // tables index inside it.
                let cru = memspace::iomap_device(
                    memaddr::PhysAddr::from(cru::CRU_BASE),
                    0x40000,
                    "rk-iommu-cru",
                )
                .map_err(|_| DriverError::Io)?
                .as_mut_ptr()
                .cast::<u32>();
                for id in &ids {
                    // The PMU-island MMU's clocks gate through the PMU-CRU
                    // sub-window, not the main CRU.
                    let (gate, window) = if let Some(gate) = pmu_cru::gate_for_clock_id(*id) {
                        (gate, unsafe { cru.add(pmu_cru::PMU_CRU_BASE / 4) })
                    } else if let Some(gate) = cru::gate_for_clock_id(*id) {
                        (gate, cru)
                    } else {
                        log::error!(
                            "rk-iommu: instance {instance:?} declares clock id {id} with no gate \
                             table entry; refusing to probe blind"
                        );
                        return Err(DriverError::Unsupported);
                    };
                    // SAFETY: `window` is inside the mapped CRU region and
                    // the gate entry indexes within the sub-window.
                    unsafe { ungate(window, gate) };
                }
                log::warn!(
                    "rk-iommu: unguarded {} CRU gate(s) for instance {instance:?}",
                    ids.len()
                );
            }
            Ok(None) => {
                log::warn!("rk-iommu: instance {instance:?} declares no clocks property");
            }
            Err(error) => {
                log::warn!("rk-iommu: clocks property unreadable: {error:?}");
            }
        }

        // Probe criterion (the Linux force_reset check): write a dummy value
        // to DTE_ADDR and read it back. A mismatch means the register file is
        // not answering -- typically the aclk/iface clocks are still gated in
        // the CRU. The instance stays unregistered in that case so the DMA
        // layer never trusts it.
        let probe_base = &hardware.bases[0];
        probe_base.write(reg::DTE_ADDR, DTE_ADDR_PROBE);
        let readback = probe_base.read(reg::DTE_ADDR);
        if readback != DTE_ADDR_PROBE {
            log::error!(
                "rk-iommu: DTE_ADDR read-back failed (wrote {DTE_ADDR_PROBE:#x}, read                  {readback:#x}) -- MMIO window not answering; check CRU aclk/iface gates"
            );
            return Err(DriverError::Io);
        }
        log::warn!("rk-iommu: DTE_ADDR read-back ok ({readback:#x}) -- register file answering");

        // Fault interrupts: a node may declare one IRQ per MMU interface; the
        // handler iterates every base per interrupt, so sharing covers all of
        // them. A node without any IRQ stays usable (its faults surface as
        // bus hangs, not storms).
        for resource in device.resources().iter() {
            let kdevice::ResourceDesc::Irq(irq) = resource else {
                continue;
            };
            let handler: Arc<dyn DevIrqHandler> = Arc::new(RkIommuFaultHandler {
                hardware: hardware.clone(),
            });
            kdevice::device::managed::devm_request_irq_with_provider(&PROVIDER, ctx, *irq, handler)
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

/// Extracts the clock-id cells from a DT `clocks` property
/// (`<&cru id>...` pairs; the phandle half is dropped).
fn clock_ids(value: &fwnode::FwPropertyValue<'_>) -> Option<Vec<u32>> {
    match value {
        fwnode::FwPropertyValue::Bytes(bytes) => {
            if !bytes.len().is_multiple_of(4) {
                return None;
            }
            Some(
                bytes
                    .chunks_exact(4)
                    .enumerate()
                    .filter(|(index, _)| index % 2 == 1)
                    .map(|(_, cell)| u32::from_be_bytes([cell[0], cell[1], cell[2], cell[3]]))
                    .collect(),
            )
        }
        fwnode::FwPropertyValue::U32(id) => Some(alloc::vec![*id]),
        _ => None,
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
