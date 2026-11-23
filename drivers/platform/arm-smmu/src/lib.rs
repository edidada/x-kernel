// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ARM SMMUv2 (`arm,mmu-500`) driver.
//!
//! The probe reads the ID registers, resets the SMMU into a known state and
//! publishes the instance to the [`kiommu`] core. Device attachment allocates
//! one Stream Match Register (SMR) plus Stream-to-Context Register (S2CR)
//! pair per master and programs one context bank per (instance, domain) pair
//! from the domain's long-descriptor page table.
//!
//! # Register model
//!
//! SMMUv2 splits its register file into three pages: GR0 (global,
//! configuration), GR1 (global context-bank attributes) and one page per
//! context bank. All offsets follow the SMMUv2 architecture specification;
//! the page stride comes from `IDR1.PAGESIZE` (4 KiB or 64 KiB).
//!
//! # TLB maintenance
//!
//! Context invalidation writes `CB_TLBIALL` and then spins on
//! `CB_TLBSYNC`/`CB_TLBSTATUS.GSACTIVE`; global invalidation uses
//! `TLBIALLNSNH` plus `sTLBGSYNC`/`sTLBGSTATUS`. Unmaps must complete the
//! sync before their IOVA range is reused — [`kiommu::IommuDomain`] calls
//! [`IommuHardware::flush_iotlb_all`] exactly at that point.

#![no_std]

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicU32, Ordering},
};

use driver_base::{DeviceKind, DriverError, DriverResult};
use driver_os_api::{IrqEvent, IrqHandler as DevIrqHandler, ResError};
use driver_os_api_xkernel::XKernelResourceProvider;
use io_pgtable::{ArmLpae, ArmLpaeConfig, TableAllocator};
use kiommu::{InstanceId, IommuDomain, IommuError, IommuFault, IommuHardware};
use kspin::SpinNoIrq;

static PROVIDER: XKernelResourceProvider = XKernelResourceProvider::new();

/// GR0 offsets.
mod gr0 {
    pub(super) const S_CR0: usize = 0x000;
    pub(super) const ID0: usize = 0x020;
    pub(super) const ID1: usize = 0x024;
    pub(super) const ID2: usize = 0x028;
    pub(super) const S_GFSR: usize = 0x048;
    pub(super) const S_GFSYNR0: usize = 0x050;
    pub(super) const S_GFSYNR1: usize = 0x054;
    pub(super) const S_GFAR: usize = 0x06c;
    pub(super) const TLBIALLNSNH: usize = 0x068;
    pub(super) const S_TLBGSYNC: usize = 0x070;
    pub(super) const S_TLBGSTATUS: usize = 0x074;
    /// Stream match registers, one per stream.
    pub(super) const SMR_BASE: usize = 0x800;
    /// Stream-to-context registers, one per stream.
    pub(super) const S2CR_BASE: usize = 0xc00;
}

/// GR1 offsets.
mod gr1 {
    pub(super) const CBAR_BASE: usize = 0x000;
    pub(super) const CBA2R_BASE: usize = 0x800;
}

/// Context-bank page offsets.
mod cb {
    pub(super) const SCTLR: usize = 0x000;
    pub(super) const RESUME: usize = 0x008;
    pub(super) const TTBR0: usize = 0x020;
    pub(super) const TCR: usize = 0x030;
    pub(super) const S1_MAIR0: usize = 0x038;
    pub(super) const FSR: usize = 0x058;
    pub(super) const FAR: usize = 0x060;
    pub(super) const FSYNR0: usize = 0x068;
    pub(super) const S1_TLBIALL: usize = 0x218;
    pub(super) const TLBSYNC: usize = 0x7f0;
    pub(super) const TLBSTATUS: usize = 0x7f4;
}

/// `sCR0` bits.
mod cr0 {
    pub(super) const USFCFG: u32 = 1 << 10;
    pub(super) const GCFGFIE: u32 = 1 << 5;
    pub(super) const GCFGFRE: u32 = 1 << 4;
    pub(super) const GFIE: u32 = 1 << 2;
    pub(super) const GFRE: u32 = 1 << 1;
    pub(super) const CLIENTPD: u32 = 1 << 0;
}

