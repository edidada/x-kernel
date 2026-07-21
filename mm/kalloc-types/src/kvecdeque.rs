// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fallible double-ended queue ([`KVecDeque`]) backed by a ring buffer.
//!
//! Provides a FIFO queue for kernel paths that need fallible push/pop
//! semantics (signal pending queues, network buffers, scheduler run queues).
//! The internal buffer is allocated through `kalloc::global_allocator()` so
//! allocation failure returns `AllocError` instead of panicking.

use core::{
    alloc::Layout,
    fmt,
    marker::PhantomData,
    mem::size_of,
    ptr::{self, NonNull},
};

use crate::{AllocContext, AllocError, context::require_supported_context};

const MIN_NON_ZERO_CAPACITY: usize = 4;

/// Allocate a raw buffer for `capacity` elements of type `T`.
fn alloc_buffer<T>(capacity: usize, context: AllocContext) -> Result<NonNull<T>, AllocError> {
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

/// Deallocate a buffer previously allocated with `alloc_buffer`.
unsafe fn dealloc_buffer<T>(ptr: NonNull<T>, capacity: usize) {
    if size_of::<T>() == 0 || capacity == 0 {
        return;
    }
    let layout = Layout::array::<T>(capacity).expect("live buffer layout must be valid");
    kalloc::global_allocator().dealloc(ptr.cast(), layout);
}

/// A fallible double-ended queue backed by a ring buffer.
///
/// Unlike `alloc::collections::VecDeque`, growth is explicit and returns
/// `Result<_, AllocError>` instead of panicking on out-of-memory.
pub struct KVecDeque<T> {
    /// Raw heap buffer. Owns `cap` slots; `[0, cap)` is the allocation.
    ptr: NonNull<T>,
    /// Index of the first valid element (0..cap when non-empty).
    head: usize,
    /// Number of valid elements.
    len: usize,
    /// Total capacity (always >= len).
    cap: usize,
    _marker: PhantomData<T>,
}

impl<T> KVecDeque<T> {
    /// Constructs an empty deque without allocating.
    pub const fn new() -> Self {
        Self {
            ptr: NonNull::dangling(),
            head: 0,
            len: 0,
            cap: if size_of::<T>() == 0 { usize::MAX } else { 0 },
            _marker: PhantomData,
        }
    }

    /// Constructs an empty deque with capacity for at least `capacity` elements.
    ///
    /// Only [`AllocContext::Sleepable`] is supported in phase 1.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when the kernel allocator cannot
    /// satisfy the pre-allocation.
    pub fn try_with_capacity(capacity: usize, context: AllocContext) -> Result<Self, AllocError> {
        let ptr = alloc_buffer::<T>(capacity, context)?;
        Ok(Self {
            ptr,
            head: 0,
            len: 0,
            cap: if size_of::<T>() == 0 {
                usize::MAX
            } else {
                capacity
            },
            _marker: PhantomData,
        })
    }

    /// Returns the number of elements in the deque.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the deque is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the current allocated capacity.
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Computes the physical index of logical position `i` (0..len).
    fn physical_index(&self, logical: usize) -> usize {
        debug_assert!(logical <= self.len);
        if self.cap == 0 || size_of::<T>() == 0 {
            return 0;
        }
        (self.head + logical) % self.cap
    }

    /// Ensure capacity for at least `additional` more elements.
    fn try_reserve(&mut self, additional: usize, context: AllocContext) -> Result<(), AllocError> {
        let required = self
            .len
            .checked_add(additional)
            .ok_or(AllocError::NoMemory)?;
        if required <= self.cap {
            // Enough room already.
            require_supported_context(context)?;
            return Ok(());
        }

        let new_cap = {
            let doubled = self.cap.checked_mul(2).unwrap_or(self.cap);
            required.max(doubled).max(MIN_NON_ZERO_CAPACITY)
        };

        let new_ptr = alloc_buffer::<T>(new_cap, context)?;

        // Copy existing elements contiguously starting at index 0.
        if self.len > 0 && size_of::<T>() != 0 {
            for logical in 0..self.len {
                let src = self.physical_index(logical);
                // SAFETY: src is within the old buffer and initialized;
                // dst is within the new buffer (freshly allocated) at an
                // uninitialised slot.
                unsafe {
                    ptr::copy_nonoverlapping(
                        self.ptr.as_ptr().add(src),
                        new_ptr.as_ptr().add(logical),
                        1,
                    );
                }
            }
        }

        // SAFETY: old buffer is no longer needed; all elements moved out.
        unsafe { dealloc_buffer(self.ptr, self.cap) };

        self.ptr = new_ptr;
        self.head = 0;
        self.cap = new_cap;
        Ok(())
    }

    /// Appends an element to the back of the deque.
    ///
    /// On failure the deque keeps its previous contents. The input `value`
    /// is dropped on **any** error path (OOM or unsupported context) because
    /// this phase-1 API returns only [`AllocError`] rather than an
    /// ownership-carrying error.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when growth is required and the kernel
    /// allocator cannot satisfy it.
    ///
    /// Returns [`AllocError::UnsupportedContext`] when the caller requests an
    /// allocation context reserved for a later phase.
    pub fn try_push_back(&mut self, value: T, context: AllocContext) -> Result<(), AllocError> {
        if self.len == self.cap {
            self.try_reserve(1, context)?;
        } else {
            require_supported_context(context)?;
        }

        let dst = self.physical_index(self.len);
        // SAFETY: dst is within the allocated capacity and currently
        // uninitialised (the slot at logical position `len` is past the
        // last valid element).
        unsafe {
            self.ptr.as_ptr().add(dst).write(value);
        }
        self.len += 1;
        Ok(())
    }

    /// Prepends an element to the front of the deque.
    ///
    /// On failure the deque keeps its previous contents. The input `value`
    /// is dropped on **any** error path (OOM or unsupported context) because
    /// this phase-1 API returns only [`AllocError`] rather than an
    /// ownership-carrying error.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when growth is required.
    ///
    /// Returns [`AllocError::UnsupportedContext`] when the caller requests an
    /// allocation context reserved for a later phase.
    pub fn try_push_front(&mut self, value: T, context: AllocContext) -> Result<(), AllocError> {
        if self.len == self.cap {
            self.try_reserve(1, context)?;
        } else {
            require_supported_context(context)?;
        }

        self.head = if self.head == 0 {
            self.cap - 1
        } else {
            self.head - 1
        };

        // SAFETY: the slot at the new head is within the allocated capacity
        // and previously unoccupied (the ring buffer invariant guarantees
        // the head-1 slot is free when len < cap).
        unsafe {
            self.ptr.as_ptr().add(self.head).write(value);
        }
        self.len += 1;
        Ok(())
    }

    /// Removes and returns the front element, if any.
    pub fn pop_front(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }

        // SAFETY: the element at `head` is initialized (ring buffer
        // invariant: first `len` logical slots are valid).
        let value = unsafe { self.ptr.as_ptr().add(self.head).read() };
        self.head = if self.head + 1 == self.cap {
            0
        } else {
            self.head + 1
        };
        self.len -= 1;
        Some(value)
    }

    /// Removes and returns the back element, if any.
    pub fn pop_back(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }

        let last_logical = self.len - 1;
        let last_physical = self.physical_index(last_logical);

        // SAFETY: the element at the logical last position is initialized.
        let value = unsafe { self.ptr.as_ptr().add(last_physical).read() };
        self.len -= 1;
        Some(value)
    }

    /// Returns the two underlying slices as `(front_contiguous, back_contiguous)`.
    ///
    /// When the buffer does not wrap, `back_contiguous` is empty.
    pub fn as_slices(&self) -> (&[T], &[T]) {
        if self.len == 0 {
            return (&[], &[]);
        }
        let phys_head = self.head;
        let phys_tail = self.physical_index(self.len);
        if phys_head < phys_tail {
            // Single contiguous segment.
            // SAFETY: [head, head+len) is within the buffer and all elements
            // in this range are initialized.
            let front =
                unsafe { core::slice::from_raw_parts(self.ptr.as_ptr().add(phys_head), self.len) };
            (front, &[])
        } else {
            // Wrapped: two segments [head, cap) and [0, tail).
            let front_len = self.cap - phys_head;
            let back_len = phys_tail;
            // SAFETY: both segments are within the buffer and all elements
            // in the covered range are initialized (ring buffer invariant).
            let front =
                unsafe { core::slice::from_raw_parts(self.ptr.as_ptr().add(phys_head), front_len) };
            // SAFETY: the back segment [0, back_len) is at the buffer start;
            // all elements in this range are initialized.
            let back = unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), back_len) };
            (front, back)
        }
    }

    /// Removes all elements while keeping the allocated buffer for reuse.
    pub fn clear(&mut self) {
        while self.pop_front().is_some() {}
    }
}

