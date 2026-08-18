// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fallible binary heap ([`KBinaryHeap`]) backed by `alloc::collections::BinaryHeap`.
//!
//! Wraps the underlying heap and provides `try_push` that reserves capacity
//! before insertion. Allocation failure returns [`AllocError`] instead of
//! panicking.

use alloc::collections::BinaryHeap;
use core::{cmp::Ord, fmt};

use crate::{AllocContext, AllocError, context::require_supported_context};

/// A fallible priority queue backed by `BinaryHeap`.
///
/// All operations that may allocate (push) take an explicit [`AllocContext`]
/// and return `Result`.
pub struct KBinaryHeap<T> {
    inner: BinaryHeap<T>,
}

impl<T> KBinaryHeap<T> {
    /// Creates an empty heap without allocating.
    pub fn new() -> Self {
        Self {
            inner: BinaryHeap::new(),
        }
    }

    /// Creates an empty heap with at least the given capacity.
    ///
    /// Only [`AllocContext::Sleepable`] is supported in phase 1.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when the kernel allocator cannot
    /// satisfy the pre-allocation.
    pub fn try_with_capacity(capacity: usize, context: AllocContext) -> Result<Self, AllocError> {
        require_supported_context(context)?;

        #[cfg(unittest)]
        if capacity != 0 {
            crate::fault_injection::maybe_fail_allocation()?;
        }

        let mut inner = BinaryHeap::new();
        inner
            .try_reserve(capacity)
            .map_err(|_| AllocError::NoMemory)?;
        Ok(Self { inner })
    }

    /// Returns the number of elements in the heap.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns `true` if the heap is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Returns the capacity of the heap.
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    /// Reserves capacity for at least `additional` more elements.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when the heap must grow and the
    /// allocator cannot satisfy the request. Returns
    /// [`AllocError::UnsupportedContext`] when `context` is not currently
    /// supported.
    pub fn try_reserve(
        &mut self,
        additional: usize,
        context: AllocContext,
    ) -> Result<(), AllocError> {
        require_supported_context(context)?;

        #[cfg(unittest)]
        if additional > self.inner.capacity().saturating_sub(self.inner.len()) {
            crate::fault_injection::maybe_fail_allocation()?;
        }

        self.inner
            .try_reserve(additional)
            .map_err(|_| AllocError::NoMemory)
    }

    /// Removes all elements, keeping the allocated buffer.
    pub fn clear(&mut self) {
        self.inner.clear();
    }
}

impl<T: Ord> KBinaryHeap<T> {
    /// Pushes an item onto the heap.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when growth is required and the
    /// kernel allocator cannot satisfy it.
    pub fn try_push(&mut self, item: T, context: AllocContext) -> Result<(), AllocError> {
        self.try_reserve(1, context)?;
        self.inner.push(item);
        Ok(())
    }

    /// Removes and returns the greatest item from the heap, if any.
    pub fn pop(&mut self) -> Option<T> {
        self.inner.pop()
    }

    /// Returns a reference to the greatest item, if any.
    pub fn peek(&self) -> Option<&T> {
        self.inner.peek()
    }
}

impl<T> Default for KBinaryHeap<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: fmt::Debug> fmt::Debug for KBinaryHeap<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

// SAFETY: KBinaryHeap wraps BinaryHeap which is Send when T: Send.
unsafe impl<T: Send> Send for KBinaryHeap<T> {}

// SAFETY: KBinaryHeap wraps BinaryHeap which is Sync when T: Sync.
unsafe impl<T: Sync> Sync for KBinaryHeap<T> {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;
    use crate::fault_injection::fail_next_allocation;

    #[def_test]
    fn new_is_empty() {
        let h = KBinaryHeap::<u32>::new();
        assert!(h.is_empty());
        assert_eq!(h.len(), 0);
    }

    #[def_test]
    fn try_push_and_pop_max() {
        let mut h = KBinaryHeap::new();
        h.try_push(3, AllocContext::Sleepable).unwrap();
        h.try_push(1, AllocContext::Sleepable).unwrap();
        h.try_push(2, AllocContext::Sleepable).unwrap();
        assert_eq!(h.len(), 3);
        assert_eq!(h.pop(), Some(3));
        assert_eq!(h.pop(), Some(2));
        assert_eq!(h.pop(), Some(1));
        assert_eq!(h.pop(), None);
    }

    #[def_test]
    fn peek_does_not_remove() {
        let mut h = KBinaryHeap::new();
        h.try_push(5, AllocContext::Sleepable).unwrap();
        h.try_push(9, AllocContext::Sleepable).unwrap();
        assert_eq!(h.peek(), Some(&9));
        assert_eq!(h.len(), 2);
    }

    #[def_test]
    fn clear_removes_all() {
        let mut h = KBinaryHeap::new();
        h.try_push(1, AllocContext::Sleepable).unwrap();
        h.try_push(2, AllocContext::Sleepable).unwrap();
        h.clear();
        assert!(h.is_empty());
    }

    #[def_test]
    fn try_push_rejects_unsupported_context() {
        let mut h = KBinaryHeap::<u32>::new();
        let err = h.try_push(1, AllocContext::Atomic).unwrap_err();
        assert_eq!(err, AllocError::UnsupportedContext);
    }
    #[def_test]
    fn try_with_capacity_preallocates() {
        let h = KBinaryHeap::<u32>::try_with_capacity(10, AllocContext::Sleepable).unwrap();
        assert!(h.capacity() >= 10);
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
