// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::cell::Cell;

use kcpu_slot::{CpuId, CpuSlotChunk, PinCurrentCpu, SlotInitError};

#[repr(align(64))]
struct Backing([u8; 256]);

fn cpu_count() -> usize {
    let count: usize = kani::any();
    kani::assume(count >= 1 && count <= 4);
    count
}

// This model supplies only a stable CPU identity. Dynamic access never reads
// the architecture base; real scheduler/IRQ pin acquisition is outside scope.
struct Pin(usize);
// SAFETY: The harness is sequential and the modeled CPU identity never changes.
// No base override is supplied or accessed by these dynamic-slot harnesses.
unsafe impl PinCurrentCpu for Pin {
    fn current_cpu(&self) -> CpuId {
        CpuId::new(self.0)
    }
}

#[kani::proof]
#[kani::unwind(5)]
fn allocation_isolation_and_access() {
    let mut backing = Backing([0; 256]);
    let count = cpu_count();
    let value: u64 = kani::any();
    // SAFETY: Four aligned 64-byte areas fit the unique backing allocation,
    // which outlives the chunk and every handle; all allocated types fit alignment.
    let chunk = unsafe { CpuSlotChunk::from_raw_parts(backing.0.as_mut_ptr(), count, 64).unwrap() };
    assert_eq!(chunk.cpu_count(), count);
    let first = chunk
        .alloc::<u8, ()>(|cpu| Ok(cpu.as_usize() as u8))
        .unwrap();
    let second = chunk
        .alloc::<u32, ()>(|cpu| Ok(cpu.as_usize() as u32 + 10))
        .unwrap();
    let third = chunk.alloc::<u64, ()>(|_| Ok(value)).unwrap();
    let selected: usize = kani::any();
    kani::assume(selected < count);
    let pin = Pin(selected);
    // SAFETY: The CPU is initialized, fixed, and no reference to its value is
    // live during this exclusive mutation. No concurrent access exists.
    unsafe { *third.get_mut(&pin).unwrap() = !value };
    for cpu in 0..count {
        assert_eq!(first.get_remote(CpuId::new(cpu)), Some(&(cpu as u8)));
        assert_eq!(second.get_remote(CpuId::new(cpu)), Some(&(cpu as u32 + 10)));
        let expected = if cpu == selected { !value } else { value };
        assert_eq!(third.get_remote(CpuId::new(cpu)), Some(&expected));
    }
    // SAFETY: The pin identifies an initialized CPU; the preceding mutable
    // reference has ended and this read has no concurrent writer.
    assert_eq!(unsafe { third.get(&pin) }, Some(&!value));
    let remote: usize = kani::any();
    if remote >= count {
        assert!(third.get_remote(CpuId::new(remote)).is_none());
    }
    kani::cover!(count == 4 && selected == 3, "last CPU mutation");
}

#[kani::proof]
#[kani::unwind(5)]
fn capacity_is_not_reclaimed_on_drop() {
    let mut backing = Backing([0; 256]);
    let count = cpu_count();
    let words: usize = kani::any();
    kani::assume(words >= 1 && words <= 4);
    // SAFETY: count <= 4 and stride <= 32 fit the unique backing; the stride
    // and base are u64-aligned and backing outlives every handle.
    let chunk =
        unsafe { CpuSlotChunk::from_raw_parts(backing.0.as_mut_ptr(), count, words * 8).unwrap() };
    for _ in 0..words {
        let slot = chunk.alloc::<u64, ()>(|_| Ok(0)).unwrap();
        drop(slot);
    }
    let calls = Cell::new(0);
    let result = chunk.alloc::<u64, ()>(|_| {
        calls.set(calls.get() + 1);
        Ok(0)
    });
    assert!(matches!(result, Err(SlotInitError::NoSpace)));
    assert_eq!(calls.get(), 0);
    // A failed reservation also leaves the exhausted cursor unchanged.
    assert!(matches!(
        chunk.alloc::<u8, ()>(|_| Ok(0)),
        Err(SlotInitError::NoSpace)
    ));
    kani::cover!(count == 4 && words == 4, "maximum modeled capacity");
}

struct Tracked {
    drops: *mut [u8; 4],
    cpu: usize,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        // SAFETY: Each harness keeps its counters alive past all handles and
        // cleanup; cpu is in 0..4. Drops are sequential, with no live counter
        // reference during mutation. No other thread accesses these counters.
        unsafe { (*self.drops)[self.cpu] += 1 };
    }
}

