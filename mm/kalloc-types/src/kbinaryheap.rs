// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fallible binary heap ([`KBinaryHeap`]) backed by [`crate::KVec`].
//!
//! The heap owns its storage through `KVec`, so every growth path uses the
//! kernel's fallible allocation contract. Heap reordering itself only swaps
//! already-initialized elements and never allocates.

use core::{cmp::Ord, fmt};

use crate::{AllocContext, AllocError, KVec};

/// A fallible max-priority queue backed by [`KVec`].
///
/// Operations that can grow the backing buffer take an explicit allocation
/// context and return [`AllocError`]. Once capacity has been reserved, pushes
/// are allocation-free and therefore valid in every [`AllocContext`].
pub struct KBinaryHeap<T> {
    inner: KVec<T>,
}

impl<T> KBinaryHeap<T> {
    /// Creates an empty heap without allocating.
    pub const fn new() -> Self {
        Self { inner: KVec::new() }
    }

    /// Creates an empty heap with at least the given capacity.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when the kernel allocator cannot
    /// satisfy the pre-allocation. A zero-capacity heap does not allocate and
    /// is valid in every context.
    pub fn try_with_capacity(capacity: usize, context: AllocContext) -> Result<Self, AllocError> {
        Ok(Self {
            inner: KVec::try_with_capacity(capacity, context)?,
        })
    }

    /// Returns the number of elements in the heap.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns `true` if the heap is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Returns the capacity available before a push must grow the heap.
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    /// Reserves capacity for at least `additional` more elements.
    ///
    /// If current capacity is already sufficient, this operation does not
    /// allocate and accepts every context.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when growth cannot be allocated, or
    /// [`AllocError::UnsupportedContext`] when growth is requested from a
    /// context not yet supported by the allocator.
    pub fn try_reserve(
        &mut self,
        additional: usize,
        context: AllocContext,
    ) -> Result<(), AllocError> {
        self.inner.try_reserve(additional, context)
    }

    /// Removes all elements while preserving the allocated backing buffer.
    pub fn clear(&mut self) {
        self.inner.clear();
    }
}

impl<T: Ord> KBinaryHeap<T> {
    fn sift_up(&mut self, mut child_index: usize) {
        while child_index != 0 {
            let parent_index = (child_index - 1) / 2;
            if self.inner.as_slice()[parent_index] >= self.inner.as_slice()[child_index] {
                break;
            }
            self.inner.as_mut_slice().swap(parent_index, child_index);
            child_index = parent_index;
        }
    }

    fn sift_down(&mut self, mut parent_index: usize) {
        let len = self.inner.len();
        loop {
            let left_child = parent_index.saturating_mul(2).saturating_add(1);
            if left_child >= len {
                return;
            }

            let right_child = left_child.saturating_add(1);
            let largest_child = if right_child < len
                && self.inner.as_slice()[right_child] > self.inner.as_slice()[left_child]
            {
                right_child
            } else {
                left_child
            };

            if self.inner.as_slice()[parent_index] >= self.inner.as_slice()[largest_child] {
                return;
            }
            self.inner.as_mut_slice().swap(parent_index, largest_child);
            parent_index = largest_child;
        }
    }

    /// Pushes an item onto the heap.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when backing-buffer growth is needed
    /// and cannot be allocated. Returns [`AllocError::UnsupportedContext`]
    /// only when the operation would need unsupported-context growth.
    pub fn try_push(&mut self, item: T, context: AllocContext) -> Result<(), AllocError> {
        self.inner.try_push(item, context)?;
        self.sift_up(self.inner.len() - 1);
        Ok(())
    }

    /// Removes and returns the greatest item from the heap, if any.
    pub fn pop(&mut self) -> Option<T> {
        if self.inner.is_empty() {
            return None;
        }

        let item = self.inner.swap_remove(0);
        if !self.inner.is_empty() {
            self.sift_down(0);
        }
        Some(item)
    }

    /// Returns a reference to the greatest item, if any.
    pub fn peek(&self) -> Option<&T> {
        self.inner.as_slice().first()
    }
}

impl<T> Default for KBinaryHeap<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: fmt::Debug> fmt::Debug for KBinaryHeap<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.inner.iter()).finish()
    }
}

// SAFETY: `KBinaryHeap` owns its `KVec` exclusively. It can be moved between
// threads when its elements can be moved between threads.
unsafe impl<T: Send> Send for KBinaryHeap<T> {}

// SAFETY: shared access exposes only shared references to the initialized
// heap elements, so `T: Sync` is sufficient.
unsafe impl<T: Sync> Sync for KBinaryHeap<T> {}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;
    use crate::fault_injection::fail_next_allocation;

    #[def_test]
    fn new_is_empty() {
        let heap = KBinaryHeap::<u32>::new();
        assert!(heap.is_empty());
        assert_eq!(heap.len(), 0);
    }

    #[def_test]
    fn try_push_and_pop_max() {
        let mut heap = KBinaryHeap::new();
        heap.try_push(3, AllocContext::Sleepable).unwrap();
        heap.try_push(1, AllocContext::Sleepable).unwrap();
        heap.try_push(2, AllocContext::Sleepable).unwrap();
        assert_eq!(heap.len(), 3);
        assert_eq!(heap.pop(), Some(3));
        assert_eq!(heap.pop(), Some(2));
        assert_eq!(heap.pop(), Some(1));
        assert_eq!(heap.pop(), None);
    }

    #[def_test]
    fn peek_does_not_remove() {
        let mut heap = KBinaryHeap::new();
        heap.try_push(5, AllocContext::Sleepable).unwrap();
        heap.try_push(9, AllocContext::Sleepable).unwrap();
        assert_eq!(heap.peek(), Some(&9));
        assert_eq!(heap.len(), 2);
    }

    #[def_test]
    fn clear_removes_all() {
        let mut heap = KBinaryHeap::new();
        heap.try_push(1, AllocContext::Sleepable).unwrap();
        heap.try_push(2, AllocContext::Sleepable).unwrap();
        heap.clear();
        assert!(heap.is_empty());
    }

    #[def_test]
    fn preallocated_push_is_valid_in_atomic_context() {
        let mut heap = KBinaryHeap::try_with_capacity(2, AllocContext::Sleepable).unwrap();
        heap.try_push(1, AllocContext::Atomic).unwrap();
        heap.try_push(2, AllocContext::Atomic).unwrap();
        assert_eq!(heap.pop(), Some(2));
    }

    #[def_test]
    fn growth_rejects_unsupported_context() {
        let mut heap = KBinaryHeap::<u32>::new();
        let err = heap.try_push(1, AllocContext::Atomic).unwrap_err();
        assert_eq!(err, AllocError::UnsupportedContext);
    }

    #[def_test]
    fn try_with_capacity_preallocates() {
        let heap = KBinaryHeap::<u32>::try_with_capacity(10, AllocContext::Sleepable).unwrap();
        assert!(heap.capacity() >= 10);
    }

    #[def_test(serial)]
    fn try_push_growth_failure_preserves_heap() {
        let mut heap = KBinaryHeap::new();
        let _guard = fail_next_allocation();

        let err = heap.try_push(7_u32, AllocContext::Sleepable).unwrap_err();

        assert_eq!(err, AllocError::NoMemory);
        assert!(heap.is_empty());
        assert_eq!(heap.peek(), None);
    }
}
