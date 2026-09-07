// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Compiler-checked API constraints; these are not Kani proofs of concurrency.
//!
//! Owned values cannot be byte-copied as static slot templates:
//! ```compile_fail,E0277
//! kcpu_slot::cpu_slot! { static OWNED: String = String::new(); }
//! ```
//! References cannot be used as static slot values:
//! ```compile_fail,E0277
//! kcpu_slot::cpu_slot_cell! { static REFERENCE: &'static u64 = &0; }
//! ```
//! The value trait cannot be implemented outside the sealed production module:
//! ```compile_fail,E0277
//! #[derive(Clone, Copy)]
//! struct External(u64);
//! impl kcpu_slot::StaticSlotValue for External {}
//! ```
//! Allocation metadata cannot be shared concurrently:
//! ```compile_fail,E0277
//! fn assert_sync<T: Sync>() {}
//! assert_sync::<kcpu_slot::CpuSlotChunk>();
//! ```
//! A non-Sync value cannot be read remotely through the safe accessor:
//! ```compile_fail,E0277
//! fn remote(slot: &kcpu_slot::DynamicCpuSlot<'_, core::cell::Cell<u8>>) {
//!     slot.get_remote(kcpu_slot::CpuId::new(0));
//! }
//! ```
//! A dynamic handle cannot outlive its chunk:
//! ```compile_fail,E0515
//! fn escape() -> kcpu_slot::DynamicCpuSlot<'static, u64> {
//!     let mut backing = [0u64; 8];
//!     // SAFETY: The local backing fits one aligned area; the attempted handle
//!     // escape is the lifetime error this compile-fail example must reject.
//!     let chunk = unsafe {
//!         kcpu_slot::CpuSlotChunk::from_raw_parts(backing.as_mut_ptr().cast(), 1, 64).unwrap()
//!     };
//!     chunk.alloc::<u64, ()>(|_| Ok(0)).unwrap()
//! }
//! ```

#[test]
fn static_value_and_descriptor_traits() {
    fn value<T: kcpu_slot::StaticSlotValue>() {}
    fn sync<T: Sync>() {}
    fn send<T: Send>() {}
    value::<bool>();
    value::<u8>();
    value::<u16>();
    value::<u32>();
    value::<u64>();
    value::<usize>();
    value::<i8>();
    value::<i16>();
    value::<i32>();
    value::<i64>();
    value::<isize>();
    value::<[[u64; 2]; 3]>();
    value::<[u64; 0]>();
    sync::<kcpu_slot::CpuSlot<u64>>();
    send::<kcpu_slot::CpuSlot<u64>>();
    sync::<kcpu_slot::CpuSlotCell<u64>>();
}
