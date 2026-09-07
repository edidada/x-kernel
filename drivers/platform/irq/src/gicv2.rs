// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! GICv2 backend.

use arm_gic_driver::v2::*;
use kcpu_id_map::KCpuMaskExt;
use kirq::TargetCpu;
use kspin::SpinNoIrq;
use lazyinit::LazyInit;

static GIC: LazyInit<SpinNoIrq<Gic>> = LazyInit::new();
static TRAP_OP: LazyInit<TrapOp> = LazyInit::new();
// A CPU-local handle exposes the driver's single-write SGI operation without
// borrowing the shared configuration object on runtime or terminal paths.
#[percpu::def_percpu]
static IPI_INTERFACE: LazyInit<CpuInterface> = LazyInit::new();
const GUEST_VTIMER_IRQ: usize = 27;

pub fn init(gicd_base: memaddr::VirtAddr, gicc_base: memaddr::VirtAddr) {
    if GIC.get().is_some() {
        return;
    }
    info!("Initialize GICv2...");
    let gicd_base = VirtAddr::new(gicd_base.into());
    let gicc_base = VirtAddr::new(gicc_base.into());
    // SAFETY: both distributor and CPU interface bases come from platform
    // discovery and point at the mapped GICv2 MMIO frames for this machine.
    let mut gic = unsafe { Gic::new(gicd_base, gicc_base, None) };
    gic.init();
    GIC.init_once(SpinNoIrq::new(gic));
    let cpu = GIC.lock().cpu_interface();
    TRAP_OP.init_once(cpu.trap_operations());
}

pub fn init_current_cpu() {
    debug!("Initialize GICv2 CPU Interface...");
    let mut cpu = GIC.lock().cpu_interface();
    cpu.init_current_cpu();
    cpu.set_eoi_mode_ns(true);
    IPI_INTERFACE.with_current(|interface| {
        // The primary CPU is configured in both early and final init. Its
        // register mapping stays fixed, so retain the first sender handle.
        let _ = interface.call_once(|| cpu);
    });
}

pub fn set_trigger(interrupt_id: usize, edge: bool) {
    trace!("GICv2 set trigger: {interrupt_id} {edge}");
    // SAFETY: callers pass a hardware IRQ number understood by this GIC
    // instance; `IntId::raw` only wraps that validated numeric identifier.
    let intid = unsafe { IntId::raw(interrupt_id as u32) };
    let cfg = if edge { Trigger::Edge } else { Trigger::Level };
    GIC.lock().set_cfg(intid, cfg);
}

pub fn enable(irq: usize, enabled: bool) {
    trace!("GICv2 set enable: {irq} {enabled}");
    // SAFETY: callers pass a hardware IRQ number understood by this GIC
    // instance; `IntId::raw` only wraps that validated numeric identifier.
    let intid = unsafe { IntId::raw(irq as u32) };
    let gic = GIC.lock();
    gic.set_irq_enable(intid, enabled);
}

pub fn set_prio(irq: usize, priority: u8) {
    // SAFETY: callers pass a hardware IRQ number understood by this GIC
    // instance; `IntId::raw` only wraps that validated numeric identifier.
    let intid = unsafe { IntId::raw(irq as u32) };
    let gic = GIC.lock();
    gic.set_priority(intid, priority);
}

/// IRQ path: ack + early EOI. GICv2 has no NMI classification.
/// Mirrors `__gic_handle_irq_from_irqson`.
pub(super) fn dispatch_irq_from_irqson() -> Option<kirq::Virq> {
    let (hwirq, completion_cookie) = claim_irq()?;
    kirq::generic_handle_irq(kirq::PendingIrq::new(
        kirq::IrqRef::Domain(kirq::GIC_ROOT_DOMAIN, hwirq),
        completion_cookie,
    ))
}

