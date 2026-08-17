// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task-context magazine storage for slab objects.
//!
//! [`super::kmem_cache::KmemCacheSet`] owns access to this storage. A normal
//! task touches only its current CPU's pair of magazines while preemption is
//! disabled by `percpu::with_current`. Hardirq callers use that cache set's
//! separate reserve instead; they must never mutate this storage.

use core::{mem, ptr::NonNull};

use alloc_engine::SizeClass;

/// Largest single-class magazine capacity.
pub(crate) const MAX_MAGAZINE_CAPACITY: usize = 48;

/// A stack-owned transfer batch between a magazine and a central cache.
///
/// The caller supplies the matching [`SizeClass`], so this internal type does
/// not duplicate class identity in every batch entry.
#[derive(Clone, Copy)]
pub(crate) struct ObjectBatch {
    objects: [usize; MAX_MAGAZINE_CAPACITY],
    len: u8,
}

impl ObjectBatch {
    /// Create an empty batch.
    pub(crate) const fn new() -> Self {
        Self {
            objects: [0; MAX_MAGAZINE_CAPACITY],
            len: 0,
        }
    }

    /// Return whether the batch contains no objects.
    pub(crate) const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Return the number of entries in the batch.
    pub(crate) const fn len(&self) -> usize {
        self.len as usize
    }

    /// Append one object address.
    pub(crate) fn push(&mut self, object: NonNull<u8>) {
        assert!(self.len() < MAX_MAGAZINE_CAPACITY, "object batch overflow");
        self.objects[self.len()] = object.as_ptr() as usize;
        self.len += 1;
    }

    /// Remove one object address.
    pub(crate) fn pop(&mut self) -> Option<NonNull<u8>> {
        if self.is_empty() {
            return None;
        }
        self.len -= 1;
        let object = self.objects[self.len()] as *mut u8;
        // SAFETY: objects enter the batch only through `push`, whose
        // `NonNull` input proves this stored address is non-null.
        Some(unsafe { NonNull::new_unchecked(object) })
    }
}

impl Default for ObjectBatch {
    fn default() -> Self {
        Self::new()
    }
}

/// Target number of objects transferred from a central cache on a miss.
///
/// These values preserve the previous cache experiment's three-quarter fill
/// policy, while their class identity now comes exclusively from `SizeClass`.
pub(crate) const fn refill_count(size_class: SizeClass) -> usize {
    magazine_capacity(size_class) * 3 / 4
}

/// Capacity of one magazine for a size class.
pub(crate) const fn magazine_capacity(size_class: SizeClass) -> usize {
    match size_class {
        SizeClass::Bytes8 | SizeClass::Bytes16 => 48,
        SizeClass::Bytes32 => 40,
        SizeClass::Bytes64 => 32,
        SizeClass::Bytes128 => 24,
        SizeClass::Bytes256 => 16,
        SizeClass::Bytes512 => 10,
        SizeClass::Bytes1024 => 8,
        SizeClass::Bytes2048 => 4,
    }
}

#[derive(Clone, Copy)]
struct Magazine<const CAPACITY: usize> {
    objects: [usize; CAPACITY],
    len: u8,
}

