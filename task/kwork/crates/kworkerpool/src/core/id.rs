// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Worker identifiers.

/// Pool-local worker slot identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerId(usize);

impl WorkerId {
    /// Creates a worker id.
    pub const fn new(id: usize) -> Self {
        Self(id)
    }

    /// Returns the slot index.
    pub const fn as_usize(self) -> usize {
        self.0
    }
}
