// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    alloc::Layout,
    fmt,
    marker::PhantomData,
    mem::{ManuallyDrop, size_of},
    ops::{Deref, DerefMut},
    ptr::{self, NonNull},
    slice,
};

use crate::{AllocContext, AllocError, context::require_supported_context};

const MIN_NON_ZERO_CAPACITY: usize = 4;

fn alloc_vec_buffer<T>(capacity: usize, context: AllocContext) -> Result<NonNull<T>, AllocError> {
    require_supported_context(context)?;

    if capacity == 0 || size_of::<T>() == 0 {
        return Ok(NonNull::dangling());
    }

    #[cfg(unittest)]
    crate::fault_injection::maybe_fail_allocation()?;

    let layout = Layout::array::<T>(capacity).map_err(|_| AllocError::NoMemory)?;
    let ptr = kalloc::global_allocator()
        .alloc(layout)
        .map_err(|_| AllocError::NoMemory)?;
    Ok(ptr.cast())
}

/// Growable heap buffer with explicit fallible allocation entry points.
///
/// Unlike `Vec<T>`, this type does not route growth through the infallible
/// global-allocation traits. It owns its raw buffer directly and grows via
/// `kalloc::global_allocator()`, so allocation failure can stay in
/// `Result<_, AllocError>` instead of terminating the kernel.
pub struct KVec<T> {
    ptr: NonNull<T>,
    len: usize,
    cap: usize,
    _marker: PhantomData<T>,
}

/// Owned iterator over a [`KVec`].
pub struct KVecIntoIter<T> {
    ptr: NonNull<T>,
    next: usize,
    len: usize,
    cap: usize,
    _marker: PhantomData<T>,
}

impl<T: fmt::Debug> fmt::Debug for KVec<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

// SAFETY: `KVec<T>` provides exclusive mutable access to its elements through
// `&mut self` and does not share internal aliasing state across threads. Its
// raw pointer only represents owned heap storage, so sending the container to
// another thread is sound whenever `T` itself is `Send`.
unsafe impl<T: Send> Send for KVec<T> {}

// SAFETY: shared access to `KVec<T>` only yields shared references to the
// initialized prefix, and mutation still requires exclusive `&mut self`.
// Therefore sharing `&KVec<T>` across threads is sound whenever element
// sharing through `&T` is sound, i.e. when `T: Sync`.
unsafe impl<T: Sync> Sync for KVec<T> {}

impl<T> Default for KVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> KVec<T> {
    /// Constructs an empty vector without allocating.
    pub const fn new() -> Self {
        Self {
            ptr: NonNull::dangling(),
            len: 0,
            cap: if size_of::<T>() == 0 { usize::MAX } else { 0 },
            _marker: PhantomData,
        }
    }

    /// Constructs an empty vector with capacity for at least `capacity`
    /// elements.
    ///
    /// Only [`AllocContext::Sleepable`] is supported in phase 1.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when the kernel allocator cannot
    /// satisfy the requested pre-allocation.
    ///
    /// Returns [`AllocError::UnsupportedContext`] when the caller requests an
    /// allocation context reserved for a later phase.
    pub fn try_with_capacity(capacity: usize, context: AllocContext) -> Result<Self, AllocError> {
        let ptr = alloc_vec_buffer::<T>(capacity, context)?;
        Ok(Self {
            ptr,
            len: 0,
            cap: if size_of::<T>() == 0 {
                usize::MAX
            } else {
                capacity
            },
            _marker: PhantomData,
        })
    }

    /// Returns the current logical length.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns the currently available capacity.
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Returns whether the vector is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns a shared slice view of the stored elements.
    pub fn as_slice(&self) -> &[T] {
        // SAFETY: The first `self.len` elements are always initialized by the
        // type invariant, and `self.ptr` either points to a live allocation for
        // `self.cap` elements or is a dangling sentinel when `self.len == 0`.
        unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// Returns a mutable slice view of the stored elements.
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: The first `self.len` elements are initialized and owned
        // exclusively by `self`, so producing a mutable slice over exactly that
        // range is valid.
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    /// Returns an iterator over shared references to the stored elements.
    pub fn iter(&self) -> slice::Iter<'_, T> {
        self.as_slice().iter()
    }

