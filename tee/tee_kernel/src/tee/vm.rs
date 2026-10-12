// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::mem::MaybeUninit;

use osvm::{MemError, read_vm_bytes, write_vm_bytes};
use tee_raw_sys::{
    TEE_ERROR_ACCESS_DENIED, TEE_ERROR_BAD_PARAMETERS, TEE_MEMORY_ACCESS_READ,
    TEE_MEMORY_ACCESS_WRITE,
};

use crate::tee::TeeResult;

/// Largest user buffer this stub will accept.
///
/// TA mappings are not walked yet, so this only stops `uaddr + len` wrap and
/// `Vec::with_capacity` of a TA-chosen `u64::MAX` from taking down the kernel.
pub const TEE_MAX_CHECKED_USER_COPY: usize = 1024 * 1024;

fn map_mem_error(err: MemError) -> u32 {
    match err {
        MemError::InvalidAddr | MemError::NoAccess => TEE_ERROR_ACCESS_DENIED,
        MemError::NameTooLong => TEE_ERROR_BAD_PARAMETERS,
    }
}

/// Check that a user copy size is representable and the range is accessible for `flags`.
pub fn vm_check_access_rights(flags: u32, uaddr: usize, len: usize) -> TeeResult {
    if len == 0 {
        return Ok(());
    }
    if len > TEE_MAX_CHECKED_USER_COPY || uaddr.checked_add(len).is_none() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let want_read = (flags & TEE_MEMORY_ACCESS_READ) != 0;
    let want_write = (flags & TEE_MEMORY_ACCESS_WRITE) != 0;
    if !want_read && !want_write {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let base = uaddr as *const u8;
    let mut probe = MaybeUninit::<u8>::uninit();
    let probe_slice = core::slice::from_mut(&mut probe);

    if want_read {
        read_vm_bytes(base, probe_slice).map_err(map_mem_error)?;
        if len > 1 {
            // SAFETY: `uaddr` and `len` validated above; last byte stays in range.
            let last = unsafe { base.add(len - 1) };
            read_vm_bytes(last, probe_slice).map_err(map_mem_error)?;
        }
    }
    if want_write {
        let mut_base = uaddr as *mut u8;
        write_vm_bytes(mut_base, &[]).map_err(map_mem_error)?;
        if len > 1 {
            // SAFETY: same range invariant as the read probe above.
            let last = unsafe { mut_base.add(len - 1) };
            write_vm_bytes(last, &[]).map_err(map_mem_error)?;
        }
    }

    Ok(())
}
