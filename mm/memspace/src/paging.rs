// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Architecture-neutral page-table integration helpers.

use kerrno::KError;
use khal::paging::PagingError;

/// Converts the page-table layer's error vocabulary to memspace errors.
pub(crate) fn map_paging_err(err: PagingError) -> KError {
    match err {
        PagingError::NoMemory => KError::NoMemory,
        _ => KError::InvalidInput,
    }
}
