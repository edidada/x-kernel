// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

/// Error returned by explicit kernel heap allocation APIs.
///
/// Phase 1 uses a small closed set so callers can distinguish a true
/// out-of-memory condition from a future unsupported allocation-context error
/// without depending on backend-specific allocator details.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AllocError {
    /// The requested allocation could not be satisfied with current memory.
    NoMemory,
    /// The caller requested an allocation context that is not implemented yet.
    UnsupportedContext,
}

impl AllocError {
    /// Maps the allocation error into the canonical kernel error domain.
    pub const fn into_kerror(self) -> kerrno::KError {
        match self {
            Self::NoMemory | Self::UnsupportedContext => kerrno::KError::NoMemory,
        }
    }
}

impl From<AllocError> for kerrno::KError {
    fn from(error: AllocError) -> Self {
        error.into_kerror()
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::AllocError;

    #[def_test]
    fn maps_all_phase1_variants_to_nomemory() {
        assert_eq!(AllocError::NoMemory.into_kerror(), kerrno::KError::NoMemory);
        assert_eq!(
            AllocError::UnsupportedContext.into_kerror(),
            kerrno::KError::NoMemory
        );
    }

    #[def_test]
    fn from_alloc_error_matches_method() {
        assert_eq!(
            kerrno::KError::from(AllocError::NoMemory),
            AllocError::NoMemory.into_kerror()
        );
        assert_eq!(
            kerrno::KError::from(AllocError::UnsupportedContext),
            AllocError::UnsupportedContext.into_kerror()
        );
    }
}
