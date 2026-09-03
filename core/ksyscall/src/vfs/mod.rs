// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VFS and path-based syscall adapters.

mod inotify;

pub use inotify::*;
pub use posix_fs::*;
