// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.
//
// This file has been modified by KylinSoft on 2025.

use std::{
    alloc::{Layout, alloc, alloc_zeroed, realloc},
    ffi::c_void,
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

use crate::{
    api::tee_api_panic::TEE_Panic,
    tee_api_defines::{TEE_MALLOC_FILL_ZERO, TEE_MALLOC_NO_FILL, TEE_MALLOC_NO_SHARE, TEE_SUCCESS},
    tee_api_types::TEE_Result,
};

static TEE_API_INSTANCE_DATA: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

/*
 * return a known non-NULL invalid pointer when the
 * requested size is zeros
 */
pub const TEE_NULL_SIZED_VA: *mut c_void = 1 as *mut c_void;
pub const TEE_NULL_SIZED_NO_SHARE_VA: *mut c_void = 2 as *mut c_void;

/* Hint implementation defines */
pub const TEE_USER_MEM_HINT_NO_FILL_ZERO: u32 = 0x80000000;

#[unsafe(no_mangle)]
pub extern "C" fn TEE_CheckMemoryAccessRights(
    _accessFlags: u32,
    _buffer: *mut c_void,
    _size: usize,
) -> TEE_Result {
    TEE_SUCCESS
}

#[unsafe(no_mangle)]
pub extern "C" fn TEE_SetInstanceData(instanceData: *const c_void) {
    TEE_API_INSTANCE_DATA.store(instanceData as _, Ordering::Release);
}
#[unsafe(no_mangle)]
pub extern "C" fn TEE_GetInstanceData() -> *const c_void {
    TEE_API_INSTANCE_DATA.load(Ordering::Acquire)
}

#[unsafe(no_mangle)]
pub extern "C" fn TEE_Malloc(size: usize, hint: u32) -> *mut c_void {
    let layout = match Layout::from_size_align(size, std::mem::align_of::<u8>()) {
        Ok(layout) => layout,
        Err(_) => return ptr::null_mut(),
    };

    match hint {
        TEE_MALLOC_FILL_ZERO => {
            if size == 0 {
                return TEE_NULL_SIZED_VA;
            }
            unsafe { alloc_zeroed(layout) as _ }
        }
        TEE_MALLOC_NO_FILL => {
            TEE_Panic(0);
            ptr::null_mut()
        }
        TEE_MALLOC_NO_SHARE => {
            if size == 0 {
                return TEE_NULL_SIZED_NO_SHARE_VA;
            }
            unsafe { alloc_zeroed(layout) as _ }
        }
        hint if hint == TEE_MALLOC_NO_FILL | TEE_MALLOC_NO_SHARE => {
            if size == 0 {
                return TEE_NULL_SIZED_NO_SHARE_VA;
            }
            unsafe { alloc(layout) as _ }
        }
        TEE_USER_MEM_HINT_NO_FILL_ZERO => {
            if size == 0 {
                return TEE_NULL_SIZED_VA;
            }
            unsafe { alloc(layout) as _ }
        }
        _ => {
            eprintln!("Invalid hint: 0x{:x}", hint);
            ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn TEE_Realloc(buffer: *mut c_void, newSize: usize) -> *mut c_void {
    let layout = match Layout::from_size_align(newSize, std::mem::align_of::<u8>()) {
        Ok(layout) => layout,
        Err(_) => return ptr::null_mut(),
    };

    if newSize == 0 {
        TEE_Free(buffer);
    }

    unsafe { realloc(buffer as _, layout, newSize) as _ }
}

#[unsafe(no_mangle)]
pub extern "C" fn TEE_Free(buffer: *mut c_void) {
    unsafe { libc::free(buffer) }
}

#[unsafe(no_mangle)]
pub extern "C" fn TEE_MemMove(dest: *mut c_void, src: *const c_void, size: usize) {
    unsafe { libc::memmove(dest, src, size) };
}

#[unsafe(no_mangle)]
pub extern "C" fn TEE_MemCompare(
    _buffer1: *const c_void,
    _buffer2: *const c_void,
    _size: usize,
) -> i32 {
    -1
}

#[unsafe(no_mangle)]
pub extern "C" fn TEE_MemFill(buff: *mut c_void, x: u32, size: usize) {
    unsafe { libc::memset(buff, x as _, size) };
}
