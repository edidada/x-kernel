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
pub use kvecdeque::KVecDeque;
pub use try_traits::{
    TryBitAnd, TryBitOr, TryBitXor, TryClone, TryExtend, TryFromIterator, TrySub,
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
            let mut v = $crate::KVec::new();
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