fn claim_irq() -> Option<(usize, usize)> {
    let ack = TRAP_OP.ack();
    if ack.is_special() {
        return None;
    }

    let irq = match ack {
        Ack::Other(intid) => intid,
        Ack::SGI { intid, cpu_id: _ } => intid,
    }
    .to_u32() as usize;
    trace!("IRQ: {ack:?}");

    // GIC non-secure mode requires an early EOI to lower the running priority,
    // while the actual deactivation is deferred to `complete_irq`.
    TRAP_OP.eoi(ack);
    // Order the acknowledged interrupt before the handler.
    aarch64_cpu::asm::barrier::isb(aarch64_cpu::asm::barrier::SY);
    Some((irq, u32::from(ack) as usize))
}

/// NMI path: ack the interrupt for completion.
///
/// GICv2 has **no pseudo‑NMI support**: PMR masking cannot promote an IRQ
/// into a non‑maskable interrupt, so this path is unreachable in a correct
/// build (the platform rejects NMI on GICv2).  The interrupt is still
/// acknowledged and completed so the GIC does not keep it pending.
pub fn dispatch_irq_from_irqsoff() -> Option<(usize, usize)> {
    claim_irq()
}

fn skip_dir_for_vmm_guest_irq(ack: Ack) -> bool {
    if !kbuild_config::KFEAT_VMM {
        return false;
    }
    let hwirq = match ack {
        Ack::Other(intid) => intid,
        Ack::SGI { intid, cpu_id: _ } => intid,
    }
    .to_u32() as usize;

    // KVM-style GICv2 HW LR injection for the guest virtual timer keeps the
    // physical PPI active until the guest EOI/deactivate path consumes the HW
    // LR. Host-side DIR here would drop the physical backing interrupt before
    // the guest observes it.
    hwirq == GUEST_VTIMER_IRQ
}

pub fn complete_irq(completion_cookie: usize) {
    let ack = Ack::from(completion_cookie as u32);
    if skip_dir_for_vmm_guest_irq(ack) {
        return;
    }
    TRAP_OP.dir(ack);
}

pub fn notify_cpu(interrupt_id: usize, target: TargetCpu) -> bool {
    if interrupt_id >= 16 {
        return false;
    }
    // `dmb ishst` orders the caller's prior Normal-memory stores before the
    // SGI MMIO write, which is required for cross-CPU publish-before-notify
    // semantics.
    aarch64_cpu::asm::barrier::dmb(aarch64_cpu::asm::barrier::ISHST);
    IPI_INTERFACE.with_current(|interface| {
        let Some(cpu) = interface.get() else {
            return false;
        };
        match target {
            TargetCpu::Self_ => {
                cpu.send_sgi(IntId::sgi(interrupt_id as u32), SGITarget::Current);
            }
            TargetCpu::Specific(cpu_id) => {
                let target_list = TargetList::new(core::iter::once(cpu_id));
                cpu.send_sgi(
                    IntId::sgi(interrupt_id as u32),
                    SGITarget::TargetList(target_list),
                );
            }
            TargetCpu::AllButSelf { .. } => {
                cpu.send_sgi(IntId::sgi(interrupt_id as u32), SGITarget::AllOther);
            }
            TargetCpu::Mask(mask) => {
                for cpu_id in mask.iter_logical() {
                    let target_list = TargetList::new(core::iter::once(cpu_id.as_usize()));
                    cpu.send_sgi(
                        IntId::sgi(interrupt_id as u32),
                        SGITarget::TargetList(target_list),
                    );
                }
            }
        }
        true
    })
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, def_test};

    use super::*;

    // The self-SGI is handled only after the guard restores local IRQs. A
    // sender that re-enters the configuration lock would deadlock here.
    #[def_test(serial)]
    fn send_sgi_does_not_acquire_configuration_lock() {
        let Some(gic) = GIC.get() else {
            return unittest::TestResult::Ignored;
        };
        let _guard = gic.lock();
        let cpu = kplat::cpu::id();
        let mut mask = kcpu_id_map::KCpuMask::new();
        mask.set_logical(cpu, true);
        for target in [
            TargetCpu::Self_,
            TargetCpu::Specific(cpu.as_usize()),
            TargetCpu::Mask(mask),
            TargetCpu::AllButSelf {
                me: cpu.as_usize(),
                total: kcpu_id_map::nr_cpus(),
            },
        ] {
            assert!(notify_cpu(super::super::GENERIC_IPI_SGI, target));
        }
    }
}
