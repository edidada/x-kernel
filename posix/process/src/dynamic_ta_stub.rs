// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unsupported dynamic TA entry point for builds without the AArch64 apploader.

use alloc::vec::Vec;

use kerrno::{KError, KResult};
use khal::uspace::UserContext;
use kprocess::UserThreadRuntimeAction;

/// Reports unsupported dynamic TA execution on configurations without the
/// AArch64 dynamic apploader runtime.
pub fn spawn_dynamic_ta(
    _image: Vec<u8>,
    _dispatch_syscall: impl FnMut(&mut UserContext, &kprocess::Thread) -> UserThreadRuntimeAction
    + Send
    + 'static,
) -> KResult {
    Err(KError::Unsupported)
}
