// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]

//! Lightweight 9P client library for no_std targets.

extern crate alloc;

mod message;
mod parse;
mod protocol;
mod session;
mod transport;

pub use session::{FileAttr, P9DirEntry, P9Session as Session};
pub use transport::Transport;

#[cfg(feature = "fuzz")]
pub mod fuzz {
    //! Pure decode helpers for WFUZZ (not part of the normal client API).
    pub use crate::parse::{parse_dir_entries, parse_dir_entries_l, path_parts, split_parent_name};
}