impl<const CAPACITY: usize> Magazine<CAPACITY> {
    const fn new() -> Self {
        Self {
            objects: [0; CAPACITY],
            len: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn try_push(&mut self, object: NonNull<u8>) -> Result<(), NonNull<u8>> {
        if self.len as usize == CAPACITY {
            return Err(object);
        }
        self.objects[self.len as usize] = object.as_ptr() as usize;
        self.len += 1;
        Ok(())
    }

    fn pop(&mut self) -> Option<NonNull<u8>> {
        if self.is_empty() {
            return None;
        }
        self.len -= 1;
        let object = self.objects[self.len as usize] as *mut u8;
        // SAFETY: magazine entries enter only through `try_push`, which
        // accepts a `NonNull` pointer.
        Some(unsafe { NonNull::new_unchecked(object) })
    }

    fn take_all(&mut self) -> ObjectBatch {
        let mut batch = ObjectBatch::new();
        while let Some(object) = self.pop() {
            batch.push(object);
        }
        batch
    }
}

#[derive(Clone, Copy)]
struct MagazinePair<const CAPACITY: usize> {
    active: Magazine<CAPACITY>,
    spare: Magazine<CAPACITY>,
}

impl<const CAPACITY: usize> MagazinePair<CAPACITY> {
    const fn new() -> Self {
        Self {
            active: Magazine::new(),
            spare: Magazine::new(),
        }
    }

    fn try_allocate(&mut self) -> Option<NonNull<u8>> {
        if let Some(object) = self.active.pop() {
            return Some(object);
        }
        if self.spare.is_empty() {
            return None;
        }
        mem::swap(&mut self.active, &mut self.spare);
        self.active.pop()
    }

    /// Cache an object, returning a full magazine that must be drained by the
    /// caller outside its per-CPU critical section.
    fn deallocate(&mut self, object: NonNull<u8>) -> Option<ObjectBatch> {
        if self.active.try_push(object).is_ok() {
            return None;
        }
        if self.spare.try_push(object).is_ok() {
            return None;
        }

        let drained = self.spare.take_all();
        debug_assert_eq!(drained.len(), CAPACITY);
        self.spare
            .try_push(object)
            .expect("empty spare magazine must accept a freed object");
        Some(drained)
    }

    /// Install a refill batch into one empty magazine.
    ///
    /// A task can migrate while it obtains a batch from the central cache. If
    /// both magazines on its new CPU became non-empty in the meantime, the
    /// untouched batch is returned so the caller can drain it centrally.
    fn install_refill(&mut self, batch: &mut ObjectBatch) {
        assert!(batch.len() <= CAPACITY, "refill exceeds magazine capacity");
        let destination = if self.active.is_empty() {
            Some(&mut self.active)
        } else if self.spare.is_empty() {
            Some(&mut self.spare)
        } else {
            None
        };
        let Some(destination) = destination else {
            return;
        };

        while let Some(object) = batch.pop() {
            destination
                .try_push(object)
                .expect("empty magazine must accept bounded refill");
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PerCpuMagazineSet {
    bytes8: MagazinePair<48>,
    bytes16: MagazinePair<48>,
    bytes32: MagazinePair<40>,
    bytes64: MagazinePair<32>,
    bytes128: MagazinePair<24>,
    bytes256: MagazinePair<16>,
    bytes512: MagazinePair<10>,
    bytes1024: MagazinePair<8>,
    bytes2048: MagazinePair<4>,
}

macro_rules! with_magazine_pair {
    ($set:expr, $size_class:expr, | $pair:ident | $body:expr) => {
        match $size_class {
            SizeClass::Bytes8 => {
                let $pair = &mut $set.bytes8;
                $body
            }
            SizeClass::Bytes16 => {
                let $pair = &mut $set.bytes16;
                $body
            }
            SizeClass::Bytes32 => {
                let $pair = &mut $set.bytes32;
                $body
            }
            SizeClass::Bytes64 => {
                let $pair = &mut $set.bytes64;
                $body
            }
            SizeClass::Bytes128 => {
                let $pair = &mut $set.bytes128;
                $body
            }
            SizeClass::Bytes256 => {
                let $pair = &mut $set.bytes256;
                $body
            }
            SizeClass::Bytes512 => {
                let $pair = &mut $set.bytes512;
                $body
            }
            SizeClass::Bytes1024 => {
                let $pair = &mut $set.bytes1024;
                $body
            }
            SizeClass::Bytes2048 => {
                let $pair = &mut $set.bytes2048;
                $body
            }
        }
    };
}

impl PerCpuMagazineSet {
    pub(crate) const fn new() -> Self {
        Self {
            bytes8: MagazinePair::new(),
            bytes16: MagazinePair::new(),
            bytes32: MagazinePair::new(),
            bytes64: MagazinePair::new(),
            bytes128: MagazinePair::new(),
            bytes256: MagazinePair::new(),
            bytes512: MagazinePair::new(),
            bytes1024: MagazinePair::new(),
            bytes2048: MagazinePair::new(),
        }
    }

    pub(crate) fn try_allocate(&mut self, size_class: SizeClass) -> Option<NonNull<u8>> {
        with_magazine_pair!(self, size_class, |pair| pair.try_allocate())
    }

    pub(crate) fn deallocate(
        &mut self,
        size_class: SizeClass,
        object: NonNull<u8>,
    ) -> Option<ObjectBatch> {
        with_magazine_pair!(self, size_class, |pair| pair.deallocate(object))
    }

    pub(crate) fn install_refill(&mut self, size_class: SizeClass, batch: &mut ObjectBatch) {
        with_magazine_pair!(self, size_class, |pair| pair.install_refill(batch));
    }
}

#[cfg(unittest)]
#[allow(missing_docs)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::{MagazinePair, ObjectBatch, SizeClass, magazine_capacity, refill_count};

    fn object(address: usize) -> core::ptr::NonNull<u8> {
        core::ptr::NonNull::new(address as *mut u8).unwrap()
    }

    #[def_test]
    fn test_magazine_pair_swaps_spare_for_allocation() {
        let mut magazines = MagazinePair::<2>::new();
        assert!(magazines.deallocate(object(0x1000)).is_none());
        assert!(magazines.deallocate(object(0x2000)).is_none());
        assert!(magazines.deallocate(object(0x3000)).is_none());

        assert_eq!(magazines.try_allocate(), Some(object(0x2000)));
        assert_eq!(magazines.try_allocate(), Some(object(0x1000)));
        assert_eq!(magazines.try_allocate(), Some(object(0x3000)));
        assert_eq!(magazines.try_allocate(), None);
    }

    #[def_test]
    fn test_magazine_pair_returns_full_spare_for_drain() {
        let mut magazines = MagazinePair::<2>::new();
        for address in [0x1000, 0x2000, 0x3000, 0x4000] {
            assert!(magazines.deallocate(object(address)).is_none());
        }

        let mut drained = magazines.deallocate(object(0x5000)).unwrap();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained.pop(), Some(object(0x3000)));
        assert_eq!(drained.pop(), Some(object(0x4000)));
        assert!(drained.is_empty());
    }

    #[def_test]
    fn test_refill_is_bounded_by_the_class_capacity() {
        for size_class in SizeClass::ALL {
            assert!(refill_count(size_class) <= magazine_capacity(size_class));
        }
    }

    #[def_test]
    fn test_install_refill_preserves_uninstallable_batch() {
        let mut magazines = MagazinePair::<2>::new();
        for address in [0x1000, 0x2000, 0x3000, 0x4000] {
            assert!(magazines.deallocate(object(address)).is_none());
        }
        let mut batch = ObjectBatch::new();
        batch.push(object(0x5000));
        magazines.install_refill(&mut batch);
        assert_eq!(batch.pop(), Some(object(0x5000)));
    }
}
