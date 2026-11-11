// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Async I/O submission/completion syscalls.
//!
//! `asyncio_setup` creates the context ring and returns a descriptor;
//! `asyncio_enter` submits SQEs, grows the worker pool, and optionally waits
//! for completions. Syscall-number ABI decoding stays here; ring and worker
//! state belong to `kasyncio`.

use kasyncio::{
    AsyncioFile,
    abi::{
        ASYNCIO_CQE_SIZE, ASYNCIO_ENTER_FLAGS_KNOWN, ASYNCIO_ENTER_TIMEOUT, ASYNCIO_ENTER_WAIT,
        ASYNCIO_PARAMS_VERSION, ASYNCIO_SQE_SIZE, AsyncioParams,
    },
    setup,
};
use kerrno::{KError, KResult};
use linux_raw_sys::general::__kernel_timespec;
use posix_types::{TimeSpanLike, UserConstPtr, UserPtr};

/// Creates an async context with an `entries`-deep submission ring.
///
/// `entries` must be a power of two no larger than `ASYNCIO_MAX_SQ_ENTRIES`.
/// The ring is mapped into the caller's address space and the out-fields of
/// `params` are filled in before the descriptor is installed.
pub(crate) fn sys_asyncio_setup(entries: u32, params: UserPtr<AsyncioParams>) -> KResult<isize> {
    let params = params.check_non_null().ok_or(KError::BadAddress)?;
    let mut value = params.read_vm()?;
    if value.version != ASYNCIO_PARAMS_VERSION || value.flags != 0 {
        return Err(KError::InvalidInput);
    }
    // Fail an unwritable parameter block before allocating a ring, instead of
    // leaving a mapping behind after the setup work succeeds.
    params.write_vm(value)?;

    let created = setup(entries, kprocess::current_cred())?;
    value.sq_entries = created.sq_entries;
    value.cq_entries = created.cq_entries;
    value.ring_user_va = created.ring_user_va;
    value.ring_user_len = created.ring_user_len;
    value.sqe_size = ASYNCIO_SQE_SIZE as u32;
    value.cqe_size = ASYNCIO_CQE_SIZE as u32;

    let fd = kprocess::current_files().add_file(created.file, false)?;
    if let Err(error) = params.write_vm(value) {
        let _ = kprocess::current_files().close_file(fd);
        return Err(error.into());
    }
    Ok(fd as isize)
}

/// Submits up to `to_submit` SQEs and optionally waits for completions.
///
/// `min_complete` is honoured only with `ASYNCIO_ENTER_WAIT`; a timeout
/// requires both `ASYNCIO_ENTER_TIMEOUT` and `ASYNCIO_ENTER_WAIT` and reads
/// a non-negative `__kernel_timespec` from `timeout`.
pub(crate) fn sys_asyncio_enter(
    fd: i32,
    to_submit: u32,
    min_complete: u32,
    flags: u32,
    timeout: UserConstPtr<__kernel_timespec>,
) -> KResult<isize> {
    if flags & !ASYNCIO_ENTER_FLAGS_KNOWN != 0 {
        return Err(KError::InvalidInput);
    }
    let wait = flags & ASYNCIO_ENTER_WAIT != 0;
    let timed = flags & ASYNCIO_ENTER_TIMEOUT != 0;
    if timed && !wait {
        return Err(KError::InvalidInput);
    }
    let timeout = if timed {
        let timeout = timeout.check_non_null().ok_or(KError::BadAddress)?;
        Some(timeout.read_vm()?.try_into_time_span()?)
    } else {
        None
    };

    let file = kprocess::current_files().get_file(fd)?;
    let asyncio = AsyncioFile::from_file(&file)?;
    let submitted = asyncio.enter(to_submit, if wait { min_complete } else { 0 }, timeout)?;
    Ok(submitted as isize)
}

unittest::path_mod_test!(tests, "../../ut-tests/io_mpx/asyncio.rs");
