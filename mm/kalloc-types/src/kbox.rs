// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    alloc::Layout,
    marker::PhantomData,
    mem::{ManuallyDrop, MaybeUninit},
    ops::{Deref, DerefMut},
    ptr::{self, NonNull},
};

use crate::{AllocContext, AllocError, context::require_supported_context};

fn alloc_storage<T>(context: AllocContext) -> Result<NonNull<T>, AllocError> {
    require_supported_context(context)?;

    #[cfg(unittest)]
    crate::fault_injection::maybe_fail_allocation()?;

    let layout = Layout::new::<T>();
    let ptr = kalloc::global_allocator()
        .alloc(layout)
        .map_err(|_| AllocError::NoMemory)?;
    Ok(ptr.cast())
}

unsafe fn dealloc_storage<T>(ptr: NonNull<T>) {
    let layout = Layout::new::<T>();
    // SAFETY: caller guarantees ptr was allocated via alloc_storage<T>.
    kalloc::global_allocator().dealloc(ptr.cast(), layout);
}

/// Explicitly fallible single-object heap allocation.
///
/// Unlike `alloc::boxed::Box`, `KBox` stores a raw `NonNull` pointer and
/// calls `kalloc::global_allocator()` directly for allocation and
/// deallocation. No infallible allocation types appear in its definition.
pub struct KBox<T> {
    ptr: NonNull<T>,
    _marker: PhantomData<T>,
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
        let boxed = Self::try_new_uninit(context)?;
        // SAFETY: pointer is valid and uninitialized; writing moves `value` into
        // the heap slot.
        unsafe { boxed.ptr.as_ptr().cast::<T>().write(value) };
        // SAFETY: the value was just written above, so the slot is initialized.
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
        let ptr = alloc_storage::<MaybeUninit<T>>(context)?;
        Ok(KBox {
            ptr,
            _marker: PhantomData,
        })
    }

    /// Consumes the wrapper and returns the owned value, deallocating the
    /// heap storage.
    pub fn into_inner(self) -> T {
        let this = ManuallyDrop::new(self);
        // SAFETY: KBox owns the heap slot, so reading it out is valid.
        // The storage is deallocated below without dropping the value again.
        let value = unsafe { ptr::read(this.ptr.as_ptr()) };
        // SAFETY: ptr was allocated via alloc_storage<T>, no drop needed
        // because the value was moved out.
        unsafe { dealloc_storage::<T>(this.ptr) };
        value
    }

    /// Consumes the wrapper and returns the raw pointer, leaking the
    /// allocation (the caller takes ownership of the memory).
    pub fn into_raw(self) -> NonNull<T> {
        let this = ManuallyDrop::new(self);
        this.ptr
    }

    /// Consumes and leaks the `KBox`, returning a mutable reference,
    /// `&'a mut T`. This is the fallible analogue of [`alloc::boxed::Box::leak`].
    ///
    /// The allocation will never be freed — it lives until the kernel shuts down.
    /// This is useful for static / global data that must outlive any single
    /// subsystem.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let x = KBox::try_new(42, AllocContext::Sleepable)?;
    /// let static_ref: &'static mut i32 = KBox::leak(x);
    /// *static_ref += 1;
    /// assert_eq!(*static_ref, 43);
    /// ```
    pub fn leak<'a>(self) -> &'a mut T
    where
        T: 'a,
    {
        let ptr = self.into_raw();
        // SAFETY: ptr was allocated via alloc_storage<T> and is valid.
        // We intentionally leak self so the memory will never be freed,
        // making the returned reference effectively 'static.
        unsafe { &mut *ptr.as_ptr() }
    }

    /// Creates a `KBox` from a raw pointer previously returned by
    /// [`into_raw`](Self::into_raw).
    ///
    /// # Safety
    ///
    /// `ptr` must have been obtained from `KBox::into_raw()`.
    pub unsafe fn from_raw(ptr: NonNull<T>) -> Self {
        Self {
            ptr,
            _marker: PhantomData,
        }
    }
}

impl<T> KBox<MaybeUninit<T>> {
    /// Converts an initialized `KBox<MaybeUninit<T>>` into `KBox<T>`.
    ///
    /// # Safety
    ///
    /// The caller must ensure the wrapped value has been fully initialized.
    pub unsafe fn assume_init(self) -> KBox<T> {
        let this = ManuallyDrop::new(self);
        let ptr = this.ptr.cast::<T>();
        KBox {
            ptr,
            _marker: PhantomData,
        }
    }
}

impl<T> Drop for KBox<T> {
    fn drop(&mut self) {
        // SAFETY: ptr was allocated via alloc_storage<T> and the value is
        // fully initialized (KBox invariant).
        unsafe {
            ptr::drop_in_place(self.ptr.as_ptr());
            dealloc_storage::<T>(self.ptr);
        }
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
        // SAFETY: ptr is valid and the value is initialized (KBox invariant).
        unsafe { self.ptr.as_ref() }
    }
}

impl<T> DerefMut for KBox<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: ptr is valid, the value is initialized, and we have &mut self
        // giving exclusive access.
        unsafe { self.ptr.as_mut() }
    }
}

impl<T: core::fmt::Debug> core::fmt::Debug for KBox<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(&**self, f)
    }
}

// SAFETY: KBox owns its heap allocation exclusively — same rationale as
// `Box<T>` for both Send and Sync.
unsafe impl<T: Send> Send for KBox<T> {}
// SAFETY: &KBox<T> provides &T through Deref; sharing is sound when T: Sync.
unsafe impl<T: Sync> Sync for KBox<T> {}

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

    #[def_test]
    fn into_raw_and_from_raw_roundtrip() {
        let boxed = KBox::try_new(42_u32, AllocContext::Sleepable).unwrap();
        let raw = boxed.into_raw();
        // SAFETY: `raw` was just obtained from `KBox::into_raw()` above,
        // satisfying the precondition of `from_raw`.
        let restored = unsafe { KBox::from_raw(raw) };
        assert_eq!(*restored, 42);
    }

    #[def_test]
    fn leak_returns_static_mut_ref() {
        let boxed = KBox::try_new(99_u32, AllocContext::Sleepable).unwrap();
        let static_ref: &mut u32 = KBox::leak(boxed);
        assert_eq!(*static_ref, 99);
        *static_ref += 1;
        assert_eq!(*static_ref, 100);
    }
}