/// `IDR0` bits used by the probe.
mod idr0 {
    pub(super) const S1TS: u32 = 1 << 30;
    pub(super) const CTTW: u32 = 1 << 14;
    pub(super) const NUMSMRG: u32 = 0xff;
}

/// `IDR1` bits used by the probe.
mod idr1 {
    pub(super) const PAGESIZE: u32 = 1 << 31;
    pub(super) const NUMPAGENDXB: u32 = 0x7 << 28;
    pub(super) const NUMCB: u32 = 0xff;

    /// Reads IDR1 through the fixed GR0 offset (valid before the page
    /// stride is known).
    ///
    /// # Safety
    ///
    /// `base` must be the mapped GR0 base of an SMMU register region.
    pub(super) unsafe fn read(base: core::ptr::NonNull<u8>) -> u32 {
        // SAFETY: the caller passes the validated GR0 base.
        let slot = unsafe { base.add(super::gr0::ID1).cast::<u32>() };
        // SAFETY: `slot` addresses the live IDR1 register.
        unsafe { slot.read_volatile() }
    }
}

/// `S2CR` / `SMR` fields.
mod stream {
    pub(super) const SMR_VALID: u32 = 1 << 31;
    pub(super) const SMR_ID: u32 = 0xffff;
    pub(super) const S2CR_TYPE_TRANS: u32 = 0b10 << 16;
    pub(super) const S2CR_CBNDX: u32 = 0xff;
}

/// Context-bank attribute fields.
mod cbar {
    /// Stage 1 translation with stage 2 bypass.
    pub(super) const TYPE_S1_BYPASS: u32 = 0b01 << 16;
    pub(super) const VMID: u32 = 0xff;
}

mod cba2r {
    pub(super) const VA64: u32 = 1;
}

/// `CB_SCTLR` bits.
mod sctlr {
    pub(super) const HUPCF: u32 = 1 << 8;
    pub(super) const CFIE: u32 = 1 << 6;
    pub(super) const CFRE: u32 = 1 << 5;
    pub(super) const M: u32 = 1;
}

/// `CB_FSR` bits.
mod fsr {
    pub(super) const MULTI: u32 = 1 << 31;
    pub(super) const SS: u32 = 1 << 30;
    pub(super) const UUT: u32 = 1 << 8;
    pub(super) const ASF: u32 = 1 << 7;
    pub(super) const TLBLKF: u32 = 1 << 6;
    pub(super) const TLBMCF: u32 = 1 << 5;
    pub(super) const EF: u32 = 1 << 4;
    pub(super) const PF: u32 = 1 << 3;
    pub(super) const AFF: u32 = 1 << 2;
    pub(super) const TF: u32 = 1 << 1;
    /// Ignorable fault classes cleared without logging.
    pub(super) const IGN: u32 = AFF | ASF | TLBMCF | TLBLKF;
    /// Every class this handler reports and clears.
    pub(super) const FAULT: u32 = MULTI | SS | UUT | EF | PF | TF | IGN;
}

/// Bounded poll iterations for TLB sync and reset completion.
const POLL_STEPS: u32 = 1_000_000;
/// Upper bound on context banks this driver manages per SMMU.
const MAX_CBS: usize = 128;

/// DMA-coherent backing for SMMU page tables (same contract as the
/// Rockchip backend's allocator).
struct DmaTableAllocator;

impl TableAllocator for DmaTableAllocator {
    fn alloc_zeroed_table_page(&self) -> Option<(*mut u8, u64)> {
        let layout = core::alloc::Layout::from_size_align(4096, 4096).ok()?;
        // SAFETY: non-zero page-aligned layout; the backend owns the buffer.
        let info = unsafe { kdma::allocate_dma_memory(layout) }.ok()?;
        Some((info.cpu_addr.as_ptr(), info.bus_addr.as_u64()))
    }

