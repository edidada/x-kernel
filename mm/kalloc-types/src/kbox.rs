// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::boxed::Box;
use core::{
    alloc::Layout,
    mem::MaybeUninit,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

use crate::{AllocContext, AllocError, context::require_supported_context};

fn alloc_box_storage<T>(context: AllocContext) -> Result<NonNull<T>, AllocError> {
    require_supported_context(context)?;

    #[cfg(unittest)]
    crate::fault_injection::maybe_fail_allocation()?;

    let layout = Layout::new::<T>();
    let ptr = kalloc::global_allocator()
        .alloc(layout)
        .map_err(|_| AllocError::NoMemory)?;
    Ok(ptr.cast())
}

/// Explicitly fallible single-object heap allocation.
#[derive(Debug)]
pub struct KBox<T> {
    inner: Box<T>,
}

impl<T> KBox<T> {
    /// Allocates storage for `value` and returns an owned heap object.
    ///
    /// Only [`AllocContext::Sleepable`] is supported in phase 1.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when the kernel allocator cannot
    /// satisfy the allocation.
    ///
    /// Returns [`AllocError::UnsupportedContext`] when the caller requests an
    /// allocation context reserved for a later phase.
    pub fn try_new(value: T, context: AllocContext) -> Result<Self, AllocError> {
        let mut boxed = Self::try_new_uninit(context)?;
        (*boxed).write(value);
        // SAFETY: The value was written to the owned heap slot immediately
        // above, so it is fully initialized before conversion.
        Ok(unsafe { boxed.assume_init() })
    }

    /// Allocates uninitialized storage for a `T`.
    ///
    /// Only [`AllocContext::Sleepable`] is supported in phase 1.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when the kernel allocator cannot
    /// satisfy the allocation.
    ///
    /// Returns [`AllocError::UnsupportedContext`] when the caller requests an
    /// allocation context reserved for a later phase.
    pub fn try_new_uninit(context: AllocContext) -> Result<KBox<MaybeUninit<T>>, AllocError> {
        let ptr = alloc_box_storage::<MaybeUninit<T>>(context)?;
        // SAFETY: `ptr` comes from the kernel global allocator with a layout
        // that exactly matches `MaybeUninit<T>`, so re-wrapping it as `Box` is
        // valid and transfers ownership into RAII.
        let inner = unsafe { Box::from_raw(ptr.as_ptr()) };
        Ok(KBox { inner })
    }

    /// Consumes the wrapper and returns the owned value.
    pub fn into_inner(self) -> T {
        *self.inner
    }

    /// Consumes the wrapper and returns the inner `Box<T>`.
    ///
    /// This is useful for integrating with code that expects a standard
    /// `Box` (e.g. B-tree node storage). The `Box` will use the same
    /// allocator (`kalloc::global_allocator()`) for deallocation.
    pub fn into_box(self) -> Box<T> {
        self.inner
    }
}

impl<T> KBox<MaybeUninit<T>> {
    /// Converts an initialized `KBox<MaybeUninit<T>>` into `KBox<T>`.
    ///
    /// # Safety
    ///
    /// The caller must ensure the wrapped value has been fully initialized.
    pub unsafe fn assume_init(self) -> KBox<T> {
        let ptr = Box::into_raw(self.inner).cast::<T>();
        // SAFETY: The caller guarantees the pointee is initialized, and the
        // allocation still belongs to the same `Box`.
        let inner = unsafe { Box::from_raw(ptr) };
        KBox { inner }
    }
}

impl<T> AsRef<T> for KBox<T> {
    fn as_ref(&self) -> &T {
        self
    }
}

impl<T> AsMut<T> for KBox<T> {
    fn as_mut(&mut self) -> &mut T {
        self
    }
}

impl<T> Deref for KBox<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> DerefMut for KBox<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::AllocContext;
    use crate::{KBox, fault_injection::fail_next_allocation};

    #[def_test]
    fn try_new_stores_value() {
        let boxed = KBox::try_new(7_u32, AllocContext::Sleepable).unwrap();
        assert_eq!(*boxed, 7);
    }

    #[def_test]
    fn into_inner_returns_owned_value() {
        let boxed = KBox::try_new(11_u32, AllocContext::Sleepable).unwrap();
        assert_eq!(boxed.into_inner(), 11);
    }

    #[def_test(serial)]
    fn try_new_reports_injected_nomemory() {
        let _guard = fail_next_allocation();
        let err = KBox::try_new(13_u32, AllocContext::Sleepable).unwrap_err();
        assert_eq!(err, crate::AllocError::NoMemory);
    }
}
