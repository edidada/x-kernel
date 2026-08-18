// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fallible allocation contract types built on top of `kalloc`.
//!
//! This crate defines the public type boundary for explicit heap-allocation
//! failure handling. It intentionally keeps the phase-1 surface small:
//! canonical allocation errors, reserved allocation-context metadata, and the
//! named heap object wrappers that later tasks make operational.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod alloc_error;
mod collect;
mod context;
#[cfg(unittest)]
pub mod fault_injection;
mod karc;
mod kbinaryheap;
mod kbox;
mod kbtree;
mod khashmap;
mod kstring;
mod kvec;
mod kvecdeque;
mod try_traits;

pub use alloc_error::AllocError;
pub use collect::try_collect_vec;
pub use context::AllocContext;
pub use karc::{KArc, KWeak};
pub use kbinaryheap::KBinaryHeap;
pub use kbox::KBox;
pub use kbtree::{map::BTreeMap as KBTreeMap, set::BTreeSet as KBTreeSet};
pub use khashmap::KHashMap;
pub use kstring::KString;
pub use kvec::{KVec, KVecIntoIter};

/// Fallible replacement for `alloc::sync::Arc`.
pub type Arc<T> = KArc<T>;

/// Fallible replacement for `alloc::sync::Weak`.
pub type Weak<T> = KWeak<T>;

/// Fallible replacement for `alloc::vec::Vec`.
pub type Vec<T> = KVec<T>;

/// Fallible replacement for `alloc::collections::VecDeque`.
pub type VecDeque<T> = KVecDeque<T>;

/// Fallible replacement for `alloc::string::String`.
pub type String = KString;

/// Fallible replacement for `alloc::boxed::Box`.
pub type Box<T> = KBox<T>;

/// Fallible replacement for `alloc::collections::BinaryHeap`.
pub type BinaryHeap<T> = KBinaryHeap<T>;

/// Fallible replacement for `hashbrown::HashMap`.
pub type HashMap<K, V, S = hashbrown::DefaultHashBuilder> = KHashMap<K, V, S>;

/// Fallible replacement for `alloc::collections::BTreeMap`.
pub type BTreeMap<K, V> = KBTreeMap<K, V>;

/// Fallible replacement for `alloc::collections::BTreeSet`.
pub type BTreeSet<T> = KBTreeSet<T>;
pub use kvecdeque::KVecDeque;
pub use try_traits::{
    TryBitAnd, TryBitOr, TryBitXor, TryClone, TryCollect, TryExtend, TryFromIterator, TrySub,
};

/// Construct a [`KVec<T>`] from a list of elements or a repeated expression,
/// analogous to `alloc::vec!`, but fallible.
///
/// Uses [`AllocContext::Sleepable`] and returns [`Result`]`<KVec<T>, `[`AllocError`]`>`.
/// Use `.unwrap()` in tests or startup code where OOM is unrecoverable.
///
/// # Examples
///
/// ```ignore
/// let v = kvec![1, 2, 3].unwrap();
/// let z = kvec![0u8; 16].unwrap();
///
/// // On user-triggerable paths, propagate the error:
/// let v = kvec![a, b, c].map_err(|_| KError::NoMemory)?;
/// ```
#[macro_export]
macro_rules! kvec {
    // kvec![elem1, elem2, ...]
    ($($x:expr),+ $(,)?) => {{
        (|| -> Result<$crate::KVec<_>, $crate::AllocError> {
            let mut v = $crate::KVec::try_with_capacity(
                $crate::kvec_count!($($x),+),
                $crate::AllocContext::Sleepable,
            )?;
            $( v.try_push($x, $crate::AllocContext::Sleepable)?; )+
            Ok(v)
        })()
    }};
    // kvec![expr; count]
    ($elem:expr; $n:expr) => {{
        (|| -> Result<$crate::KVec<_>, $crate::AllocError> {
            let mut v = $crate::KVec::try_with_capacity($n, $crate::AllocContext::Sleepable)?;
            for _ in 0..$n {
                v.try_push($elem.clone(), $crate::AllocContext::Sleepable)?;
            }
            Ok(v)
        })()
    }};
    // kvec![]
    () => {
        Ok::<$crate::KVec<_>, $crate::AllocError>($crate::KVec::new())
    };
}
/// Fallible alternative to `alloc::vec!`, returns `Result<KVec<T>, AllocError>`.
#[macro_export]
macro_rules! vec {
    ($($x:expr),+ $(,)?) => { $crate::kvec!($($x),+) };
    ($elem:expr; $n:expr) => { $crate::kvec!($elem; $n) };
    () => { $crate::kvec!() };
}

/// Construct a [`KString`] from a format string, analogous to `format!`, but fallible.
///
/// Returns `Result<KString, AllocError>`. The first argument is the
/// [`AllocContext`] (typically [`AllocContext::Sleepable`]).
///
/// # Examples
///
/// ```ignore
/// let msg = kformat!(AllocContext::Sleepable, "error code={}", e)?;
/// let info = kformat!(ctx, "{}:{}", file, line)?;
/// ```
///
/// Panics do not occur on OOM — the error propagates to the caller.
#[macro_export]
macro_rules! kformat {
    ($ctx:expr, $($arg:tt)*) => {{
        let mut s = $crate::KString::new();
        match s.try_push_fmt(format_args!($($arg)*), $ctx) {
            Ok(()) => Ok(s),
            Err(e) => Err(e),
        }
    }};
}