    /// Returns an iterator over mutable references to the stored elements.
    pub fn iter_mut(&mut self) -> slice::IterMut<'_, T> {
        self.as_mut_slice().iter_mut()
    }

    fn next_capacity(&self, required: usize) -> Result<usize, AllocError> {
        if size_of::<T>() == 0 {
            return Ok(usize::MAX);
        }

        let doubled = self.cap.checked_mul(2).unwrap_or(self.cap);
        Ok(required.max(doubled).max(MIN_NON_ZERO_CAPACITY))
    }

    fn dealloc_buffer(&mut self) {
        if size_of::<T>() == 0 || self.cap == 0 {
            return;
        }

        let layout = Layout::array::<T>(self.cap).expect("live KVec layout must be valid");
        kalloc::global_allocator().dealloc(self.ptr.cast(), layout);
    }

    /// Ensures that the vector can grow by at least `additional` elements.
    ///
    /// On failure the vector keeps its previous contents and length.
    ///
    /// Only [`AllocContext::Sleepable`] is supported in phase 1.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when the kernel allocator cannot
    /// satisfy the requested growth.
    ///
    /// Returns [`AllocError::UnsupportedContext`] when the caller requests an
    /// allocation context reserved for a later phase.
    pub fn try_reserve(
        &mut self,
        additional: usize,
        context: AllocContext,
    ) -> Result<(), AllocError> {
        require_supported_context(context)?;

        let required = self
            .len
            .checked_add(additional)
            .ok_or(AllocError::NoMemory)?;
        if required <= self.cap {
            return Ok(());
        }

        let new_cap = self.next_capacity(required)?;
        let new_ptr = alloc_vec_buffer::<T>(new_cap, context)?;

        if self.len != 0 && size_of::<T>() != 0 {
            // SAFETY: `new_ptr` is a fresh allocation for at least `new_cap`
            // elements, `self.ptr` currently owns at least `self.len`
            // initialized elements, and the two allocations do not overlap.
            // Copying exactly `self.len` elements preserves the old logical
            // contents before the old buffer is released.
            unsafe {
                ptr::copy_nonoverlapping(self.ptr.as_ptr(), new_ptr.as_ptr(), self.len);
            }
        }

        self.dealloc_buffer();
        self.ptr = new_ptr;
        self.cap = new_cap;
        Ok(())
    }

    /// Appends one value, growing the vector explicitly if needed.
    ///
    /// On failure the vector keeps its previous contents and length. The input
    /// `value` is dropped if capacity growth fails because this phase-1 API
    /// returns only [`AllocError`] rather than an ownership-carrying error.
    ///
    /// Only [`AllocContext::Sleepable`] is supported in phase 1.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when growth is required and the kernel
    /// allocator cannot satisfy it.
    ///
    /// Returns [`AllocError::UnsupportedContext`] when the caller requests an
    /// allocation context reserved for a later phase.
    pub fn try_push(&mut self, value: T, context: AllocContext) -> Result<(), AllocError> {
        if self.len == self.cap {
            self.try_reserve(1, context)?;
        } else {
            require_supported_context(context)?;
        }

        // SAFETY: `self.try_reserve` above ensured spare capacity for at least
        // one more element, so the slot at `self.len` is in-bounds and not yet
        // initialized. `self` exclusively owns the buffer.
        unsafe {
            self.ptr.as_ptr().add(self.len).write(value);
        }
        self.len += 1;
        Ok(())
    }

    /// Removes the last element and returns it, if any.
    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }

        self.len -= 1;
        // SAFETY: The slot at the old tail index was initialized by the KVec
        // invariant. After decrementing `len`, reading that element transfers
        // ownership out and leaves the slot logically uninitialized again.
        Some(unsafe { self.ptr.as_ptr().add(self.len).read() })
    }

    /// Shortens the vector to `new_len`, dropping removed elements.
    pub fn truncate(&mut self, new_len: usize) {
        if new_len >= self.len {
            return;
        }

        let tail_len = self.len - new_len;
        // SAFETY: Elements in the range `[new_len, self.len)` are initialized
        // and exclusively owned by `self`. Dropping exactly that tail range is
        // valid and preserves the initialized-prefix invariant for the new
        // length.
        unsafe {
            ptr::drop_in_place(ptr::slice_from_raw_parts_mut(
                self.ptr.as_ptr().add(new_len),
                tail_len,
            ));
        }
        self.len = new_len;
    }

    /// Removes all elements while keeping the allocated buffer for reuse.
    pub fn clear(&mut self) {
        self.truncate(0);
    }

    /// Removes and returns the element at `index`, replacing it with the last
    /// element.
    pub fn swap_remove(&mut self, index: usize) -> T {
        assert!(index < self.len, "swap_remove index out of bounds");

        let last_index = self.len - 1;
        // SAFETY: `index < self.len` guarantees `index` refers to an
        // initialized element. Reading it transfers ownership out of the slot.
        let removed = unsafe { self.ptr.as_ptr().add(index).read() };

        if index != last_index {
            // SAFETY: `last_index` is also within the initialized prefix.
            // Reading the last element moves it out, and writing it into
            // `index` re-initializes the hole left by the removed value.
            unsafe {
                let last = self.ptr.as_ptr().add(last_index).read();
                self.ptr.as_ptr().add(index).write(last);
            }
        }

        self.len = last_index;
        removed
    }
}