impl<T: fmt::Debug> fmt::Debug for KVecDeque<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T> Default for KVecDeque<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for KVecDeque<T> {
    fn drop(&mut self) {
        self.clear();
        // SAFETY: all elements have been dropped via clear(). The raw buffer
        // is no longer needed.
        unsafe { dealloc_buffer(self.ptr, self.cap) };
    }
}

// SAFETY: KVecDeque<T> provides exclusive mutable access through &mut self.
// The raw pointer represents owned heap storage. Sending to another thread
// is sound when T: Send.
unsafe impl<T: Send> Send for KVecDeque<T> {}

// SAFETY: shared access only yields &T through iterators. Sharing across
// threads is sound when T: Sync.
unsafe impl<T: Sync> Sync for KVecDeque<T> {}

// -- Iterator support ---------------------------------------------------

/// An iterator over shared references to elements of a [`KVecDeque`].
pub struct Iter<'a, T> {
    deque: &'a KVecDeque<T>,
    pos: usize,
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<&'a T> {
        if self.pos >= self.deque.len {
            return None;
        }
        let idx = self.deque.physical_index(self.pos);
        self.pos += 1;
        // SAFETY: idx points to an initialized element within the buffer.
        Some(unsafe { &*self.deque.ptr.as_ptr().add(idx) })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.deque.len - self.pos;
        (remaining, Some(remaining))
    }
}

