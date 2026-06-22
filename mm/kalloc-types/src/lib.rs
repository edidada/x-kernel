// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fallible allocation contract types built on top of `kalloc`.
//!
//! This crate defines the public type boundary for explicit heap-allocation
//! failure handling. It intentionally keeps the phase-1 surface small:
//! canonical allocation errors, reserved allocation-context metadata, and the
//! named heap object wrappers that later tasks make operational.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod alloc_error;
mod collect;
mod context;
#[cfg(unittest)]
mod fault_injection;
mod kbox;
mod kvec;

pub use alloc_error::{AllocError, map_alloc_error};
pub use collect::try_collect_vec;
pub use context::AllocContext;
pub use kbox::KBox;
pub use kvec::{KVec, KVecIntoIter};