    /// # Safety
    ///
    /// `(ptr, bus)` must be a live allocation from this allocator.
    unsafe fn free_table_page(&self, ptr: *mut u8, bus: u64) {
        let Ok(layout) = core::alloc::Layout::from_size_align(4096, 4096) else {
            return;
        };
        let Some(cpu_addr) = NonNull::new(ptr) else {
            return;
        };
        let info = kdma::DmaInfo {
            cpu_addr,
            bus_addr: kdma::DmaBusAddress::new(bus),
        };
        // SAFETY: single ownership per the contract.
        unsafe { kdma::deallocate_dma_memory(info, layout) };
    }
}

/// One mapped SMMU register file.
struct SmmuRegs {
    gr0: NonNull<u8>,
    /// GR1 base, `pgshift` above GR0.
    gr1: NonNull<u8>,
    /// Context-bank region base.
    cb_base: NonNull<u8>,
    pgshift: u32,
}

// SAFETY: the register file is exclusively owned by the driver; all accesses
// are volatile and serialized through the `SmmuDevice` lock discipline.
unsafe impl Send for SmmuRegs {}
// SAFETY: register accesses are volatile; cross-core ordering is provided by
// the driver's own TLB-sync barriers, never by shared concurrent writes.
unsafe impl Sync for SmmuRegs {}

impl SmmuRegs {
    /// Reads the ID registers, validates the mapping and derives the page
    /// layout from `IDR1`.
    ///
    /// # Safety
    ///
    /// `base` must be the mapped SMMU register region of `size` bytes that
    /// stays mapped for this object's lifetime.
    unsafe fn probe(base: NonNull<u8>, size: usize) -> Result<Self, DriverError> {
        // ID registers live at fixed GR0 offsets regardless of the stride.
        // SAFETY: the caller's contract guarantees the GR0 page is mapped.
        let idr_slot = unsafe { base.add(gr0::ID1).cast::<u32>() };
        // SAFETY: `idr_slot` addresses the live IDR1 register.
        let idr1 = unsafe { idr_slot.read_volatile() };
        let pgshift = if idr1 & idr1::PAGESIZE != 0 { 16 } else { 12 };
        let stride = 1usize << pgshift;
        let num_cb = (idr1 & idr1::NUMCB) as usize;
        let page_count = 1usize << ((idr1 & idr1::NUMPAGENDXB) >> 28);
        let required = (2 + num_cb) << pgshift;
        if page_count << pgshift < required || size < required {
            log::error!("arm-smmu: register window too small: need {required:#x}, have {size:#x}");
            return Err(DriverError::InvalidInput);
        }
        // SAFETY: `required` fits inside the mapped `size`.
        let gr1 = unsafe { base.add(stride) };
        // SAFETY: `required` covers two configuration pages plus every bank.
        let cb_base = unsafe { base.add(stride * 2) };
        Ok(Self {
            gr0: base,
            gr1,
            cb_base,
            pgshift,
        })
    }

    fn read(&self, page: NonNull<u8>, offset: usize) -> u32 {
        // SAFETY: the caller passes only page bases returned by `probe`.
        let slot = unsafe { page.add(offset).cast::<u32>() };
        // SAFETY: `slot` addresses a live register of the mapped window.
        unsafe { slot.read_volatile() }
    }

    fn write(&self, page: NonNull<u8>, offset: usize, value: u32) {
        // SAFETY: the caller passes only page bases returned by `probe`.
        let slot = unsafe { page.add(offset).cast::<u32>() };
        // SAFETY: `slot` addresses a live register of the mapped window.
        unsafe { slot.write_volatile(value) }
    }

    fn read_gr0(&self, offset: usize) -> u32 {
        self.read(self.gr0, offset)
    }

    fn write_gr0(&self, offset: usize, value: u32) {
        self.write(self.gr0, offset, value);
    }

    fn write_gr1(&self, offset: usize, value: u32) {
        self.write(self.gr1, offset, value);
    }

