// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fallible cloning trait, used by KBTreeMap for clone operations
//! that may allocate. Adapted from fallible_collections.

use crate::AllocError;

/// Fallible clone — like `Clone` but returns `Result` to allow allocation
/// failure to propagate.
pub trait TryClone: Sized {
    fn try_clone(&self) -> Result<Self, AllocError>;
}

macro_rules! impl_try_clone_copy {
    ($($t:ty),*) => {
        $(impl TryClone for $t {
            #[inline(always)]
            fn try_clone(&self) -> Result<Self, AllocError> {
                Ok(*self)
            }
        })*
    };
}

impl_try_clone_copy!(u8, u16, u32, u64, i8, i16, i32, i64, usize, isize, bool);

impl<T: TryClone> TryClone for Option<T> {
    #[inline]
    fn try_clone(&self) -> Result<Self, AllocError> {
        Ok(match self {
            Some(t) => Some(t.try_clone()?),
            None => None,
        })
    }
}
