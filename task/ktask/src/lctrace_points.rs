// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! LCTrace probes on the scheduler paths.
//!
//! Each probe has a real body when the `lctrace` feature is on and an empty
//! one otherwise, so the call sites in `run_queue` stay free of `cfg`
//! attributes. This mirrors how `record_remote_resched` is gated in the same
//! module.

/// Records that `prev_tid` is being switched out in favour of `next_tid`.
#[cfg(feature = "lctrace")]
#[inline]
pub(crate) fn sched_switch(prev_tid: u64, next_tid: u64, prev_state: u8) {
    lctrace::event!(SchedSwitch, prev_tid, next_tid, prev_state);
}

/// Records that `tid` was made runnable on `target_cpu`.
#[cfg(feature = "lctrace")]
#[inline]
pub(crate) fn sched_wakeup(tid: u64, target_cpu: usize) {
    lctrace::event!(SchedWakeup, tid, target_cpu);
}

#[cfg(not(feature = "lctrace"))]
#[inline]
pub(crate) fn sched_switch(_prev_tid: u64, _next_tid: u64, _prev_state: u8) {}

#[cfg(not(feature = "lctrace"))]
#[inline]
pub(crate) fn sched_wakeup(_tid: u64, _target_cpu: usize) {}