    fn write_cb(&self, cbndx: usize, offset: usize, value: u32) {
        // SAFETY: `cbndx` is validated against IDR1.NUMCB before use.
        let page = unsafe { self.cb_base.add(cbndx << self.pgshift) };
        self.write(page, offset, value);
    }

    fn read_cb(&self, cbndx: usize, offset: usize) -> u32 {
        // SAFETY: `cbndx` is validated against IDR1.NUMCB before use.
        let page = unsafe { self.cb_base.add(cbndx << self.pgshift) };
        self.read(page, offset)
    }
}

/// One programmed context bank bound to a domain.
#[derive(Clone, Copy)]
struct ContextBank {
    cbndx: usize,
}

/// SMMUv2 hardware instance.
pub struct ArmSmmuHw {
    instance: InstanceId,
    regs: SmmuRegs,
    num_context_banks: usize,
    num_smr: usize,
    /// Free-stream allocation cursor over the SMR bitmap.
    next_smr: AtomicU32,
    vmid_cursor: AtomicU32,
    /// Context banks this instance has programmed (fault handler scope).
    programmed_banks: SpinNoIrq<Vec<usize>>,
}

impl ArmSmmuHw {
    /// Global TLB invalidation with `sTLBGSYNC` completion polling.
    fn tlb_sync_global(&self) {
        self.regs.write_gr0(gr0::S_TLBGSYNC, 0);
        for _ in 0..POLL_STEPS {
            if self.regs.read_gr0(gr0::S_TLBGSTATUS) & 1 == 0 {
                return;
            }
            core::hint::spin_loop();
        }
        log::warn!("arm-smmu: global TLB sync timed out");
    }

    /// Context TLB invalidation with `TLBSYNC` completion polling.
    fn tlb_sync_context(&self, cbndx: usize) {
        self.regs.write_cb(cbndx, cb::TLBSYNC, 0);
        for _ in 0..POLL_STEPS {
            if self.regs.read_cb(cbndx, cb::TLBSTATUS) & 1 == 0 {
                return;
            }
            core::hint::spin_loop();
        }
        log::warn!("arm-smmu: context TLB sync timed out (cb {cbndx})");
    }

    fn invalidate_context(&self, cbndx: usize) {
        // Ensure PTE writes are visible before the invalidation.
        core::sync::atomic::fence(Ordering::Release);
        self.regs.write_cb(cbndx, cb::S1_TLBIALL, 0);
        self.tlb_sync_context(cbndx);
    }

    /// Programs one context bank for `domain` (stage 1, AArch64 long
    /// descriptors, TTBR1 disabled) and returns its index.
    fn program_context_bank(&self, domain: &IommuDomain) -> Result<ContextBank, IommuError> {
        if self.num_context_banks == 0 {
            return Err(IommuError::HardwareBusy);
        }
        // Allocation policy: context banks are assigned in round-robin order
        // from probe time; each domain programs exactly one bank per SMMU.
        let cbndx =
            self.vmid_cursor.fetch_add(1, Ordering::AcqRel) as usize % self.num_context_banks;
        let vmid = (cbndx + 1) as u32;

        // Attributes are taken from the domain's long-descriptor backend.
        // The backend owns the TCR/MAIR encodings; the SMMU only needs the
        // root address.
        let root = domain.root_table_address();

        self.regs.write_gr1(
            gr1::CBAR_BASE + cbndx * 4,
            cbar::TYPE_S1_BYPASS | (vmid & cbar::VMID),
        );
        self.regs
            .write_gr1(gr1::CBA2R_BASE + cbndx * 4, cba2r::VA64);

        // The stage-1 TCR/MAIR values come from the backend's register
        // surface via the domain's root configuration.
        let (tcr, mair0) = domain.stage1_registers();
        self.regs.write_cb(cbndx, cb::TCR, tcr as u32);
        self.regs.write_cb(cbndx, cb::TTBR0, root as u32);
        self.regs.write_cb(cbndx, cb::S1_MAIR0, mair0 as u32);
        self.regs.write_cb(
            cbndx,
            cb::SCTLR,
            sctlr::M | sctlr::CFIE | sctlr::CFRE | sctlr::HUPCF,
        );
        self.invalidate_context(cbndx);
        let mut banks = self.programmed_banks.lock();
        if !banks.contains(&cbndx) {
            banks.push(cbndx);
        }
        Ok(ContextBank { cbndx })
    }

