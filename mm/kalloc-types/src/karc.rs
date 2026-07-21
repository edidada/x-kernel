// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fallible atomically reference-counted pointer ([`KArc`]) and its weak
//! counterpart ([`KWeak`]).
//!
//! These types provide the same sharing semantics as `alloc::sync::Arc` /
//! `alloc::sync::Weak`, but the owning constructor `KArc::try_new` performs
//! explicit fallible allocation through `kalloc::global_allocator()` rather
//! than routing through the infallible `#[global_allocator]` path.

use core::{
    alloc::Layout,
    fmt,
    marker::PhantomData,
    mem::ManuallyDrop,
    ops::Deref,
    ptr::{self, NonNull},
    sync::atomic::{self, AtomicUsize, Ordering},
};

use crate::{AllocContext, AllocError, context::require_supported_context};

/// Internal heap allocation shared by [`KArc`] and [`KWeak`].
///
/// # Layout stability
///
/// `#[repr(C)]` guarantees predictable field ordering:
/// - `strong` at offset 0
/// - `weak` at offset `size_of::<AtomicUsize>()`
/// - `data` at offset `2 * size_of::<AtomicUsize>()`
///
/// This struct is never exposed outside this module.
#[repr(C)]
struct KArcInner<T> {
    /// Strong reference count. `data` is only alive while this is >= 1.
    strong: AtomicUsize,
    /// Weak reference count (includes one implicit count for every `KArc`).
    /// When this reaches 0, the entire allocation is freed.
    weak: AtomicUsize,
    /// The owned data.
    data: T,
}

/// A fallible atomically reference-counted pointer, analogous to
/// `alloc::sync::Arc`.
///
/// Unlike `Arc`, the `try_new` constructor performs explicit fallible heap
/// allocation through `kalloc::global_allocator()` instead of panicking on
/// out-of-memory.
pub struct KArc<T> {
    ptr: NonNull<KArcInner<T>>,
    _phantom: PhantomData<KArcInner<T>>,
}

/// A weak counterpart to [`KArc`], analogous to `alloc::sync::Weak`.
///
/// `KWeak` does not keep the inner data alive — once all `KArc` handles are
/// dropped, `upgrade()` returns `None`.
pub struct KWeak<T> {
    ptr: NonNull<KArcInner<T>>,
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Allocate an uninitialised `KArcInner<T>` via the fallible kernel allocator.
fn alloc_arc_inner<T>(context: AllocContext) -> Result<NonNull<KArcInner<T>>, AllocError> {
    require_supported_context(context)?;

    #[cfg(unittest)]
    crate::fault_injection::maybe_fail_allocation()?;

    let layout = Layout::new::<KArcInner<T>>();
    let ptr = kalloc::global_allocator()
        .alloc(layout)
        .map_err(|_| AllocError::NoMemory)?;
    Ok(ptr.cast())
}

/// Deallocate a `KArcInner<T>` that was previously allocated via
/// `alloc_arc_inner`.
///
/// # Safety
///
/// The pointer must have been obtained from `alloc_arc_inner` and must not be
/// used afterwards. The stored `data` must already have been dropped.
unsafe fn dealloc_arc_inner<T>(ptr: NonNull<KArcInner<T>>) {
    let layout = Layout::new::<KArcInner<T>>();
    kalloc::global_allocator().dealloc(ptr.cast(), layout);
}

// ---------------------------------------------------------------------------
// KArc
// ---------------------------------------------------------------------------

impl<T> KArc<T> {
    /// Construct a new `KArc<T>` with fallible heap allocation.
    ///
    /// Only [`AllocContext::Sleepable`] is supported in phase 1.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when the kernel allocator cannot
    /// satisfy the allocation, or [`AllocError::UnsupportedContext`] when
    /// the caller requests a context reserved for a later phase.
    pub fn try_new(data: T, context: AllocContext) -> Result<Self, AllocError> {
        let ptr = alloc_arc_inner::<T>(context)?;

        // SAFETY: alloc_arc_inner returned a valid exclusive pointer to
        // uninitialised memory large enough for KArcInner<T>. We write all
        // fields before the allocation can be observed by any other thread.
        unsafe {
            ptr::addr_of_mut!((*ptr.as_ptr()).strong).write(AtomicUsize::new(1));
            ptr::addr_of_mut!((*ptr.as_ptr()).weak).write(AtomicUsize::new(1));
            ptr::addr_of_mut!((*ptr.as_ptr()).data).write(data);
        }

        Ok(KArc {
            ptr,
            _phantom: PhantomData,
        })
    }

