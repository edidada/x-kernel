// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fallible alternatives to standard infallible traits.
//!
//! Standard traits like `Clone`, `Extend`, and `FromIterator` cannot return
//! `Result`, so kernel code that wants to survive OOM must use the fallible
//! alternatives defined here instead.

use crate::AllocError;

/// Fallible clone — like `Clone` but returns `Result` to allow allocation
/// failure to propagate.
pub trait TryClone: Sized {
    fn try_clone(&self) -> Result<Self, AllocError>;
}

/// Fallible extend — like `Extend` but returns `Result` to allow allocation
/// failure to propagate.
pub trait TryExtend<A> {
    fn try_extend<I: IntoIterator<Item = A>>(&mut self, iter: I) -> Result<(), AllocError>;
}

/// Fallible from-iterator — like `FromIterator` but returns `Result` to allow
/// allocation failure to propagate.
pub trait TryFromIterator<A>: Sized {
    fn try_from_iter<I: IntoIterator<Item = A>>(iter: I) -> Result<Self, AllocError>;
}

/// Fallible set difference — like `Sub` (`-`) but returns `Result`.
pub trait TrySub<Rhs = Self>: Sized {
    type Output;
    fn try_sub(self, rhs: Rhs) -> Result<Self::Output, AllocError>;
}

/// Fallible symmetric difference — like `BitXor` (`^`) but returns `Result`.
pub trait TryBitXor<Rhs = Self>: Sized {
    type Output;
    fn try_bitxor(self, rhs: Rhs) -> Result<Self::Output, AllocError>;
}

/// Fallible intersection — like `BitAnd` (`&`) but returns `Result`.
pub trait TryBitAnd<Rhs = Self>: Sized {
    type Output;
    fn try_bitand(self, rhs: Rhs) -> Result<Self::Output, AllocError>;
}

/// Fallible union — like `BitOr` (`|`) but returns `Result`.
pub trait TryBitOr<Rhs = Self>: Sized {
    type Output;
    fn try_bitor(self, rhs: Rhs) -> Result<Self::Output, AllocError>;
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

impl TryClone for () {
    #[inline(always)]
    fn try_clone(&self) -> Result<Self, AllocError> {
        Ok(())
    }
}

impl<T: TryClone> TryClone for Option<T> {
    #[inline]
    fn try_clone(&self) -> Result<Self, AllocError> {
        Ok(match self {
            Some(t) => Some(t.try_clone()?),
            None => None,
        })
    }
}
