// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

/// Maximum number of pending work entries one worker pool's storage holds.
///
/// The storage itself is owned by `kworkerpool`; kwork keeps this exported
/// capacity because workqueue users and tests size their pending expectations
/// through the workqueue crate.
pub const MAX_WORKQUEUE_PENDING: usize = kbuild_config::WORKQUEUE_PENDING_CAP;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingBarrierAttach {
    Attached,
    Missing,
    Full,
}
