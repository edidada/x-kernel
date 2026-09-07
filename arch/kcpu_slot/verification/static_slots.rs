// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kcpu_slot::{CpuId, PinCurrentCpu};

kcpu_slot::cpu_slot! { static WORD: u64 = 0x102; }
kcpu_slot::cpu_slot_cell! { static CELL: u64 = 0x304; }

// Explicit linker/architecture contract models. These functions are assumptions,
// not verified replacements for the machine instructions or linker script.
fn word_offset() -> usize {
    8
}
fn cell_offset() -> usize {
    16
}
fn template_size() -> usize {
    24
}
static TEMPLATE: [u8; 24] = [
    0, 0, 0, 0, 0, 0, 0, 0, 2, 1, 0, 0, 0, 0, 0, 0, 4, 3, 0, 0, 0, 0, 0, 0,
];
fn template_start() -> usize {
    TEMPLATE.as_ptr() as usize
}
static mut MODEL_BASE: usize = 0;
unsafe fn set_current_base(base: usize) {
    // SAFETY: initialize_cpu's harness supplies an aligned, unique 64-byte
    // allocation that outlives the modeled CPU context. Execution is sequential.
    unsafe {
        *(base as *mut usize) = base;
        MODEL_BASE = base;
    }
}
fn current_base() -> usize {
    // SAFETY: Only sequential harness initialization modifies MODEL_BASE.
    unsafe { MODEL_BASE }
}
struct Pin;
// SAFETY: Created after initialization; no CPU switch occurs while it is used.
unsafe impl PinCurrentCpu for Pin {
    fn current_cpu(&self) -> CpuId {
        CpuId::new(0)
    }
}
#[repr(align(64))]
struct Backing([u8; 64]);

#[kani::proof]
#[kani::unwind(65)]
#[kani::stub(WORD::offset, word_offset)]
#[kani::stub(CELL::offset, cell_offset)]
#[kani::stub(kcpu_slot::layout::template_start, template_start)]
#[kani::stub(kcpu_slot::template_size, template_size)]
#[kani::stub(kcpu_slot::arch::set_current_base, set_current_base)]
#[kani::stub(kcpu_slot::arch::current_base, current_base)]
fn initialization_and_static_access_under_platform_contract() {
    let mut backing = Backing([0xAA; 64]);
    let base = backing.0.as_mut_ptr();
    // SAFETY: The modeled linker template is valid, non-overlapping and 24
    // bytes long; the exclusive aligned destination has the full 64-byte area.
    unsafe { kcpu_slot::initialize_cpu(base) };
    assert_eq!(kcpu_slot::area_size(), 64);
    let pin = Pin;
    assert_eq!(pin.base(), base as usize);
    // SAFETY: Genuine macro-generated descriptors use modeled linker offsets
    // into the initialized area. The CPU is fixed and accesses are sequential.
    unsafe {
        assert_eq!(*WORD.get(&pin), 0x102);
        assert_eq!(*WORD.get_at(base as usize), 0x102);
        assert_eq!(CELL.read(&pin), 0x304);
        let value: u64 = kani::any();
        *WORD.get_mut(&pin) = value;
        CELL.write(&pin, !value);
        assert_eq!(*WORD.get(&pin), value);
        assert_eq!(*CELL.get(&pin).get(), !value);
    }
    for index in 24..64 {
        assert_eq!(backing.0[index], 0xAA);
    }
}

static ZERO_TEMPLATE: [u8; 64] = [0; 64];
fn zero_template_start() -> usize {
    ZERO_TEMPLATE.as_ptr() as usize
}
fn full_template_size() -> usize {
    64
}
fn array_offset() -> usize {
    32
}

// Each expansion compiles the real declaration macros and all static accessors
// for one scalar and its array form. Only the linker/register boundary is modeled.
macro_rules! static_type_contract {
    ($module:ident, $ty:ty, $zero:expr) => {
        mod $module {
            use super::*;
            kcpu_slot::cpu_slot! { static VALUE: $ty = $zero; }
            kcpu_slot::cpu_slot_cell! { static ARRAY: [$ty; 2] = [$zero; 2]; }

            #[kani::proof]
            #[kani::unwind(65)]
            #[kani::stub(VALUE::offset, word_offset)]
            #[kani::stub(ARRAY::offset, array_offset)]
            #[kani::stub(kcpu_slot::layout::template_start, zero_template_start)]
            #[kani::stub(kcpu_slot::template_size, full_template_size)]
            #[kani::stub(kcpu_slot::arch::set_current_base, set_current_base)]
            #[kani::stub(kcpu_slot::arch::current_base, current_base)]
            fn static_type_under_platform_contract() {
                let mut backing = Backing([0xAA; 64]);
                let base = backing.0.as_mut_ptr();
                // SAFETY: The modeled 64-byte linker template has the declared
                // zero values and valid representations for this scalar/array;
                // the destination is unique, aligned, large enough and disjoint.
                unsafe { kcpu_slot::initialize_cpu(base) };
                let pin = Pin;
                let value: $ty = kani::any();
                let array: [$ty; 2] = kani::any();
                // SAFETY: Macro descriptors use modeled linker offsets in the
                // initialized area. The fixed CPU has sequential, exclusive
                // accesses; each mutable reference ends before the next access.
                unsafe {
                    assert_eq!(*VALUE.get(&pin), $zero);
                    assert_eq!(ARRAY.read(&pin), [$zero; 2]);
                    *VALUE.get_mut(&pin) = value;
                    ARRAY.write(&pin, array);
                    assert_eq!(*VALUE.get_at(base as usize), value);
                    assert_eq!(*VALUE.get(&pin), value);
                    assert_eq!(*ARRAY.get(&pin).get(), array);
                    assert_eq!(ARRAY.read(&pin), array);
                }
            }
        }
    };
}