/// Fallible alternative to `format!`, returns `Result<KString, AllocError>`.
///
/// This shadows the standard `format!` macro when imported from this crate,
/// using [`AllocContext::Sleepable`] as the default context.
///
/// # Examples
///
/// ```ignore
/// use kalloc_types::format;
/// let msg = format!("error code={}", e)?;
/// ```
#[macro_export]
macro_rules! format {
    ($($arg:tt)*) => {
        $crate::kformat!($crate::AllocContext::Sleepable, $($arg)*)
    };
}

/// Count the number of expressions in a comma-separated list without
/// evaluating any of them. Used internally by [`kvec!`].
#[doc(hidden)]
#[macro_export]
macro_rules! kvec_count {
    () => (0usize);
    ($head:expr $(, $rest:expr)*) => (1usize + $crate::kvec_count!($($rest),*));
}

#[cfg(unittest)]
mod integration_tests {
    use unittest::def_test;

    use super::{
        AllocContext, AllocError, KArc, KBTreeMap, KBTreeSet, KBinaryHeap, KBox, KHashMap, KString,
        KVec, KVecDeque, KWeak,
    };
    use crate::fault_injection::fail_allocations_after;

    /// A small composition of the allocation shapes MM needs while it builds
    /// mapping metadata. This is intentionally local to the test module: it
    /// validates the shared type contract without creating a second MM model.
    struct MappingPlan {
        label: KString,
        page_frames: KVec<u64>,
        deferred_invalidations: KVecDeque<u64>,
        reclaim_priority: KBinaryHeap<u64>,
        intervals: KBTreeMap<u64, KBox<u64>>,
        resident_starts: KBTreeSet<u64>,
        views: KHashMap<u64, KArc<u64>>,
        owner: KArc<u64>,
        owner_weak: KWeak<u64>,
    }

    fn try_build_mapping_plan() -> Result<MappingPlan, AllocError> {
        let label = KString::try_from_str("anonymous-mapping", AllocContext::Sleepable)?;

        let mut page_frames = KVec::try_with_capacity(2, AllocContext::Sleepable)?;
        page_frames.try_push(0x1000, AllocContext::Atomic)?;
        page_frames.try_push(0x2000, AllocContext::Atomic)?;

        let mut deferred_invalidations = KVecDeque::try_with_capacity(2, AllocContext::Sleepable)?;
        deferred_invalidations.try_push_back(0x3000, AllocContext::Atomic)?;
        deferred_invalidations.try_push_front(0x1000, AllocContext::Atomic)?;

        let mut reclaim_priority = KBinaryHeap::try_with_capacity(2, AllocContext::Sleepable)?;
        reclaim_priority.try_push(4, AllocContext::Atomic)?;
        reclaim_priority.try_push(9, AllocContext::Atomic)?;

        let owner = KArc::try_new(0xfeed_u64, AllocContext::Sleepable)?;
        let owner_weak = KArc::downgrade(&owner);

        let mut intervals = KBTreeMap::new();
        let interval = KBox::try_new(0x4000_u64, AllocContext::Sleepable)?;
        intervals.try_insert_in(0x4000, interval, AllocContext::Sleepable)?;

        let mut resident_starts = KBTreeSet::new();
        resident_starts.try_insert_in(0x4000, AllocContext::Sleepable)?;

        let mut views = KHashMap::try_with_capacity(1, AllocContext::Sleepable)?;
        views.try_insert(7, owner.clone(), AllocContext::Atomic)?;

        Ok(MappingPlan {
            label,
            page_frames,
            deferred_invalidations,
            reclaim_priority,
            intervals,
            resident_starts,
            views,
            owner,
            owner_weak,
        })
    }

    #[def_test]
    fn mapping_plan_composes_every_fallible_heap_type() {
        let mut plan = try_build_mapping_plan().unwrap();

        assert_eq!(plan.label.as_str(), "anonymous-mapping");
        assert_eq!(plan.page_frames.as_slice(), &[0x1000, 0x2000]);
        assert_eq!(plan.deferred_invalidations.pop_front(), Some(0x1000));
        assert_eq!(plan.deferred_invalidations.pop_back(), Some(0x3000));
        assert_eq!(plan.reclaim_priority.pop(), Some(9));
        assert_eq!(
            plan.intervals.get(&0x4000).map(|value| **value),
            Some(0x4000)
        );
        assert!(plan.resident_starts.contains(&0x4000));

        let view_owner = plan.views.get(&7).expect("view must retain its owner");
        assert!(KArc::ptr_eq(&plan.owner, view_owner));
        let upgraded = plan.owner_weak.upgrade().expect("owner is still live");
        assert!(KArc::ptr_eq(&plan.owner, &upgraded));
    }

    #[def_test(serial)]
    fn mapping_plan_propagates_injected_oom_from_every_allocation_stage() {
        let mut saw_oom = false;
        let mut saw_success = false;

        // Each custom container invokes the test-only hook before allocating.
        // Try every possible failure point in this bounded plan, then one or
        // more points after its final allocation. A failed build returns no
        // partially published plan: all local components are dropped first.
        for successful_allocations in 0..32 {
            let guard = fail_allocations_after(successful_allocations);
            match try_build_mapping_plan() {
                Err(AllocError::NoMemory) => saw_oom = true,
                Err(error) => panic!("unexpected allocation error: {error:?}"),
                Ok(plan) => {
                    saw_success = true;
                    drop(plan);
                }
            }
            drop(guard);
        }

        assert!(saw_oom, "injection must reach at least one allocation site");
        assert!(saw_success, "the bounded plan must eventually build");
    }
}
