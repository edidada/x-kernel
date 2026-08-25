// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{BarrierAttachResult, WorkBarrier, WorkBarrierQueue, WorkInstanceId};

/// Workqueue-owned metadata for one currently running worker slot.
///
/// Worker-pool scheduling only needs lifecycle, token, and accounting state.
/// `kwork` keeps the current work identity and flush barriers in this sidecar
/// so they do not leak into the generic worker-pool model.
pub(crate) struct RunningWorkSlot {
    current_work_key: usize,
    current_instance_id: Option<WorkInstanceId>,
    running_barriers: WorkBarrierQueue,
}

impl RunningWorkSlot {
    pub(crate) const fn new() -> Self {
        Self {
            current_work_key: 0,
            current_instance_id: None,
            running_barriers: WorkBarrierQueue::new(),
        }
    }

    pub(crate) fn is_running_instance(&self, work_key: usize, instance_id: WorkInstanceId) -> bool {
        self.current_work_key == work_key && self.current_instance_id == Some(instance_id)
    }

    pub(crate) fn has_current_work(&self) -> bool {
        self.current_instance_id.is_some()
    }

    pub(crate) fn set_current_work(
        &mut self,
        work_key: usize,
        instance_id: WorkInstanceId,
        barriers: alloc::vec::Vec<WorkBarrier>,
    ) {
        self.current_work_key = work_key;
        self.current_instance_id = Some(instance_id);
        let stale = self.running_barriers.take_all();
        debug_assert!(stale.is_empty());
        let appended = self.running_barriers.append_from_vec(barriers);
        debug_assert!(appended);
    }

    pub(crate) fn clear_current_work(&mut self) -> alloc::vec::Vec<WorkBarrier> {
        self.current_work_key = 0;
        self.current_instance_id = None;
        self.running_barriers.take_all()
    }

    pub(crate) fn push_running_barrier(&mut self, barrier: WorkBarrier) -> BarrierAttachResult {
        if self.running_barriers.push_front_linked_barrier(barrier) {
            BarrierAttachResult::Attached
        } else {
            BarrierAttachResult::Full
        }
    }

    #[cfg(unittest)]
    pub(crate) fn current_work_key_for_tests(&self) -> usize {
        self.current_work_key
    }
}