static_type_contract!(boolean, bool, false);
static_type_contract!(unsigned8, u8, 0);
static_type_contract!(unsigned16, u16, 0);
static_type_contract!(unsigned32, u32, 0);
static_type_contract!(unsigned64, u64, 0);
static_type_contract!(unsigned_size, usize, 0);
static_type_contract!(signed8, i8, 0);
static_type_contract!(signed16, i16, 0);
static_type_contract!(signed32, i32, 0);
static_type_contract!(signed64, i64, 0);
static_type_contract!(signed_size, isize, 0);
static_type_contract!(empty_array, [u64; 0], []);
static_type_contract!(nested_array, [u8; 2], [0; 2]);

#[kani::proof]
#[kani::unwind(65)]
#[kani::stub(WORD::offset, word_offset)]
#[kani::stub(CELL::offset, cell_offset)]
#[kani::stub(kcpu_slot::layout::template_start, template_start)]
#[kani::stub(kcpu_slot::template_size, template_size)]
#[kani::stub(kcpu_slot::arch::set_current_base, set_current_base)]
#[kani::stub(kcpu_slot::arch::current_base, current_base)]
fn static_cpu_areas_remain_isolated_under_platform_contract() {
    let mut first = Backing([0; 64]);
    let mut second = Backing([0; 64]);
    let first_base = first.0.as_mut_ptr();
    let second_base = second.0.as_mut_ptr();
    // SAFETY: Each modeled CPU receives its own disjoint, aligned 64-byte area,
    // larger than the modeled template. Both areas outlive all accesses.
    unsafe { kcpu_slot::initialize_cpu(first_base) };
    // SAFETY: Same template and a second unique destination; no pin is alive
    // while the model installs this CPU's base register.
    unsafe { kcpu_slot::initialize_cpu(second_base) };
    let pin = Pin;
    let value: u64 = kani::any();
    // SAFETY: pin selects the second initialized area with no migration or
    // concurrent access. get_at reads the disjoint first CPU's initialized data.
    unsafe {
        *WORD.get_mut(&pin) = value;
        CELL.write(&pin, !value);
        assert_eq!(*WORD.get_at(first_base as usize), 0x102);
        assert_eq!(*WORD.get_at(second_base as usize), value);
        assert_eq!(CELL.read(&pin), !value);
    }
    assert_eq!(first.0[16], 4);
    assert_eq!(first.0[17], 3);
}

static mut SYMBOLIC_SIZE: usize = 0;
static mut SYMBOLIC_TEMPLATE: [u8; 64] = [0; 64];
fn symbolic_size() -> usize {
    // SAFETY: Each sequential harness sets the size before any modeled call.
    unsafe { SYMBOLIC_SIZE }
}
fn symbolic_template_start() -> usize {
    (&raw const SYMBOLIC_TEMPLATE).cast::<u8>() as usize
}
unsafe fn install_after_copy(base: usize) {
    // The assertion is a caller-order obligation: all bytes must have been
    // copied before the real implementation invokes the installation boundary.
    for index in 0..symbolic_size() {
        // SAFETY: The harness supplies 64 valid bytes, size <= 64, and no
        // concurrent access to the modeled source or unique destination.
        unsafe { assert_eq!(*((base as *const u8).add(index)), SYMBOLIC_TEMPLATE[index]) };
    }
    // SAFETY: The destination is aligned, unique, and outlives the CPU context.
    unsafe { set_current_base(base) };
}

#[kani::proof]
#[kani::stub(kcpu_slot::template_size, symbolic_size)]
fn area_size_under_linker_size_contract() {
    let size: usize = kani::any();
    // SAFETY: This harness owns the model state and has no concurrent readers.
    unsafe { SYMBOLIC_SIZE = size };
    let rounded = ((size as u128 + 63) / 64) * 64;
    let expected = rounded.min((usize::MAX - 63) as u128) as usize;
    assert_eq!(kcpu_slot::area_size(), expected);
}

#[kani::proof]
#[kani::unwind(65)]
#[kani::stub(kcpu_slot::template_size, symbolic_size)]
#[kani::stub(kcpu_slot::layout::template_start, symbolic_template_start)]
#[kani::stub(kcpu_slot::arch::set_current_base, install_after_copy)]
fn template_copy_and_install_order_under_platform_contract() {
    let size: usize = kani::any();
    kani::assume(size >= 8 && size <= 64);
    let mut template: [u8; 64] = kani::any();
    // The x86 self-pointer template is zero before per-CPU installation.
    for byte in &mut template[..8] {
        *byte = 0;
    }
    // SAFETY: The harness exclusively initializes its model globals before
    // calls; the byte array is valid for every modeled source length.
    unsafe {
        SYMBOLIC_SIZE = size;
        SYMBOLIC_TEMPLATE = template;
    }
    let mut backing = Backing([0xAA; 64]);
    let base = backing.0.as_mut_ptr();
    // SAFETY: The symbolic template is valid and disjoint, size is in 8..=64,
    // and the aligned 64-byte destination is exclusively owned and lives long
    // enough for the modeled installation and all subsequent observations.
    unsafe { kcpu_slot::initialize_cpu(base) };
    for index in 8..64 {
        assert_eq!(
            backing.0[index],
            if index < size { template[index] } else { 0xAA }
        );
    }
    assert_eq!(current_base(), base as usize);
    kani::cover!(size == 8, "self-pointer-only template");
    kani::cover!(size == 64, "full cache-line template");
}