impl<T: Copy> KVec<T> {
    /// Appends a copied slice, growing the vector explicitly if needed.
    ///
    /// On failure the vector keeps its previous contents and length.
    ///
    /// Only [`AllocContext::Sleepable`] is supported in phase 1.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when growth is required and the kernel
    /// allocator cannot satisfy it.
    ///
    /// Returns [`AllocError::UnsupportedContext`] when the caller requests an
    /// allocation context reserved for a later phase.
    pub fn try_extend_from_slice(
        &mut self,
        values: &[T],
        context: AllocContext,
    ) -> Result<(), AllocError> {
        if values.len() > self.cap - self.len {
            self.try_reserve(values.len(), context)?;
        } else {
            require_supported_context(context)?;
        }

        if values.is_empty() {
            return Ok(());
        }

        // SAFETY: `try_reserve` above guaranteed enough spare capacity for
        // `values.len()` more elements. The source slice is valid for reads,
        // the destination range starts at the first uninitialized slot in the
        // owned buffer, and the regions cannot overlap because the source
        // lives outside this vector allocation.
        unsafe {
            ptr::copy_nonoverlapping(
                values.as_ptr(),
                self.ptr.as_ptr().add(self.len),
                values.len(),
            );
        }
        self.len += values.len();
        Ok(())
    }
}

impl<T: Clone> KVec<T> {
    /// Appends elements cloned from `values`, growing the vector if needed.
    ///
    /// Each element is individually cloned via `T::clone`. On failure the
    /// vector keeps its previous contents and length.
    ///
    /// Only [`AllocContext::Sleepable`] is supported in phase 1.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when growth is required and the kernel
    /// allocator cannot satisfy it.
    ///
    /// Returns [`AllocError::UnsupportedContext`] when the caller requests an
    /// allocation context reserved for a later phase.
    ///
    /// # Panics
    ///
    /// If a `T::clone` call panics, already-cloned elements are dropped before
    /// the panic propagates, and the vector remains in its previous state.
    pub fn try_extend_cloned_from_slice(
        &mut self,
        values: &[T],
        context: AllocContext,
    ) -> Result<(), AllocError> {
        if values.len() > self.cap - self.len {
            self.try_reserve(values.len(), context)?;
        } else {
            require_supported_context(context)?;
        }

        if values.is_empty() {
            return Ok(());
        }

        // Guard: on panic during cloning, drop the elements we've already
        // written and restore the old length.
        struct CloneGuard<'a, T> {
            vec: &'a mut KVec<T>,
            start: usize,
            cloned: usize,
        }

