// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fallible B-tree map and set, adapted from std's BTreeMap implementation
//! with explicit allocation-failure handling via [`KBox`].
//!
//! These are forked from standard library / fallible_collections sources.
//! Pre-existing style issues from the upstream code are suppressed.

#![allow(clippy::empty_line_after_doc_comments)]
#![allow(clippy::multiple_bound_locations)]

pub(crate) mod map;
mod node;
mod search;
pub(crate) mod set;

#[doc(hidden)]
pub(crate) trait Recover<Q: ?Sized> {
    type Key;

    fn get(&self, key: &Q) -> Option<&Self::Key>;
    fn take(&mut self, key: &Q) -> Option<Self::Key>;
    fn replace(&mut self, key: Self::Key) -> Result<Option<Self::Key>, crate::AllocError>;
}