#[kani::proof]
#[kani::unwind(5)]
fn initialization_failure_cleanup_and_reservation() {
    let mut backing = Backing([0; 256]);
    let mut drops = [0u8; 4];
    let count = cpu_count();
    let failure: usize = kani::any();
    kani::assume(failure < count);
    // SAFETY: Tracked is 16 bytes with 8-byte alignment on the pinned target;
    // all CPU areas fit the unique backing, which outlives the chunk.
    let chunk = unsafe { CpuSlotChunk::from_raw_parts(backing.0.as_mut_ptr(), count, 16).unwrap() };
    let mut calls = 0;
    let result = chunk.alloc(|cpu| {
        assert_eq!(cpu.as_usize(), calls);
        calls += 1;
        if cpu.as_usize() == failure {
            Err(failure)
        } else {
            Ok(Tracked {
                drops: &raw mut drops,
                cpu: cpu.as_usize(),
            })
        }
    });
    assert!(matches!(result, Err(SlotInitError::Init(index)) if index == failure));
    drop(result);
    assert_eq!(calls, failure + 1);
    for cpu in 0..4 {
        assert_eq!(drops[cpu], u8::from(cpu < failure));
    }
    assert!(matches!(
        chunk.alloc::<u8, ()>(|_| Ok(0)),
        Err(SlotInitError::NoSpace)
    ));
    kani::cover!(failure == 0, "failure before first initialization");
    kani::cover!(failure == 3, "failure after three initializations");
}

#[kani::proof]
#[kani::unwind(5)]
fn successful_handle_drops_each_cpu_once() {
    let mut backing = Backing([0; 256]);
    let mut drops = [0u8; 4];
    let count = cpu_count();
    // SAFETY: The aligned backing fits count 16-byte areas and outlives all
    // handles. The unique counters also outlive every Tracked destructor.
    let chunk = unsafe { CpuSlotChunk::from_raw_parts(backing.0.as_mut_ptr(), count, 16).unwrap() };
    let mut calls = 0;
    let slot = chunk
        .alloc::<Tracked, ()>(|cpu| {
            assert_eq!(cpu.as_usize(), calls);
            calls += 1;
            Ok(Tracked {
                drops: &raw mut drops,
                cpu: cpu.as_usize(),
            })
        })
        .unwrap();
    assert_eq!(calls, count);
    assert_eq!(drops, [0; 4]);
    drop(slot);
    for cpu in 0..4 {
        assert_eq!(drops[cpu], u8::from(cpu < count));
    }
}