    /// Allocates one SMR/S2CR pair routing `stream_id` to `cbndx`.
    fn route_stream(&self, stream_id: u32, cbndx: usize) -> Result<(), IommuError> {
        if self.num_smr == 0 {
            return Err(IommuError::HardwareBusy);
        }
        // Find a free SMR (valid bit clear), scanning from the cursor.
        let start = self.next_smr.fetch_add(1, Ordering::AcqRel);
        for step in 0..self.num_smr as u32 {
            let index = (start + step) % self.num_smr as u32;
            let offset = gr0::SMR_BASE + index as usize * 4;
            if self.regs.read_gr0(offset) & stream::SMR_VALID != 0 {
                continue;
            }
            self.regs
                .write_gr0(offset, stream::SMR_VALID | (stream_id & stream::SMR_ID));
            self.regs.write_gr0(
                gr0::S2CR_BASE + index as usize * 4,
                stream::S2CR_TYPE_TRANS | (cbndx as u32 & stream::S2CR_CBNDX),
            );
            return Ok(());
        }
        Err(IommuError::HardwareBusy)
    }

    /// Releases the SMR matching `stream_id`.
    fn unroute_stream(&self, stream_id: u32) {
        for index in 0..self.num_smr {
            let offset = gr0::SMR_BASE + index * 4;
            let smr = self.regs.read_gr0(offset);
            if smr & stream::SMR_VALID != 0 && smr & stream::SMR_ID == stream_id {
                self.regs.write_gr0(gr0::S2CR_BASE + index * 4, 0);
                self.regs.write_gr0(offset, 0);
                return;
            }
        }
    }

    /// Device reset: disable translation, clear TLBs, re-enable with fault
    /// reporting on.
    fn device_reset(&self) {
        // Disable the SMMU while reconfiguring.
        let mut cr0 = self.regs.read_gr0(gr0::S_CR0);
        cr0 |= cr0::CLIENTPD;
        cr0 &= !(cr0::USFCFG | cr0::GCFGFIE | cr0::GCFGFRE | cr0::GFIE | cr0::GFRE);
        self.regs.write_gr0(gr0::S_CR0, cr0);
        self.regs.write_gr0(gr0::TLBIALLNSNH, 0);
        self.tlb_sync_global();
        // Re-enable with global fault reporting and unknown-stream faults on.
        cr0 = self.regs.read_gr0(gr0::S_CR0);
        cr0 &= !cr0::CLIENTPD;
        cr0 |= cr0::GFRE | cr0::GFIE | cr0::GCFGFRE | cr0::GCFGFIE | cr0::USFCFG;
        self.regs.write_gr0(gr0::S_CR0, cr0);
    }
}

impl IommuHardware for ArmSmmuHw {
    /// Routes one master Stream ID to a context bank programmed for
    /// `domain`'s page tables.
    fn attach(&self, master: &[u32], domain: &IommuDomain) -> Result<(), IommuError> {
        let Some(stream_id) = master.first() else {
            return Err(IommuError::NotFound);
        };
        let bank = self.program_context_bank(domain)?;
        self.route_stream(*stream_id, bank.cbndx)
    }

    fn detach(&self, master: &[u32]) -> Result<(), IommuError> {
        if let Some(stream_id) = master.first() {
            self.unroute_stream(*stream_id);
        }
        Ok(())
    }

    /// Invalidates every programmed context bank. Domains map through one
    /// bank per SMMU, so this covers all attached masters' TLBs.
    fn flush_iotlb_all(&self) {
        for cbndx in 0..self.num_context_banks {
            if self.regs.read_cb(cbndx, cb::SCTLR) & sctlr::M != 0 {
                self.invalidate_context(cbndx);
            }
        }
        self.tlb_sync_global();
    }
}