        impl<T> Drop for CloneGuard<'_, T> {
            fn drop(&mut self) {
                // SAFETY: The range [start, start+cloned) contains
                // initialized elements that were written by the clone loop.
                // Dropping them and resetting len preserves the KVec
                // invariant (only elements in [0, len) are initialized).
                unsafe {
                    core::ptr::drop_in_place(core::ptr::slice_from_raw_parts_mut(
                        self.vec.ptr.as_ptr().add(self.start),
                        self.cloned,
                    ));
                }
                // The caller's panic will prevent reaching the post-loop
                // len update; restoring old len means the vector looks
                // exactly like it did before try_extend_cloned_from_slice.
            }
        }

        let start = self.len;
        let mut guard = CloneGuard {
            vec: self,
            start,
            cloned: 0,
        };

        for value in values {
            // SAFETY: try_reserve above ensured capacity for at least
            // `values.len()` more elements. The slot at start+cloned is
            // within the allocation and currently uninitialized.
            unsafe {
                guard
                    .vec
                    .ptr
                    .as_ptr()
                    .add(start + guard.cloned)
                    .write(value.clone());
            }
            guard.cloned += 1;
        }

        // All clones succeeded. Disarm the guard.
        let cloned_count = guard.cloned;
        core::mem::forget(guard);

        // SAFETY: The cloned_count elements written above are now fully
        // owned by the vector. Advancing len brings them into the
        // initialized prefix, satisfying the KVec invariant.
        unsafe {
            // Reborrow self to get past the guard borrow.
            let this: &mut KVec<T> = &mut *(self as *mut KVec<T>);
            this.len += cloned_count;
        }

        Ok(())
    }
}

impl<T> Drop for KVec<T> {
    fn drop(&mut self) {
        if self.len != 0 {
            // SAFETY: The first `self.len` elements are initialized by the
            // `KVec` invariant and exclusively owned by `self`, so dropping
            // that exact slice is valid before releasing the buffer.
            unsafe {
                ptr::drop_in_place(ptr::slice_from_raw_parts_mut(self.ptr.as_ptr(), self.len));
            }
        }
        self.dealloc_buffer();
    }
}

impl<T> AsRef<[T]> for KVec<T> {
    fn as_ref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T> AsMut<[T]> for KVec<T> {
    fn as_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T> Deref for KVec<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

impl<T> DerefMut for KVec<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.as_mut_slice()
    }
}

impl<T> Iterator for KVecIntoIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next == self.len {
            return None;
        }

        let index = self.next;
        self.next += 1;
        // SAFETY: `index < self.len` guarantees this slot is within the owned
        // initialized prefix. Advancing `next` transfers ownership of exactly
        // one element out of the iterator.
        Some(unsafe { self.ptr.as_ptr().add(index).read() })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.len - self.next;
        (remaining, Some(remaining))
    }
}

impl<T> ExactSizeIterator for KVecIntoIter<T> {}

impl<T> Drop for KVecIntoIter<T> {
    fn drop(&mut self) {
        if self.next != self.len {
            // SAFETY: Elements in the remaining initialized suffix
            // `[self.next, self.len)` still belong to this iterator and have
            // not been yielded. Dropping exactly that range preserves RAII.
            unsafe {
                ptr::drop_in_place(ptr::slice_from_raw_parts_mut(
                    self.ptr.as_ptr().add(self.next),
                    self.len - self.next,
                ));
            }
        }

        if size_of::<T>() == 0 || self.cap == 0 {
            return;
        }

        let layout = Layout::array::<T>(self.cap).expect("live KVec layout must be valid");
        kalloc::global_allocator().dealloc(self.ptr.cast(), layout);
    }
}

