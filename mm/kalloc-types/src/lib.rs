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
pub type Arc<T> = KArc<T>;
pub type Vec<T> = KVec<T>;
pub type String = KString;
pub type Box<T> = KBox<T>;
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