impl<T> KVecDeque<T> {
    /// Returns an iterator over shared references.
    pub fn iter(&self) -> Iter<'_, T> {
        Iter {
            deque: self,
            pos: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(unittest)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::def_test;

    use super::*;
    use crate::fault_injection::fail_next_allocation;

    #[derive(Debug)]
    struct DropTracker<'a> {
        drops: &'a AtomicUsize,
    }

    impl Drop for DropTracker<'_> {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[def_test]
    fn new_is_empty() {
        let d = KVecDeque::<u32>::new();
        assert!(d.is_empty());
        assert_eq!(d.len(), 0);
    }

    #[def_test]
    fn try_with_capacity_preallocates() {
        let d = KVecDeque::<u32>::try_with_capacity(8, AllocContext::Sleepable).unwrap();
        assert!(d.is_empty());
        assert!(d.capacity() >= 8);
    }

    #[def_test]
    fn push_back_pop_front_fifo() {
        let mut d = KVecDeque::new();
        d.try_push_back(1_u32, AllocContext::Sleepable).unwrap();
        d.try_push_back(2_u32, AllocContext::Sleepable).unwrap();
        d.try_push_back(3_u32, AllocContext::Sleepable).unwrap();
        assert_eq!(d.len(), 3);
        assert_eq!(d.pop_front(), Some(1));
        assert_eq!(d.pop_front(), Some(2));
        assert_eq!(d.pop_front(), Some(3));
        assert_eq!(d.pop_front(), None);
    }

    #[def_test]
    fn push_front_pop_back_lifo() {
        let mut d = KVecDeque::new();
        d.try_push_front(1_u32, AllocContext::Sleepable).unwrap();
        d.try_push_front(2_u32, AllocContext::Sleepable).unwrap();
        assert_eq!(d.pop_back(), Some(1));
        assert_eq!(d.pop_back(), Some(2));
    }

    #[def_test]
    fn mixed_push_pop() {
        let mut d = KVecDeque::new();
        d.try_push_back(10_u32, AllocContext::Sleepable).unwrap();
        d.try_push_front(20_u32, AllocContext::Sleepable).unwrap();
        d.try_push_back(30_u32, AllocContext::Sleepable).unwrap();
        // deque: [20, 10, 30]
        assert_eq!(d.pop_front(), Some(20));
        assert_eq!(d.pop_back(), Some(30));
        assert_eq!(d.pop_front(), Some(10));
        assert_eq!(d.pop_front(), None);
    }

    #[def_test]
    fn growth_preserves_order() {
        let mut d = KVecDeque::<u32>::new();
        for i in 0..10 {
            d.try_push_back(i, AllocContext::Sleepable).unwrap();
        }
        assert!(d.capacity() >= 10);
        for i in 0..10 {
            assert_eq!(d.pop_front(), Some(i));
        }
    }

    #[def_test]
    fn wrap_around_after_mixed_ops() {
        let mut d = KVecDeque::try_with_capacity(4, AllocContext::Sleepable).unwrap();
        d.try_push_back(1_u32, AllocContext::Sleepable).unwrap();
        d.try_push_back(2_u32, AllocContext::Sleepable).unwrap();
        d.try_push_back(3_u32, AllocContext::Sleepable).unwrap();
        assert_eq!(d.pop_front(), Some(1));
        assert_eq!(d.pop_front(), Some(2));
        // head is now at 2; one element remains at index 2
        d.try_push_back(4_u32, AllocContext::Sleepable).unwrap();
        d.try_push_back(5_u32, AllocContext::Sleepable).unwrap();
        // buffer wraps: [5, ?, 3, 4]  (head=2, len=3)
        assert_eq!(d.pop_front(), Some(3));
        assert_eq!(d.pop_front(), Some(4));
        assert_eq!(d.pop_front(), Some(5));
    }