impl<T> IntoIterator for KVec<T> {
    type IntoIter = KVecIntoIter<T>;
    type Item = T;

    fn into_iter(self) -> Self::IntoIter {
        let vec = ManuallyDrop::new(self);
        KVecIntoIter {
            ptr: vec.ptr,
            next: 0,
            len: vec.len,
            cap: vec.cap,
            _marker: PhantomData,
        }
    }
}

#[cfg(unittest)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::def_test;

    use super::{AllocContext, AllocError, KVec};
    use crate::fault_injection::{fail_allocations_after, fail_next_allocation};

    #[derive(Debug)]
    struct DropTracker<'a> {
        id: usize,
        drops: &'a AtomicUsize,
        sum: &'a AtomicUsize,
    }

    impl Drop for DropTracker<'_> {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            self.sum.fetch_add(self.id, Ordering::SeqCst);
        }
    }

    #[def_test]
    fn default_vector_is_empty() {
        let vec = KVec::<u32>::default();
        assert!(vec.is_empty());
        assert_eq!(vec.len(), 0);
        assert_eq!(vec.as_slice(), &[]);
    }

    #[def_test]
    fn new_vector_is_empty_without_capacity() {
        let vec = KVec::<u32>::new();
        assert!(vec.is_empty());
        assert_eq!(vec.len(), 0);
        assert_eq!(vec.capacity(), 0);
    }

    #[def_test]
    fn try_with_capacity_allocates_requested_space() {
        let vec = KVec::<u32>::try_with_capacity(4, AllocContext::Sleepable).unwrap();
        assert_eq!(vec.len(), 0);
        assert!(vec.capacity() >= 4);
    }

    #[def_test]
    fn try_push_appends_and_grows() {
        let mut vec = KVec::try_with_capacity(1, AllocContext::Sleepable).unwrap();
        vec.try_push(3_u32, AllocContext::Sleepable).unwrap();
        vec.try_push(5_u32, AllocContext::Sleepable).unwrap();
        vec.try_push(8_u32, AllocContext::Sleepable).unwrap();
        assert_eq!(vec.as_slice(), &[3, 5, 8]);
        assert!(vec.capacity() >= vec.len());
    }

    #[def_test]
    fn try_extend_from_slice_appends_bulk() {
        let mut vec = KVec::try_with_capacity(1, AllocContext::Sleepable).unwrap();
        vec.try_push(3_u32, AllocContext::Sleepable).unwrap();
        vec.try_extend_from_slice(&[5_u32, 8_u32, 13_u32], AllocContext::Sleepable)
            .unwrap();
        assert_eq!(vec.as_slice(), &[3, 5, 8, 13]);
        assert!(vec.capacity() >= vec.len());
    }

    #[def_test]
    fn try_with_capacity_rejects_unsupported_context() {
        let err = KVec::<u32>::try_with_capacity(4, AllocContext::Atomic).unwrap_err();
        assert_eq!(err, AllocError::UnsupportedContext);
    }

    #[def_test]
    fn try_reserve_rejects_unsupported_context_without_mutation() {
        let mut vec = KVec::<u32>::new();
        let err = vec.try_reserve(4, AllocContext::NoWait).unwrap_err();
        assert_eq!(err, AllocError::UnsupportedContext);
        assert!(vec.is_empty());
        assert_eq!(vec.capacity(), 0);
    }

    #[def_test]
    fn try_push_rejects_unsupported_context_without_mutation() {
        let mut vec = KVec::<u32>::new();
        let err = vec.try_push(9, AllocContext::ReservedCritical).unwrap_err();
        assert_eq!(err, AllocError::UnsupportedContext);
        assert!(vec.is_empty());
        assert_eq!(vec.as_slice(), &[]);
    }

    #[def_test]
    fn try_extend_from_slice_rejects_unsupported_context_without_mutation() {
        let mut vec = KVec::<u32>::new();
        let err = vec
            .try_extend_from_slice(&[9_u32, 10_u32], AllocContext::ReservedCritical)
            .unwrap_err();
        assert_eq!(err, AllocError::UnsupportedContext);
        assert!(vec.is_empty());
        assert_eq!(vec.as_slice(), &[]);
    }

    #[def_test(serial)]
    fn try_reserve_reports_injected_nomemory_without_mutation() {
        let mut vec = KVec::try_with_capacity(1, AllocContext::Sleepable).unwrap();
        vec.try_push(3_u32, AllocContext::Sleepable).unwrap();
        let before = vec.as_slice().to_vec();
        let old_capacity = vec.capacity();

        let _guard = fail_next_allocation();
        let err = vec
            .try_reserve(old_capacity, AllocContext::Sleepable)
            .unwrap_err();
        assert_eq!(err, AllocError::NoMemory);
        assert_eq!(vec.as_slice(), before.as_slice());
        assert_eq!(vec.capacity(), old_capacity);
    }

    #[def_test]
    fn pop_returns_last_element() {
        let mut vec = KVec::new();
        vec.try_push(3_u32, AllocContext::Sleepable).unwrap();
        vec.try_push(5_u32, AllocContext::Sleepable).unwrap();
        assert_eq!(vec.pop(), Some(5));
        assert_eq!(vec.pop(), Some(3));
        assert_eq!(vec.pop(), None);
    }

    #[def_test]
    fn swap_remove_keeps_remaining_elements_valid() {
        let mut vec = KVec::new();
        vec.try_push(10_u32, AllocContext::Sleepable).unwrap();
        vec.try_push(20_u32, AllocContext::Sleepable).unwrap();
        vec.try_push(30_u32, AllocContext::Sleepable).unwrap();
        assert_eq!(vec.swap_remove(1), 20);
        assert_eq!(vec.as_slice(), &[10, 30]);
    }

    #[def_test]
    fn truncate_drops_removed_tail_only() {
        let drop_count = AtomicUsize::new(0);
        let drop_sum = AtomicUsize::new(0);
        let mut vec = KVec::new();
        vec.try_push(
            DropTracker {
                id: 1,
                drops: &drop_count,
                sum: &drop_sum,
            },
            AllocContext::Sleepable,
        )
        .unwrap();
        vec.try_push(
            DropTracker {
                id: 2,
                drops: &drop_count,
                sum: &drop_sum,
            },
            AllocContext::Sleepable,
        )
        .unwrap();
        vec.try_push(
            DropTracker {
                id: 3,
                drops: &drop_count,
                sum: &drop_sum,
            },
            AllocContext::Sleepable,
        )
        .unwrap();

        vec.truncate(1);
        assert_eq!(drop_count.load(Ordering::SeqCst), 2);
        assert_eq!(drop_sum.load(Ordering::SeqCst), 5);
        drop(vec);
        assert_eq!(drop_count.load(Ordering::SeqCst), 3);
        assert_eq!(drop_sum.load(Ordering::SeqCst), 6);
    }

    #[def_test]
    fn clear_preserves_capacity_for_reuse() {
        let mut vec = KVec::try_with_capacity(4, AllocContext::Sleepable).unwrap();
        let initial_capacity = vec.capacity();
        vec.try_push(1_u32, AllocContext::Sleepable).unwrap();
        vec.try_push(2_u32, AllocContext::Sleepable).unwrap();
        vec.clear();
        assert!(vec.is_empty());
        assert_eq!(vec.capacity(), initial_capacity);
    }

    #[def_test]
    fn zst_push_and_pop_keep_logical_length() {
        let mut vec = KVec::<()>::new();
        vec.try_push((), AllocContext::Sleepable).unwrap();
        vec.try_push((), AllocContext::Sleepable).unwrap();
        assert_eq!(vec.len(), 2);
        assert_eq!(vec.pop(), Some(()));
        assert_eq!(vec.pop(), Some(()));
        assert_eq!(vec.pop(), None);
    }

    #[def_test(serial)]
    fn try_push_failure_drops_input_and_preserves_old_contents() {
        let drop_count = AtomicUsize::new(0);
        let drop_sum = AtomicUsize::new(0);
        let mut vec = KVec::try_with_capacity(1, AllocContext::Sleepable).unwrap();
        vec.try_push(
            DropTracker {
                id: 7,
                drops: &drop_count,
                sum: &drop_sum,
            },
            AllocContext::Sleepable,
        )
        .unwrap();
        let old_capacity = vec.capacity();

        let _guard = fail_next_allocation();
        let err = vec
            .try_push(
                DropTracker {
                    id: 9,
                    drops: &drop_count,
                    sum: &drop_sum,
                },
                AllocContext::Sleepable,
            )
            .unwrap_err();
        assert_eq!(err, AllocError::NoMemory);
        assert_eq!(drop_count.load(Ordering::SeqCst), 1);
        assert_eq!(drop_sum.load(Ordering::SeqCst), 9);
        assert_eq!(vec.len(), 1);
        assert_eq!(vec.as_slice()[0].id, 7);
        assert_eq!(vec.capacity(), old_capacity);
    }

    #[def_test]
    fn into_iter_yields_values_in_order() {
        let mut vec = KVec::new();
        vec.try_push(1_u32, AllocContext::Sleepable).unwrap();
        vec.try_push(2_u32, AllocContext::Sleepable).unwrap();
        vec.try_push(3_u32, AllocContext::Sleepable).unwrap();
        let mut iter = vec.into_iter();
        assert_eq!(iter.size_hint(), (3, Some(3)));
        assert_eq!(iter.next(), Some(1));
        assert_eq!(iter.next(), Some(2));
        assert_eq!(iter.next(), Some(3));
        assert_eq!(iter.next(), None);
    }

    #[def_test]
    fn into_iter_drop_cleans_remaining_elements() {
        let drop_count = AtomicUsize::new(0);
        let drop_sum = AtomicUsize::new(0);
        let mut vec = KVec::new();
        vec.try_push(
            DropTracker {
                id: 1,
                drops: &drop_count,
                sum: &drop_sum,
            },
            AllocContext::Sleepable,
        )
        .unwrap();
        vec.try_push(
            DropTracker {
                id: 2,
                drops: &drop_count,
                sum: &drop_sum,
            },
            AllocContext::Sleepable,
        )
        .unwrap();
        vec.try_push(
            DropTracker {
                id: 3,
                drops: &drop_count,
                sum: &drop_sum,
            },
            AllocContext::Sleepable,
        )
        .unwrap();

        let mut iter = vec.into_iter();
        let first = iter.next().unwrap();
        assert_eq!(first.id, 1);
        drop(first);
        drop(iter);

        assert_eq!(drop_count.load(Ordering::SeqCst), 3);
        assert_eq!(drop_sum.load(Ordering::SeqCst), 6);
    }

    #[def_test(serial)]
    fn injected_growth_failure_keeps_existing_prefix() {
        let mut vec = KVec::new();
        for value in 0_u32..4 {
            vec.try_push(value, AllocContext::Sleepable).unwrap();
        }

        let _guard = fail_allocations_after(0);
        let err = vec.try_push(99, AllocContext::Sleepable).unwrap_err();
        assert_eq!(err, AllocError::NoMemory);
        assert_eq!(vec.as_slice(), &[0, 1, 2, 3]);
    }

    #[def_test(serial)]
    fn extend_from_slice_growth_failure_keeps_existing_prefix() {
        let mut vec = KVec::try_with_capacity(1, AllocContext::Sleepable).unwrap();
        vec.try_push(3_u32, AllocContext::Sleepable).unwrap();

        let _guard = fail_next_allocation();
        let err = vec
            .try_extend_from_slice(&[5_u32, 8_u32], AllocContext::Sleepable)
            .unwrap_err();
        assert_eq!(err, AllocError::NoMemory);
        assert_eq!(vec.as_slice(), &[3]);
    }

    // -- try_extend_cloned_from_slice -----------------------------------

    #[derive(Debug, Clone)]
    struct CloneTracker<'a> {
        id: usize,
        clones: &'a AtomicUsize,
        drops: &'a AtomicUsize,
    }

    impl Drop for CloneTracker<'_> {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Clone for DropTracker<'_> {
        fn clone(&self) -> Self {
            Self {
                id: self.id,
                drops: self.drops,
                sum: self.sum,
            }
        }
    }

    #[def_test]
    fn try_extend_cloned_into_empty() {
        let clone_count = AtomicUsize::new(0);
        let drop_count = AtomicUsize::new(0);

        let src = crate::kvec![
            CloneTracker {
                id: 1,
                clones: &clone_count,
                drops: &drop_count,
            },
            CloneTracker {
                id: 2,
                clones: &clone_count,
                drops: &drop_count,
            },
        ]
        .unwrap();

        let mut vec = KVec::try_with_capacity(2, AllocContext::Sleepable).unwrap();
        vec.try_extend_cloned_from_slice(&src, AllocContext::Sleepable)
            .unwrap();
        assert_eq!(vec.len(), 2);
        assert_eq!(vec.as_slice()[0].id, 1);
        assert_eq!(vec.as_slice()[1].id, 2);

        drop(vec);
        assert_eq!(drop_count.load(Ordering::SeqCst), 2);
    }

    #[def_test]
    fn try_extend_cloned_appends_to_existing() {
        let clone_count = AtomicUsize::new(0);
        let drop_count = AtomicUsize::new(0);

        let mut vec = KVec::try_with_capacity(4, AllocContext::Sleepable).unwrap();
        vec.try_push(
            CloneTracker {
                id: 10,
                clones: &clone_count,
                drops: &drop_count,
            },
            AllocContext::Sleepable,
        )
        .unwrap();

        let src = crate::kvec![
            CloneTracker {
                id: 20,
                clones: &clone_count,
                drops: &drop_count,
            },
            CloneTracker {
                id: 30,
                clones: &clone_count,
                drops: &drop_count,
            },
        ]
        .unwrap();
        vec.try_extend_cloned_from_slice(&src, AllocContext::Sleepable)
            .unwrap();

        assert_eq!(vec.len(), 3);
        assert_eq!(vec.as_slice()[0].id, 10);
        assert_eq!(vec.as_slice()[1].id, 20);
        assert_eq!(vec.as_slice()[2].id, 30);
    }

    #[def_test]
    fn try_extend_cloned_empty_is_noop() {
        let mut vec = KVec::try_with_capacity(4, AllocContext::Sleepable).unwrap();
        vec.try_push(1_u32, AllocContext::Sleepable).unwrap();
        let empty: &[u32] = &[];
        vec.try_extend_cloned_from_slice(empty, AllocContext::Sleepable)
            .unwrap();
        assert_eq!(vec.len(), 1);
        assert_eq!(vec.as_slice(), &[1]);
    }

    #[def_test(serial)]
    fn try_extend_cloned_growth_failure_preserves_content() {
        let mut vec = KVec::try_with_capacity(1, AllocContext::Sleepable).unwrap();
        vec.try_push(1_u32, AllocContext::Sleepable).unwrap();

        let _guard = fail_next_allocation();
        let err = vec
            .try_extend_cloned_from_slice(&[2_u32, 3_u32], AllocContext::Sleepable)
            .unwrap_err();
        assert_eq!(err, AllocError::NoMemory);
        // The original content must be intact.
        assert_eq!(vec.as_slice(), &[1]);
        assert_eq!(vec.len(), 1);
    }
}
