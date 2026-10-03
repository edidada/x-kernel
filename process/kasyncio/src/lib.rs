// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Asynchronous I/O submission/completion infrastructure.
//!
//! This crate implements the kernel side of the X-Kernel async I/O model:
//! user/kernel shared submission and completion rings whose operations
//! execute on in-process kernel worker threads, so a blocking I/O no longer
//! parks the submitting user thread. Workers are sibling threads of the
//! submitting process, so user-memory access, credentials, and fd semantics
//! are identical to the synchronous syscall path. The synchronous syscall
//! surface stays untouched; async entry points are incremental.
//!
//! Not goals: io_uring ABI compatibility, kernel-side polling of the
//! submission ring (SQPOLL), and cancellation semantics.

#![no_std]

#[macro_use]
extern crate klogger;

extern crate alloc;

// The abi/context/worker modules are exercised through in-crate unit tests
// until the syscall surface lands; keep non-unittest builds warning-clean.
#[cfg_attr(not(unittest), expect(dead_code))]
mod abi;
#[cfg_attr(not(unittest), expect(dead_code))]
mod context;
#[cfg_attr(not(unittest), expect(dead_code))]
mod worker;

#[cfg(unittest)]
mod tests;