/// Fault interrupt handler: report context-bank faults (translation /
/// permission) with Stream ID, IOVA and access type, then global faults.
/// Every status register is cleared by write-back and the stalled
/// transaction is terminated — the sequence that stops the interrupt storm.
struct ArmSmmuFaultHandler {
    hardware: Arc<ArmSmmuHw>,
}

impl ArmSmmuFaultHandler {
    /// Reads, reports and clears every pending context-bank fault.
    fn handle_context_faults(&self, hardware: &ArmSmmuHw) {
        let banks = hardware.programmed_banks.lock();
        for &cbndx in banks.iter() {
            let fsr = hardware.regs.read_cb(cbndx, cb::FSR);
            if fsr & fsr::FAULT == 0 {
                continue;
            }
            let far = hardware.regs.read_cb(cbndx, cb::FAR);
            let fsynr0 = hardware.regs.read_cb(cbndx, cb::FSYNR0);
            kiommu::report_fault(&IommuFault {
                instance: hardware.instance,
                master: u32::from(cbndx as u16) << 8 | (fsynr0 & 0xff),
                iova: kiommu::Iova::new(u64::from(far)),
                is_write: fsynr0 & fsynr0::WNR != 0,
            });
            if fsr & fsr::MULTI != 0 {
                log::error!("arm-smmu: multiple faults pending in context bank {cbndx}");
            }
            // Write-back clear, then terminate the stalled transaction so
            // the master resumes and the interrupt re-arms.
            hardware.regs.write_cb(cbndx, cb::FSR, fsr & fsr::FAULT);
            hardware.regs.write_cb(cbndx, cb::RESUME, resume::TERMINATE);
        }
    }
}

/// `CB_FSYNR0` bits used for fault attribution.
mod fsynr0 {
    pub(super) const WNR: u32 = 1 << 4;
}

/// `CB_RESUME` bits.
mod resume {
    pub(super) const TERMINATE: u32 = 1;
}

impl ArmSmmuHw {
    /// Whether any programmed bank still reports a pending fault after the
    /// handler pass (shared-line disambiguation).
    fn clear_pending_context_faults(&self) -> bool {
        let banks = self.programmed_banks.lock();
        let mut pending = false;
        for &cbndx in banks.iter() {
            let fsr = self.regs.read_cb(cbndx, cb::FSR);
            if fsr & fsr::FAULT != 0 {
                self.regs.write_cb(cbndx, cb::FSR, fsr & fsr::FAULT);
                self.regs.write_cb(cbndx, cb::RESUME, resume::TERMINATE);
                pending = true;
            }
        }
        pending
    }
}

impl DevIrqHandler for ArmSmmuFaultHandler {
    fn handle(&self, _irq: usize) -> IrqEvent {
        let hardware = &*self.hardware;
        self.handle_context_faults(hardware);
        let gfsr = hardware.regs.read_gr0(gr0::S_GFSR);
        if gfsr == 0 {
            // Not our global fault: claim the line only when a context-bank
            // fault was pending (this handler may share one IRQ across the
            // context-bank and global sources).
            return if hardware.clear_pending_context_faults() {
                IrqEvent::HANDLED
            } else {
                IrqEvent::NOT_HANDLED
            };
        }
        let gfsynr0 = hardware.regs.read_gr0(gr0::S_GFSYNR0);
        let gfsynr1 = hardware.regs.read_gr0(gr0::S_GFSYNR1);
        let gfar = hardware.regs.read_gr0(gr0::S_GFAR);
        kiommu::report_fault(&IommuFault {
            instance: hardware.instance,
            master: gfsynr1 & 0xffff,
            iova: kiommu::Iova::new(u64::from(gfar)),
            is_write: gfsynr0 & 0x2 != 0,
        });
        if gfsr & !fsr::FAULT != 0 {
            log::error!("arm-smmu: unhandled GFSR bits {:#x}", gfsr & !fsr::FAULT);
        }
        // Write-back clear of the reported bits rearms the interrupt.
        hardware.regs.write_gr0(gr0::S_GFSR, gfsr);
        IrqEvent::HANDLED
    }
}

