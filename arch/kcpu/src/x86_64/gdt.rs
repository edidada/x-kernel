// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Global Descriptor Table (GDT) and Task State Segment (TSS) setup.

use kcpu_slot::{CpuSlot, cpu_slot_cell};
use x86_64::{
    PrivilegeLevel,
    instructions::tables::load_tss,
    registers::segmentation::{CS, DS, ES, SS, Segment, SegmentSelector},
    structures::gdt::Descriptor,
};

// Raw template so trap-entry assembly can reach TSS fields through
// `gs:[offset __KCPU_SLOT_TSS]` without going through a descriptor.
// SAFETY: x86 trap assembly resolves this unique data symbol as the TSS template base and uses
// its fixed gs-relative field offsets; the static has the expected TaskStateSegment layout.
#[unsafe(no_mangle)]
#[used]
#[unsafe(link_section = ".cpu_slot.template")]
pub(super) static __KCPU_SLOT_TSS: x86_64::structures::tss::TaskStateSegment =
    x86_64::structures::tss::TaskStateSegment::new();

/// Typed descriptor over the raw TSS template above.
#[doc(hidden)]
pub(super) static TSS: CpuSlot<x86_64::structures::tss::TaskStateSegment> =
    // SAFETY: the offset function names the raw template above.
    unsafe {
        CpuSlot::from_offset_fn(|| kcpu_slot::__cpu_slot_symbol_offset!(__KCPU_SLOT_TSS))
    };

cpu_slot_cell! {
    static GDT: x86_64::structures::gdt::GlobalDescriptorTable =
        x86_64::structures::gdt::GlobalDescriptorTable::new();
}

/// Kernel code segment for 64-bit mode.
pub(super) const KCODE64: SegmentSelector = SegmentSelector::new(1, PrivilegeLevel::Ring0);
/// Kernel data segment.
pub(super) const KDATA: SegmentSelector = SegmentSelector::new(2, PrivilegeLevel::Ring0);
/// User data segment.
pub(super) const UDATA: SegmentSelector = SegmentSelector::new(3, PrivilegeLevel::Ring3);
/// User code segment for 64-bit mode.
pub(super) const UCODE64: SegmentSelector = SegmentSelector::new(4, PrivilegeLevel::Ring3);

/// Initializes the per-CPU TSS and GDT structures and loads them into the
/// current CPU.
pub(super) fn init() {
    // SAFETY: called during per-CPU initialization (boot or secondary CPU
    // bring-up), single-threaded for this CPU. `GlobalDescriptorTable::load`
    // requires a `'static` reference, so this uses the self-hosting mutable
    // accessor; it is the one x86 raw boundary.
    let gdt = unsafe { GDT.current_shared_mut() };
    assert_eq!(gdt.append(Descriptor::kernel_code_segment()), KCODE64);
    assert_eq!(gdt.append(Descriptor::kernel_data_segment()), KDATA);
    assert_eq!(gdt.append(Descriptor::user_data_segment()), UDATA);
    assert_eq!(gdt.append(Descriptor::user_code_segment()), UCODE64);
    let nmi_top = super::nmi::init();
    // SAFETY: the local TSS is still exclusively initialized before loading
    // the IDT; NMI stacks and metadata are complete and remain mapped forever.
    let tss_offset = kcpu_slot::__cpu_slot_symbol_offset!(__KCPU_SLOT_TSS);
    let tss_state = (x86_64::registers::model_specific::GsBase::read().as_u64() as usize
        + tss_offset) as *mut x86_64::structures::tss::TaskStateSegment;
    // SAFETY: no descriptor/entry yet reads the field; write through the raw
    // template address without creating a lasting mutable TSS reference.
    let ist = unsafe { core::ptr::addr_of_mut!((*tss_state).interrupt_stack_table) }
        .cast::<x86_64::VirtAddr>();
    // SAFETY: the derived IST field is writable during exclusive TSS setup.
    unsafe { ist.write_unaligned(nmi_top) };
    // The TSS template is permanent and `TaskStateSegment: Sync`, so the
    // guard-free shared accessor yields the `&'static` descriptor needs.
    let tss = gdt.append(Descriptor::tss_segment(TSS.current_shared()));
    gdt.load();
    // SAFETY: KDATA was verified against the descriptor installed in the now-loaded GDT; DS may
    // select it.
    unsafe { DS::set_reg(KDATA) };
    // SAFETY: KDATA names the verified kernel data descriptor in the loaded GDT; ES may select it.
    unsafe { ES::set_reg(KDATA) };
    // SAFETY: KDATA names the verified writable kernel stack/data descriptor in the loaded GDT.
    unsafe { SS::set_reg(KDATA) };
    // SAFETY: KCODE64 names the verified 64-bit kernel code descriptor in the loaded GDT.
    unsafe { CS::set_reg(KCODE64) };
    // SAFETY: tss selects the TSS descriptor just appended; its per-CPU storage is permanent and
    // the GDT is loaded.
    unsafe { load_tss(tss) };
}