#[kani::proof]
#[kani::unwind(5)]
fn zero_sized_slots_initialize_without_consuming_capacity() {
    let mut backing = Backing([0; 256]);
    let count = cpu_count();
    // SAFETY: The unique backing fits count aligned 8-byte areas and outlives
    // all handles; both () and u64 satisfy the constructor's alignment contract.
    let chunk = unsafe { CpuSlotChunk::from_raw_parts(backing.0.as_mut_ptr(), count, 8).unwrap() };
    let mut calls = 0;
    let first = chunk
        .alloc::<(), ()>(|_| {
            calls += 1;
            Ok(())
        })
        .unwrap();
    let second = chunk
        .alloc::<(), ()>(|_| {
            calls += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(calls, count * 2);
    let full = chunk.alloc::<u64, ()>(|_| Ok(42)).unwrap();
    for cpu in 0..count {
        assert_eq!(first.get_remote(CpuId::new(cpu)), Some(&()));
        assert_eq!(second.get_remote(CpuId::new(cpu)), Some(&()));
        assert_eq!(full.get_remote(CpuId::new(cpu)), Some(&42));
    }
}

#[kani::proof]
#[kani::unwind(9)]
fn misaligned_base_does_not_reserve_space() {
    let mut backing = Backing([0; 256]);
    let offset: usize = kani::any();
    kani::assume(offset >= 1 && offset <= 7);
    let base = backing.0.as_mut_ptr().wrapping_add(offset);
    // SAFETY: The constructor requires writable storage and stride alignment,
    // both satisfied by this unique 8-byte region. alloc checks base alignment
    // before initializing u64; u8 requires only byte alignment.
    let chunk = unsafe { CpuSlotChunk::from_raw_parts(base, 1, 8).unwrap() };
    let result = chunk.alloc::<u64, ()>(|_| panic!("must reject before initialization"));
    assert!(matches!(result, Err(SlotInitError::Misaligned)));
    let slot = chunk.alloc::<[u8; 8], ()>(|_| Ok([9; 8])).unwrap();
    assert_eq!(slot.get_remote(CpuId::new(0)), Some(&[9; 8]));
}

#[kani::proof]
#[kani::unwind(9)]
fn failed_capacity_reservation_preserves_remaining_bytes() {
    let mut backing = Backing([0; 256]);
    let count = cpu_count();
    // SAFETY: The unique backing fits count aligned 8-byte CPU areas and
    // outlives all handles; u8, u64 and [u8; 7] meet stride alignment.
    let chunk = unsafe { CpuSlotChunk::from_raw_parts(backing.0.as_mut_ptr(), count, 8).unwrap() };
    let first = chunk.alloc::<u8, ()>(|_| Ok(1)).unwrap();
    let rejected = chunk.alloc::<u64, ()>(|_| panic!("must reject before initialization"));
    assert!(matches!(rejected, Err(SlotInitError::NoSpace)));
    let remainder = chunk.alloc::<[u8; 7], ()>(|_| Ok([2; 7])).unwrap();
    for cpu in 0..count {
        assert_eq!(first.get_remote(CpuId::new(cpu)), Some(&1));
        assert_eq!(remainder.get_remote(CpuId::new(cpu)), Some(&[2; 7]));
    }
}

#[kani::proof]
fn constructor_rejects_null_all_numeric_inputs() {
    let count: usize = kani::any();
    let stride: usize = kani::any();
    // SAFETY: Null is a documented rejected argument with no backing obligation.
    assert!(
        unsafe { CpuSlotChunk::from_raw_parts(core::ptr::null_mut(), count, stride) }.is_none()
    );
}

#[kani::proof]
fn constructor_rejects_zero_dimensions() {
    let other: usize = kani::any();
    let base = core::ptr::NonNull::<u8>::dangling().as_ptr();
    // SAFETY: A zero dimension is rejected without a backing-store obligation.
    assert!(unsafe { CpuSlotChunk::from_raw_parts(base, 0, other) }.is_none());
    // SAFETY: A zero dimension is rejected without a backing-store obligation.
    assert!(unsafe { CpuSlotChunk::from_raw_parts(base, other, 0) }.is_none());
}

#[kani::proof]
#[kani::solver(cvc5)]
fn constructor_rejects_total_size_overflow() {
    let count: usize = kani::any();
    let stride: usize = kani::any();
    kani::assume((count as u128) * (stride as u128) > usize::MAX as u128);
    let base = core::ptr::NonNull::<u8>::dangling().as_ptr();
    // SAFETY: Overflow is a documented rejected argument; the widened product
    // independently establishes this domain without assuming the return value.
    assert!(unsafe { CpuSlotChunk::from_raw_parts(base, count, stride) }.is_none());
    kani::cover!(count == usize::MAX && stride == 2, "overflow boundary");
}

#[kani::proof]
#[kani::unwind(5)]
fn misaligned_stride_is_rejected_without_reservation() {
    let mut backing = Backing([0; 256]);
    let count = cpu_count();
    let stride: usize = kani::any();
    kani::assume(stride >= 1 && stride <= 64 && stride % 8 != 0);
    // SAFETY: count * stride <= 256 fits the unique backing, which outlives
    // all handles. Type alignment is checked by alloc, not the constructor.
    let chunk =
        unsafe { CpuSlotChunk::from_raw_parts(backing.0.as_mut_ptr(), count, stride).unwrap() };
    assert!(matches!(
        chunk.alloc::<u64, ()>(|_| panic!("must reject first")),
        Err(SlotInitError::Misaligned)
    ));
    let byte = chunk.alloc::<u8, ()>(|_| Ok(99)).unwrap();
    for cpu in 0..count {
        assert_eq!(byte.get_remote(CpuId::new(cpu)), Some(&99));
    }
}

#[kani::proof]
#[kani::unwind(5)]
fn local_access_rejects_cpu_outside_chunk() {
    let mut backing = Backing([0; 256]);
    let count: usize = kani::any();
    kani::assume(count >= 1 && count <= 3);
    let current: usize = kani::any();
    kani::assume(current >= count && current < 4);
    // SAFETY: The backing uniquely provides count aligned areas and outlives
    // every handle. The modeled machine has four CPUs, but this chunk has fewer.
    let chunk = unsafe { CpuSlotChunk::from_raw_parts(backing.0.as_mut_ptr(), count, 8).unwrap() };
    let slot = chunk.alloc::<u64, ()>(|_| Ok(7)).unwrap();
    let pin = Pin(current);
    // SAFETY: The pin is a valid, fixed CPU of the modeled machine; accesses
    // are exclusive. This CPU has no corresponding area in this smaller chunk.
    assert!(unsafe { slot.get(&pin) }.is_none());
    // SAFETY: Same valid pin and no outstanding references or concurrent access.
    assert!(unsafe { slot.get_mut(&pin) }.is_none());
}
