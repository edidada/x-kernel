// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Working directory context
//!
//! Lightweight state management for root and current working directory.

use kvfs::{Location, VfsError, VfsResult};

/// Returns whether `location` is the root itself or one of its descendants.
pub(crate) fn is_within_root(root: &Location, location: &Location) -> bool {
    let mut current = location.clone();
    loop {
        if current.ptr_eq(root) {
            return true;
        }
        let Some(parent) = current.parent() else {
            return false;
        };
        current = parent;
    }
}

/// Working context - manages root and current working directory
///
/// This is a lightweight structure that only holds directory state.
/// It does not contain any path resolution or file operation logic.
#[derive(Debug, Clone)]
pub struct WorkingContext {
    root_dir: Location,
    current_dir: Location,
}

impl WorkingContext {
    /// Creates a new working context with the given root directory
    ///
    /// Initially, both root and current directory point to the same location.
    #[inline]
    pub fn new(root: Location) -> Self {
        Self {
            root_dir: root.clone(),
            current_dir: root,
        }
    }

    /// Returns a reference to the root directory
    #[inline]
    pub fn root(&self) -> &Location {
        &self.root_dir
    }

    /// Returns a reference to the current working directory
    #[inline]
    pub fn cwd(&self) -> &Location {
        &self.current_dir
    }

    /// Changes the current working directory
    ///
    /// # Errors
    /// Returns `NotADirectory` if the target is not a directory, or
    /// `PermissionDenied` if it is outside this context's root.
    pub fn chdir(&mut self, dir: Location) -> VfsResult<()> {
        dir.check_is_dir()?;
        if !is_within_root(&self.root_dir, &dir) {
            return Err(VfsError::PermissionDenied);
        }
        self.current_dir = dir;
        Ok(())
    }

    /// Creates a new context with a different current working directory
    ///
    /// This is an immutable version of `chdir` that returns a new context
    /// instead of modifying the existing one.
    ///
    /// # Errors
    /// Returns `NotADirectory` if the target is not a directory, or
    /// `PermissionDenied` if it is outside this context's root.
    pub fn with_cwd(&self, dir: Location) -> VfsResult<Self> {
        dir.check_is_dir()?;
        if !is_within_root(&self.root_dir, &dir) {
            return Err(VfsError::PermissionDenied);
        }
        Ok(Self {
            root_dir: self.root_dir.clone(),
            current_dir: dir,
        })
    }
}