    /// Construct a `KArc<T>` where the inner data is allowed to reference the
    /// `KWeak<T>` that wraps this allocation, analogous to `Arc::new_cyclic`.
    ///
    /// Before `data_fn` is called, a temporary `KWeak<T>` is created pointing
    /// at this allocation (with `strong = 0`). This means that `KWeak::upgrade`
    /// called inside `data_fn` returns `None`, preventing observation of
    /// uninitialised data.
    ///
    /// Only [`AllocContext::Sleepable`] is supported in phase 1.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when the kernel allocator cannot
    /// satisfy the allocation.
    pub fn try_new_cyclic<F>(data_fn: F, context: AllocContext) -> Result<Self, AllocError>
    where
        F: FnOnce(&KWeak<T>) -> T,
    {
        let ptr = alloc_arc_inner::<T>(context)?;

        // SAFETY: We have exclusive ownership of a properly-sized allocation.
        // Writing strong=0 and weak=1 initialises the ref-count header before
        // the temporary KWeak is created.
        unsafe {
            ptr::addr_of_mut!((*ptr.as_ptr()).strong).write(AtomicUsize::new(0));
            ptr::addr_of_mut!((*ptr.as_ptr()).weak).write(AtomicUsize::new(1));
        }

        // Create a temporary KWeak. ManuallyDrop prevents its Drop from
        // decrementing `weak` — the eventual KArc will own that slot.
        let tmp_weak = ManuallyDrop::new(KWeak { ptr });
        let data = data_fn(&tmp_weak);

        // SAFETY: The data slot was previously uninitialised. Writing the
        // closure result initialises it before strong is set to 1.
        unsafe {
            ptr::addr_of_mut!((*ptr.as_ptr()).data).write(data);
        }

        // Promote strong from 0 to 1, making data visible to clones.
        // SAFETY: ptr points to a live allocation; Release pairs with Acquire
        // in clone/deref to ensure the data write above is visible.
        unsafe {
            (*ptr.as_ptr()).strong.store(1, Ordering::Release);
        }

        Ok(KArc {
            ptr,
            _phantom: PhantomData,
        })
    }

    /// Returns the number of strong references to this allocation.
    ///
    /// This is a snapshot and may be stale by the time the caller inspects it.
    pub fn strong_count(this: &Self) -> usize {
        // SAFETY: `this.ptr` points to a live KArcInner<T> (KArc still exists).
        unsafe { (*this.ptr.as_ptr()).strong.load(Ordering::Relaxed) }
    }

    /// Returns the number of weak references to this allocation.
    ///
    /// This count includes the implicit weak reference held by every `KArc`,
    /// so it is always >= the number of live `KArc` handles.
    pub fn weak_count(this: &Self) -> usize {
        // SAFETY: `this.ptr` points to a live KArcInner<T> (KArc still exists).
        let raw = unsafe { (*this.ptr.as_ptr()).weak.load(Ordering::Relaxed) };
        // Subtract the implicit weak that every KArc holds. Saturate at 0.
        raw.saturating_sub(1)
    }

    /// Returns `true` if the two `KArc`s point to the same allocation.
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        ptr::eq(this.ptr.as_ptr(), other.ptr.as_ptr())
    }

    /// Create a new [`KWeak`] pointer to this allocation.
    pub fn downgrade(this: &Self) -> KWeak<T> {
        // SAFETY: `this.ptr` points to a live KArcInner<T>.
        let old_weak = unsafe { (*this.ptr.as_ptr()).weak.fetch_add(1, Ordering::Relaxed) };
        // SAFETY: overflow means ref-count corruption — unreachable.
        if old_weak > isize::MAX as usize {
            abort_overflow();
        }

        KWeak { ptr: this.ptr }
    }