    #[def_test]
    fn as_slices_no_wrap() {
        let mut d = KVecDeque::try_with_capacity(4, AllocContext::Sleepable).unwrap();
        d.try_push_back(1_u32, AllocContext::Sleepable).unwrap();
        d.try_push_back(2_u32, AllocContext::Sleepable).unwrap();
        let (front, back) = d.as_slices();
        assert_eq!(front, &[1, 2]);
        assert!(back.is_empty());
    }

    #[def_test]
    fn as_slices_wrapped() {
        let mut d = KVecDeque::try_with_capacity(4, AllocContext::Sleepable).unwrap();
        d.try_push_back(1_u32, AllocContext::Sleepable).unwrap();
        d.try_push_back(2_u32, AllocContext::Sleepable).unwrap();
        d.try_push_back(3_u32, AllocContext::Sleepable).unwrap();
        // drain front to create wrap
        assert_eq!(d.pop_front(), Some(1));
        assert_eq!(d.pop_front(), Some(2));
        d.try_push_back(4_u32, AllocContext::Sleepable).unwrap();
        d.try_push_back(5_u32, AllocContext::Sleepable).unwrap();
        // head=2, len=3, elements at physical [2,3,0] = [3,4,5]
        let (front, back) = d.as_slices();
        assert_eq!(front.len() + back.len(), 3);
    }

    #[def_test]
    fn drop_cleans_all_elements() {
        let drops = AtomicUsize::new(0);
        let mut d = KVecDeque::new();
        d.try_push_back(DropTracker { drops: &drops }, AllocContext::Sleepable)
            .unwrap();
        d.try_push_back(DropTracker { drops: &drops }, AllocContext::Sleepable)
            .unwrap();
        drop(d);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[def_test]
    fn clear_drops_elements() {
        let drops = AtomicUsize::new(0);
        let mut d = KVecDeque::new();
        d.try_push_back(DropTracker { drops: &drops }, AllocContext::Sleepable)
            .unwrap();
        d.clear();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(d.is_empty());
    }

    #[def_test]
    fn try_push_back_rejects_unsupported_context() {
        let mut d = KVecDeque::<u32>::new();
        // new deque has cap=0 so push will try to grow
        let err = d.try_push_back(1, AllocContext::Atomic).unwrap_err();
        assert_eq!(err, AllocError::UnsupportedContext);
    }

    #[def_test(serial)]
    fn try_push_back_reports_injected_nomemory() {
        let mut d = KVecDeque::<u32>::try_with_capacity(1, AllocContext::Sleepable).unwrap();
        d.try_push_back(42, AllocContext::Sleepable).unwrap();
        // Deque is full, next push must grow.
        let _guard = fail_next_allocation();
        let err = d.try_push_back(99, AllocContext::Sleepable).unwrap_err();
        assert_eq!(err, AllocError::NoMemory);
        assert_eq!(d.len(), 1);
        assert_eq!(d.pop_front(), Some(42));
    }

    #[def_test]
    fn iterator_yields_in_fifo_order() {
        let mut d = KVecDeque::new();
        d.try_push_back(10_u32, AllocContext::Sleepable).unwrap();
        d.try_push_back(20_u32, AllocContext::Sleepable).unwrap();
        d.try_push_back(30_u32, AllocContext::Sleepable).unwrap();
        let collected: alloc::vec::Vec<_> = d.iter().copied().collect();
        assert_eq!(collected, &[10, 20, 30]);
    }

    #[def_test]
    fn iterator_after_wrap() {
        let mut d = KVecDeque::try_with_capacity(4, AllocContext::Sleepable).unwrap();
        d.try_push_back(1_u32, AllocContext::Sleepable).unwrap();
        d.try_push_back(2_u32, AllocContext::Sleepable).unwrap();
        d.try_push_back(3_u32, AllocContext::Sleepable).unwrap();
        d.pop_front();
        d.pop_front();
        d.try_push_back(4_u32, AllocContext::Sleepable).unwrap();
        d.try_push_back(5_u32, AllocContext::Sleepable).unwrap();
        // head=2, physical: [4,5,?,3]  logical: [3,4,5]
        let collected: alloc::vec::Vec<_> = d.iter().copied().collect();
        assert_eq!(collected, &[3, 4, 5]);
    }

    #[def_test]
    fn zst_operations() {
        let mut d = KVecDeque::<()>::new();
        d.try_push_back((), AllocContext::Sleepable).unwrap();
        d.try_push_back((), AllocContext::Sleepable).unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(d.pop_front(), Some(()));
        assert_eq!(d.pop_front(), Some(()));
        assert_eq!(d.pop_front(), None);
    }
}
