// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Memory controller: binds one cgroup to one accounting node.
//!
//! Page ownership is tracked by `memacct`, not here. This controller exists so
//! the generic delegation machinery can create the accounting node at the
//! right place in the memory hierarchy and so control files can find it.

use alloc::sync::Arc;

use memacct::Memcg;

use super::{Controller, ControllerKind};

/// Per-cgroup memory controller.
pub struct MemoryController {
    memcg: Arc<Memcg>,
}

impl MemoryController {
    /// Wraps the accounting node owned by this cgroup.
    pub fn new(memcg: Arc<Memcg>) -> Self {
        Self { memcg }
    }

    /// Returns the accounting node owned by this cgroup.
    pub fn memcg(&self) -> Arc<Memcg> {
        Arc::clone(&self.memcg)
    }
}

impl Controller for MemoryController {
    fn kind(&self) -> ControllerKind {
        ControllerKind::Memory
    }

    // Memory charges follow pages, not tasks: a task that migrates keeps the
    // pages it already faulted charged to their original cgroup, while new
    // faults are charged to the destination. Per-task hooks therefore carry no
    // memory state.
}