    /// Attempt to unwrap this `KArc`, returning the inner value if this is the
    /// only strong reference.
    ///
    /// Returns `Err(self)` if there are other strong references.
    pub fn try_unwrap(this: Self) -> Result<T, Self> {
        // SAFETY: `this.ptr` points to a live KArcInner<T>.
        // Use CAS to atomically claim the last strong ref and prevent a
        // concurrent KWeak::upgrade() from racing past us.
        let prev = unsafe {
            (*this.ptr.as_ptr())
                .strong
                .compare_exchange(1, 0, Ordering::Acquire, Ordering::Relaxed)
        };
        if prev.is_err() {
            return Err(this);
        }

        // We hold the only strong reference (now atomically set to 0).
        // Prevent the KArc Drop from running.
        let this = ManuallyDrop::new(this);

        // SAFETY: strong was atomically set to 0; no other KArc can exist.
        let data = unsafe { ptr::read(ptr::addr_of!((*this.ptr.as_ptr()).data)) };

        // Release the implicit weak reference held by this KArc.
        // SAFETY: `this.ptr` still points to a live KArcInner<T>.
        let old_weak = unsafe { (*this.ptr.as_ptr()).weak.fetch_sub(1, Ordering::Release) };

        if old_weak == 1 {
            // SAFETY: Last reference of any kind. Acquire fence synchronises
            // with prior releases, then deallocate.
            unsafe {
                atomic::fence(Ordering::Acquire);
                dealloc_arc_inner(this.ptr);
            }
        }

        Ok(data)
    }
}

impl<T: fmt::Debug> fmt::Debug for KArc<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T> Deref for KArc<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: ptr points to a live KArcInner<T> whose data is initialised
        // (strong >= 1 guarantees this while any KArc exists).
        unsafe { &(*self.ptr.as_ptr()).data }
    }
}

impl<T> Clone for KArc<T> {
    fn clone(&self) -> Self {
        // SAFETY: `self.ptr` points to a live KArcInner<T>.
        let old_strong = unsafe { (*self.ptr.as_ptr()).strong.fetch_add(1, Ordering::Relaxed) };

        // Overflow means refcount leak/corruption — abort.
        if old_strong > isize::MAX as usize {
            abort_overflow();
        }

        KArc {
            ptr: self.ptr,
            _phantom: PhantomData,
        }
    }
}