/// Device-driver shell for one `arm,mmu-500` node.
struct ArmSmmuDriver;

impl kdevice::DeviceDriver for ArmSmmuDriver {
    type Data = Arc<ArmSmmuHw>;

    fn name(&self) -> &'static str {
        "arm-smmu-500"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Bus
    }

    fn bus_types(&self) -> &'static [kdevice::BusTypeId] {
        &[kdevice::BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn kdevice::DeviceMatcher {
        static SPEC: kdevice::FirmwareMatchSpec = kdevice::FirmwareMatchSpec {
            alias: "arm-smmu-500",
            dt_compatibles: &["arm,mmu-500", "arm,smmu-v2", "qcom,smmu-500"],
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
        let mapped =
            kdevice::device::managed::devm_iomap_with_provider(&PROVIDER, ctx, mmio, "arm-smmu")
                .map_err(map_res_err)?;
        let size = mmio.size;

        // SAFETY: `mapped` is the live register mapping; `probe` validates
        // the derived page layout against `size` before returning.
        let regs = unsafe { SmmuRegs::probe(mapped, size)? };

        let idr0 = regs.read_gr0(gr0::ID0);
        let idr2 = regs.read_gr0(gr0::ID2);
        if idr0 & idr0::S1TS == 0 {
            log::error!("arm-smmu: IDR0 reports no stage-1 support; refusing");
            return Err(DriverError::Unsupported);
        }
        // SAFETY: `mapped` is the GR0 base validated by `SmmuRegs::probe`.
        let num_context_banks = (unsafe { idr1::read(mapped) } & idr1::NUMCB) as usize;
        let num_smr = (idr0 & idr0::NUMSMRG) as usize;
        if num_context_banks == 0 || num_context_banks > MAX_CBS {
            log::error!("arm-smmu: unusable context bank count {num_context_banks}");
            return Err(DriverError::BadState);
        }
        let coherent_walk = idr0 & idr0::CTTW != 0;
        // IDR2.IAS encodes the input/output address size; kept for probes.
        let _ = idr2 & 0xf;

        let hardware = Arc::new(ArmSmmuHw {
            instance,
            regs,
            num_context_banks,
            num_smr,
            next_smr: AtomicU32::new(0),
            vmid_cursor: AtomicU32::new(0),
            programmed_banks: SpinNoIrq::new(Vec::new()),
        });

        if let Some(irq) = device.first_irq() {
            let handler: Arc<dyn DevIrqHandler> = Arc::new(ArmSmmuFaultHandler {
                hardware: hardware.clone(),
            });
            kdevice::device::managed::devm_request_irq_with_provider(&PROVIDER, ctx, irq, handler)
                .map_err(map_res_err)?;
        }

        // Build the stage-1 domain over an ARM LPAE table with the probed
        // walk coherency, then reset the SMMU into translation mode.
        let config = ArmLpaeConfig {
            va_bits: 48,
            pa_bits: 44,
            coherent_walk,
        };
        let page_table =
            ArmLpae::new(config, Arc::new(DmaTableAllocator)).map_err(|_| DriverError::NoMemory)?;
        let domain = Arc::new(IommuDomain::new(Box::new(page_table), 1 << 32, 4096));
        let registered = kiommu::register_instance(instance, hardware.clone(), domain);
        hardware.device_reset();
        log::info!(
            "arm-smmu: instance {instance:?} registered (cbs {num_context_banks}, smrs {num_smr}, \
             coherent {coherent_walk}, root {:#x})",
            registered.root_table_address()
        );
        Ok(hardware)
    }

    fn remove(
        &self,
        _ctx: &kdevice::RemoveContext<'_>,
        _data: Option<&mut Self::Data>,
    ) -> DriverResult<()> {
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
    kdevice::BoxedDriver::new(ArmSmmuDriver)
}
