// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use tee_raw_sys::TEE_ERROR_BAD_PARAMETERS;

use crate::tee::TeeResult;

/// Largest user buffer this stub will accept.
///
/// TA mappings are not walked yet, so this only stops `uaddr + len` wrap and
/// `Vec::with_capacity` of a TA-chosen `u64::MAX` from taking down the kernel.
pub const TEE_MAX_CHECKED_USER_COPY: usize = 1024 * 1024;

/// Check that a user copy size is representable before allocating.
///
/// Privilege bits in `flags` are not enforced until TA VMAs are available.
pub fn vm_check_access_rights(_flags: u32, uaddr: usize, len: usize) -> TeeResult {
    if len == 0 {
        return Ok(());
    }
    if len > TEE_MAX_CHECKED_USER_COPY || uaddr.checked_add(len).is_none() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    Ok(())
}