impl<T> Drop for KArc<T> {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` points to a live KArcInner<T> that we own a
        // strong reference to. Release ordering makes prior data accesses
        // visible to the thread that drops the last strong ref.
        let old_strong = unsafe { (*self.ptr.as_ptr()).strong.fetch_sub(1, Ordering::Release) };

        if old_strong != 1 {
            return;
        }

        // We were the last strong reference.
        atomic::fence(Ordering::Acquire);

        // SAFETY: The data is exclusively owned by us (last strong ref).
        unsafe {
            ptr::drop_in_place(ptr::addr_of_mut!((*self.ptr.as_ptr()).data));
        }

        // Release the implicit weak reference.
        // SAFETY: `self.ptr` still points to a live KArcInner<T>.
        let old_weak = unsafe { (*self.ptr.as_ptr()).weak.fetch_sub(1, Ordering::Release) };

        if old_weak == 1 {
            // SAFETY: Last reference of any kind. Deallocate the inner struct.
            unsafe {
                atomic::fence(Ordering::Acquire);
                dealloc_arc_inner(self.ptr);
            }
        }
    }
}

// SAFETY: KArc<T> is Send when T: Send + Sync. The atomic ref counts ensure
// concurrent Clone/Drop are safe. T must be Sync because &T can be obtained
// from distinct KArc clones sharing the same allocation.
unsafe impl<T: Send + Sync> Send for KArc<T> {}

// SAFETY: KArc<T> is Sync when T: Sync. &KArc<T> provides only &T to all
// threads, which requires T: Sync. The ref-count atomics are inherently Sync.
unsafe impl<T: Sync> Sync for KArc<T> {}

// ---------------------------------------------------------------------------
// KWeak
// ---------------------------------------------------------------------------

impl<T> KWeak<T> {
    /// Attempt to upgrade this weak reference to a [`KArc<T>`].
    ///
    /// Returns `None` if all strong references have been dropped (i.e. the
    /// inner data has been deallocated).
    pub fn upgrade(&self) -> Option<KArc<T>> {
        // SAFETY: ptr points to a live KArcInner allocation (KWeak holds a
        // weak reference, so the allocation has not been freed).
        let mut strong = unsafe { (*self.ptr.as_ptr()).strong.load(Ordering::Relaxed) };

        loop {
            if strong == 0 {
                return None;
            }

            // SAFETY: ptr points to a live KArcInner. CAS tries to increment
            // strong from the value we just read.
            match unsafe {
                (*self.ptr.as_ptr()).strong.compare_exchange_weak(
                    strong,
                    strong + 1,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
            } {
                Ok(_) => {
                    return Some(KArc {
                        ptr: self.ptr,
                        _phantom: PhantomData,
                    });
                }
                Err(current) => strong = current,
            }
        }
    }

    /// Returns `true` if the two `KWeak`s point to the same allocation.
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        ptr::eq(this.ptr.as_ptr(), other.ptr.as_ptr())
    }
}

impl<T: fmt::Debug> fmt::Debug for KWeak<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "(KWeak)")
    }
}

impl<T> Clone for KWeak<T> {
    fn clone(&self) -> Self {
        // SAFETY: ptr points to a live KArcInner allocation.
        let old_weak = unsafe { (*self.ptr.as_ptr()).weak.fetch_add(1, Ordering::Relaxed) };
        if old_weak > isize::MAX as usize {
            abort_overflow();
        }

        KWeak { ptr: self.ptr }
    }
}

impl<T> Drop for KWeak<T> {
    fn drop(&mut self) {
        // SAFETY: ptr points to a live KArcInner allocation (we hold a weak
        // reference). Release pairs with Acquire in deallocation path.
        let old_weak = unsafe { (*self.ptr.as_ptr()).weak.fetch_sub(1, Ordering::Release) };

        if old_weak != 1 {
            return;
        }

        // Last weak reference.
        atomic::fence(Ordering::Acquire);

        // SAFETY: ptr still points to a live KArcInner (we are the last weak).
        let strong = unsafe { (*self.ptr.as_ptr()).strong.load(Ordering::Relaxed) };

        if strong == 0 {
            // SAFETY: Data already dropped by last KArc. We are the last
            // reference of any kind — deallocate the inner struct.
            unsafe {
                dealloc_arc_inner(self.ptr);
            }
        }
    }
}

// SAFETY: KWeak<T> is Send when T: Send + Sync. upgrade() can produce a KArc
// that exposes &T across threads.
unsafe impl<T: Send + Sync> Send for KWeak<T> {}

// SAFETY: KWeak<T> is Sync when T: Sync, same reasoning as KArc.
unsafe impl<T: Sync> Sync for KWeak<T> {}

// ---------------------------------------------------------------------------
// Overflow abort
// ---------------------------------------------------------------------------

#[cold]
#[inline(never)]
fn abort_overflow() -> ! {
    panic!("KArc/KWeak ref-count overflow — possible leak or memory corruption");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(unittest)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::def_test;

    use super::*;
    use crate::fault_injection::{fail_allocations_after, fail_next_allocation};

    /// A helper that records how many times it has been dropped and the sum of
    /// IDs of dropped instances.
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

    // -- Basic construction and access ----------------------------------

    #[def_test]
    fn try_new_allocates_and_derefs() {
        let arc = KArc::try_new(42_u32, AllocContext::Sleepable).unwrap();
        assert_eq!(*arc, 42);
        assert_eq!(KArc::strong_count(&arc), 1);
    }

    #[def_test]
    fn clone_increments_strong_count() {
        let a = KArc::try_new(99_u32, AllocContext::Sleepable).unwrap();
        assert_eq!(KArc::strong_count(&a), 1);
        let b = a.clone();
        assert_eq!(*b, 99);
        assert_eq!(KArc::strong_count(&a), 2);
        assert_eq!(KArc::strong_count(&b), 2);
    }

    #[def_test]
    fn ptr_eq_same_inner() {
        let a = KArc::try_new(1_u32, AllocContext::Sleepable).unwrap();
        let b = a.clone();
        assert!(KArc::ptr_eq(&a, &b));
    }

    #[def_test]
    fn ptr_eq_different_inner() {
        let a = KArc::try_new(1_u32, AllocContext::Sleepable).unwrap();
        let b = KArc::try_new(2_u32, AllocContext::Sleepable).unwrap();
        assert!(!KArc::ptr_eq(&a, &b));
    }

    #[def_test]
    fn strong_and_weak_count_reflect_references() {
        let a = KArc::try_new("hello", AllocContext::Sleepable).unwrap();
        assert_eq!(KArc::strong_count(&a), 1);
        assert_eq!(KArc::weak_count(&a), 0);

        let w = KArc::downgrade(&a);
        assert_eq!(KArc::weak_count(&a), 1);

        let _a2 = a.clone();
        assert_eq!(KArc::strong_count(&a), 2);

        drop(w);
        assert_eq!(KArc::weak_count(&a), 0);
    }

    // -- Drop behaviour -------------------------------------------------

    #[def_test]
    fn drop_after_clone_keeps_inner_alive() {
        let drop_count = AtomicUsize::new(0);
        let drop_sum = AtomicUsize::new(0);

        let a = KArc::try_new(
            DropTracker {
                id: 7,
                drops: &drop_count,
                sum: &drop_sum,
            },
            AllocContext::Sleepable,
        )
        .unwrap();

        let b = a.clone();
        drop(a);
        assert_eq!(drop_count.load(Ordering::SeqCst), 0);
        assert_eq!(*b.id_ref(), 7);

        drop(b);
        assert_eq!(drop_count.load(Ordering::SeqCst), 1);
        assert_eq!(drop_sum.load(Ordering::SeqCst), 7);
    }

    // Helper extension for DropTracker
    impl DropTracker<'_> {
        fn id_ref(&self) -> &usize {
            &self.id
        }
    }

    // -- Weak references -------------------------------------------------

    #[def_test]
    fn downgrade_and_upgrade_succeeds() {
        let a = KArc::try_new(123_u32, AllocContext::Sleepable).unwrap();
        let w = KArc::downgrade(&a);
        let upgraded = w.upgrade().unwrap();
        assert_eq!(*upgraded, 123);
        assert!(KArc::ptr_eq(&a, &upgraded));
    }

    #[def_test]
    fn weak_upgrade_returns_none_after_all_strongs_dropped() {
        let a = KArc::try_new(42_u32, AllocContext::Sleepable).unwrap();
        let w = KArc::downgrade(&a);
        drop(a);
        assert!(w.upgrade().is_none());
    }

    #[def_test]
    fn weak_clone_increments_weak_count() {
        let a = KArc::try_new(1_u32, AllocContext::Sleepable).unwrap();
        let w1 = KArc::downgrade(&a);
        assert_eq!(KArc::weak_count(&a), 1);
        let w2 = w1.clone();
        assert_eq!(KArc::weak_count(&a), 2);
        drop(w1);
        assert_eq!(KArc::weak_count(&a), 1);
        drop(w2);
        assert_eq!(KArc::weak_count(&a), 0);
    }

    #[def_test]
    fn weak_does_not_prevent_data_drop() {
        let drop_count = AtomicUsize::new(0);
        let drop_sum = AtomicUsize::new(0);

        let a = KArc::try_new(
            DropTracker {
                id: 3,
                drops: &drop_count,
                sum: &drop_sum,
            },
            AllocContext::Sleepable,
        )
        .unwrap();

        let _w = KArc::downgrade(&a);
        // Drop all strong refs — data should be dropped even though KWeak exists.
        drop(a);
        assert_eq!(drop_count.load(Ordering::SeqCst), 1);
        assert_eq!(drop_sum.load(Ordering::SeqCst), 3);
    }

    // -- try_unwrap -----------------------------------------------------

    #[def_test]
    fn try_unwrap_last_strong_succeeds() {
        let a = KArc::try_new(77_u32, AllocContext::Sleepable).unwrap();
        let val = KArc::try_unwrap(a).unwrap();
        assert_eq!(val, 77);
    }

    #[def_test]
    fn try_unwrap_with_other_strong_fails() {
        let a = KArc::try_new(88_u32, AllocContext::Sleepable).unwrap();
        let _b = a.clone();
        match KArc::try_unwrap(a) {
            Err(returned) => {
                assert_eq!(*returned, 88);
            }
            Ok(_) => panic!("expected Err, got Ok"),
        }
    }

    // -- new_cyclic ----------------------------------------------------

    #[def_test]
    fn new_cyclic_constructs_self_referential() {
        struct SelfRef {
            self_weak: Option<KWeak<SelfRef>>,
            value: u32,
        }

        let arc = KArc::<SelfRef>::try_new_cyclic(
            |weak| SelfRef {
                self_weak: Some(weak.clone()),
                value: 42,
            },
            AllocContext::Sleepable,
        )
        .unwrap();

        assert_eq!(arc.value, 42);
        let upgraded = arc.self_weak.as_ref().unwrap().upgrade().unwrap();
        assert!(KArc::ptr_eq(&arc, &upgraded));
        assert_eq!(upgraded.value, 42);
    }

    #[def_test]
    fn new_cyclic_weak_cannot_upgrade_during_closure() {
        let arc = KArc::<u32>::try_new_cyclic(
            |weak| {
                assert!(weak.upgrade().is_none());
                99
            },
            AllocContext::Sleepable,
        )
        .unwrap();

        assert_eq!(*arc, 99);
    }

    // -- Error handling -------------------------------------------------

    #[def_test]
    fn try_new_rejects_unsupported_context() {
        let err = KArc::<u32>::try_new(1, AllocContext::Atomic).unwrap_err();
        assert_eq!(err, AllocError::UnsupportedContext);
    }

    #[def_test]
    fn try_new_cyclic_rejects_unsupported_context() {
        let err = KArc::<u32>::try_new_cyclic(|_| 1, AllocContext::NoWait).unwrap_err();
        assert_eq!(err, AllocError::UnsupportedContext);
    }

    #[def_test(serial)]
    fn try_new_reports_injected_nomemory() {
        let _guard = fail_next_allocation();
        let err = KArc::<u32>::try_new(42, AllocContext::Sleepable).unwrap_err();
        assert_eq!(err, AllocError::NoMemory);
    }

    #[def_test(serial)]
    fn try_new_cyclic_reports_injected_nomemory() {
        let _guard = fail_next_allocation();
        let err = KArc::<u32>::try_new_cyclic(|_| 1, AllocContext::Sleepable).unwrap_err();
        assert_eq!(err, AllocError::NoMemory);
    }

    // -- Debug formatting -----------------------------------------------

    #[def_test]
    fn debug_format_delegates_to_inner() {
        let arc = KArc::try_new(42_u32, AllocContext::Sleepable).unwrap();
        let s = alloc::format!("{:?}", arc);
        assert!(s.contains("42"));
    }

    #[def_test]
    fn weak_debug_format() {
        let arc = KArc::try_new(1_u32, AllocContext::Sleepable).unwrap();
        let w = KArc::downgrade(&arc);
        let s = alloc::format!("{:?}", w);
        assert!(s.contains("KWeak"));
    }

    // -- Multi-allocation roundtrip -------------------------------------

    #[def_test(serial)]
    fn multiple_allocations_independent() {
        let _guard = fail_allocations_after(1);
        let a = KArc::try_new(10_u32, AllocContext::Sleepable).unwrap();
        assert_eq!(*a, 10);
        let err = KArc::<u32>::try_new(20, AllocContext::Sleepable).unwrap_err();
        assert_eq!(err, AllocError::NoMemory);
    }
}
