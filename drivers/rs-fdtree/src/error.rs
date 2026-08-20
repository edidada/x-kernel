// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Error types for minimal FDT parsing.

/// Possible errors when working with a Flattened Device Tree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FdtError {
    /// The FDT magic number was not `0xd00dfeed`.
    BadMagic,
    /// A null pointer was passed to [`crate::LinuxFdt::from_ptr`].
    BadPtr,
    /// The buffer was shorter than the header or `totalsize`.
    BufferTooSmall,
    /// A header offset or block size fell outside the FDT blob.
    BadOffset,
}

impl core::fmt::Display for FdtError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FdtError::BadMagic => write!(f, "bad FDT magic value"),
            FdtError::BadPtr => write!(f, "an invalid pointer was passed"),
            FdtError::BufferTooSmall => write!(f, "the given buffer was too small"),
            FdtError::BadOffset => write!(f, "an FDT block offset was out of range"),
        }
    }
}

/// Convenience type alias for Result with FdtError
#[allow(dead_code)]
pub type Result<T> = core::result::Result<T, FdtError>;
