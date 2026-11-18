// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! v9fs mount options (`msize=`, `cache=`, `debug=`).

use kvfs::{VfsError, VfsResult};
use memaddr::PAGE_SIZE_4K;

/// Smallest `msize` Linux `p9_client_create()` accepts.
const MIN_MSIZE_BYTES: u32 = 4096;
/// Size of the header of a 9P read or write message (`P9_IOHDRSZ`).
const IO_HEADER_BYTES: u32 = 24;
/// Default `msize` of the Linux 9P client (`DEFAULT_MSIZE`).
const DEFAULT_MSIZE_BYTES: u32 = 128 * 1024 + IO_HEADER_BYTES;

/// Caching policy selected with `cache=`, by the legacy Linux mode names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum V9fsCacheMode {
    /// No caching: every operation goes to the server.
    None,
    /// Cache file data only while a file is open for mmap.
    Mmap,
    /// Data and metadata caching with relaxed consistency.
    Loose,
    /// Loose caching backed by a persistent local cache.
    Fscache,
    /// Cache file data for read-ahead only.
    Readahead,
}

impl V9fsCacheMode {
    /// Parses a `cache=` value.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` for a mode name Linux does not define.
    pub fn from_option(value: &[u8]) -> VfsResult<Self> {
        match value {
            b"none" => Ok(Self::None),
            b"mmap" => Ok(Self::Mmap),
            b"loose" => Ok(Self::Loose),
            b"fscache" => Ok(Self::Fscache),
            b"readahead" => Ok(Self::Readahead),
            _ => Err(VfsError::InvalidInput),
        }
    }
}

/// Options of one v9fs mount.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct V9fsMountOptions {
    /// Largest 9P message the client may negotiate, header included.
    pub msize_bytes: u32,
    /// Caching policy.
    pub cache_mode: V9fsCacheMode,
    /// `P9_DEBUG_*` categories to trace.
    pub debug_mask: u32,
}

impl V9fsMountOptions {
    /// Options for a mount that names only its message size.
    pub const fn new(msize_bytes: u32) -> Self {
        Self {
            msize_bytes,
            cache_mode: V9fsCacheMode::None,
            debug_mask: 0,
        }
    }

    /// Parses legacy `mount(2)` data.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` for an unknown option, a malformed number or an
    /// `msize` below what the 9P client can work with.
    pub fn parse(data: &[u8]) -> VfsResult<Self> {
        let mut options = Self::new(DEFAULT_MSIZE_BYTES);
        for option in mount_data_options(data) {
            if let Some(value) = option.strip_prefix(b"msize=") {
                options.msize_bytes = parse_decimal_u32(value).ok_or(VfsError::InvalidInput)?;
            } else if let Some(value) = option.strip_prefix(b"cache=") {
                options.cache_mode = V9fsCacheMode::from_option(value)?;
            } else if let Some(value) = option.strip_prefix(b"debug=") {
                options.debug_mask = parse_decimal_u32(value).ok_or(VfsError::InvalidInput)?;
            } else {
                return Err(VfsError::InvalidInput);
            }
        }
        if options.msize_bytes < MIN_MSIZE_BYTES {
            return Err(VfsError::InvalidInput);
        }
        Ok(options)
    }

    /// Largest whole-page payload one read message can carry, the read-ahead
    /// step for this mount. Zero when `msize` leaves room for less than a page.
    pub fn readahead_bytes(&self) -> usize {
        let payload_bytes = self.msize_bytes.saturating_sub(IO_HEADER_BYTES);
        let payload_bytes = usize::try_from(payload_bytes).unwrap_or(usize::MAX);
        align_down_bytes(payload_bytes, PAGE_SIZE_4K)
    }

    /// Number of debug categories enabled by `debug=`.
    pub fn debug_category_count(&self) -> u32 {
        let mut remaining = self.debug_mask;
        let mut categories = 0;
        while remaining != 0 {
            categories += remaining & 1;
            remaining >>= 1;
        }
        categories
    }
}

/// Rounds `size_bytes` down to a multiple of `align_bytes`.
///
/// # Panics
///
/// Panics when `align_bytes` is not a non-zero power of two.
const fn align_down_bytes(size_bytes: usize, align_bytes: usize) -> usize {
    assert!(
        align_bytes.is_power_of_two(),
        "alignment must be a non-zero power of two"
    );
    let offset_in_block = size_bytes & (align_bytes - 1);
    size_bytes - offset_in_block
}

/// Splits legacy `mount(2)` data into its comma-separated options.
///
/// The buffer is NUL-padded to a fixed size, so only the text before the first
/// NUL is option text. Empty options are skipped.
fn mount_data_options(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let text_len = data
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(data.len());
    data[..text_len]
        .split(|byte| *byte == b',')
        .filter(|option| !option.is_empty())
}

/// Parses a numeric option value as a non-negative decimal number.
///
/// Returns `None` for an empty value, a leading zero on a value of more than
/// one digit, a sign, a non-digit, or a value that does not fit in `u32`, so
/// `msize=010` is rejected rather than guessed to be octal or decimal.
fn parse_decimal_u32(value: &[u8]) -> Option<u32> {
    if value.is_empty() || (value.len() > 1 && value[0] == b'0') {
        return None;
    }
    let mut number: u32 = 0;
    for &byte in value {
        let digit = byte.checked_sub(b'0').filter(|digit| *digit <= 9)?;
        number = number.checked_mul(10)?.checked_add(u32::from(digit))?;
    }
    Some(number)
}
