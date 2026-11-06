// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device-tree node availability policy.

use crate::{FdtNode, PropertyError};

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/availability.rs");

/// Interpret this node's status without applying ancestor or population policy.
///
/// Missing status, `ok` and `okay` mean available. Other strings mean
/// unavailable; malformed or duplicate properties return an error. This is
/// allocation-free and does not imply that a controller or driver is ready.
pub fn is_available(node: FdtNode<'_, '_>) -> Result<bool, PropertyError> {
    match node.read_string("status") {
        Ok(status) => Ok(matches!(status, "ok" | "okay")),
        Err(PropertyError::Missing) => Ok(true),
        Err(error) => Err(error),
    }
}
