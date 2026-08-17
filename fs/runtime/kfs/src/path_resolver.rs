// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Path resolution module
//!
//! Provides stateless path resolution functionality, separated from filesystem operations.

use alloc::{borrow::ToOwned, string::String};

use kvfs::{
    Location, NodeType, VfsError, VfsResult,
    path::{Component, Components, Path, PathBuf},
};

use crate::working_context::is_within_root;

/// Default maximum symlink follow depth
pub const DEFAULT_MAX_SYMLINKS: usize = 40;

/// Path resolver - stateless path resolution logic
///
/// This component handles all path resolution logic including:
/// - Absolute and relative path resolution
/// - Symlink following with loop detection
/// - Path component normalization (`.` and `..`)
#[derive(Debug, Clone)]
pub struct PathResolver {
    max_symlinks: usize,
}

impl PathResolver {
    /// Creates a new path resolver with default settings
    #[inline]
    pub fn new() -> Self {
        Self {
            max_symlinks: DEFAULT_MAX_SYMLINKS,
        }
    }

    /// Creates a path resolver with custom max symlink depth
    #[inline]
    pub fn with_max_symlinks(max: usize) -> Self {
        Self { max_symlinks: max }
    }

    /// Resolves a path starting from the given base location
    ///
    /// # Arguments
    /// * `root` - The root boundary for absolute paths and parent traversal
    /// * `base` - The base directory to resolve from (typically root or cwd)
    /// * `path` - The path to resolve
    /// * `follow_symlinks` - Whether to follow symlinks
    ///
    /// # Returns
    /// The resolved `Location`, or an error if the path doesn't exist or a loop is detected
    pub fn resolve(
        &self,
        root: &Location,
        base: &Location,
        path: &Path,
        follow_symlinks: bool,
    ) -> VfsResult<Location> {
        let mut follow_count = 0;
        self.resolve_with_count(root, base, path, follow_symlinks, &mut follow_count)
    }

    /// Internal resolution with symlink counter
    fn resolve_with_count(
        &self,
        root: &Location,
        base: &Location,
        path: &Path,
        follow_symlinks: bool,
        follow_count: &mut usize,
    ) -> VfsResult<Location> {
        let entry_name = path.file_name();
        let mut components = path.components();

        // If path has a file name, we need to resolve parent first
        if entry_name.is_some() {
            components.next_back();
        }

        let dir = self.resolve_components(root, base, components, follow_count)?;
        dir.check_is_dir()?;

        match entry_name {
            Some(name) => {
                if follow_symlinks {
                    self.lookup(root, &dir, name, follow_count)
                } else {
                    dir.lookup_no_follow(name)
                }
            }
            None => Ok(dir),
        }
    }

    /// Resolves a path to its parent directory and entry name
    ///
    /// # Returns
    /// `(parent_directory, entry_name)` tuple
    pub fn resolve_parent(
        &self,
        root: &Location,
        base: &Location,
        path: &Path,
    ) -> VfsResult<(Location, String)> {
        let (dir, name) = self.resolve_inner(root, base, path, &mut 0)?;
        if let Some(name) = name {
            Ok((dir, name.to_owned()))
        } else if dir.ptr_eq(root) {
            Err(VfsError::InvalidInput)
        } else if let Some(parent) = dir.parent() {
            Ok((parent, dir.name().to_owned()))
        } else {
            Err(VfsError::InvalidInput)
        }
    }

    /// Resolves a path that is expected not to exist (for creation)
    ///
    /// Verifies that the parent directory exists but the entry doesn't
    pub fn resolve_nonexistent<'a>(
        &self,
        root: &Location,
        base: &Location,
        path: &'a Path,
    ) -> VfsResult<(Location, &'a str)> {
        let (dir, name) = self.resolve_inner(root, base, path, &mut 0)?;
        if let Some(name) = name {
            Ok((dir, name))
        } else {
            Err(VfsError::InvalidInput)
        }
    }

    /// Internal helper for resolve_parent and resolve_nonexistent
    fn resolve_inner<'a>(
        &self,
        root: &Location,
        base: &Location,
        path: &'a Path,
        follow_count: &mut usize,
    ) -> VfsResult<(Location, Option<&'a str>)> {
        let entry_name = path.file_name();
        let mut components = path.components();
        if entry_name.is_some() {
            components.next_back();
        }
        let dir = self.resolve_components(root, base, components, follow_count)?;
        dir.check_is_dir()?;
        Ok((dir, entry_name))
    }

    /// Resolves path components iteratively (public for internal use)
    #[doc(hidden)]
    pub fn resolve_components_internal(
        &self,
        root: &Location,
        base: &Location,
        components: Components,
        follow_count: &mut usize,
    ) -> VfsResult<Location> {
        self.resolve_components(root, base, components, follow_count)
    }

    /// Resolves path components iteratively
    fn resolve_components(
        &self,
        root: &Location,
        base: &Location,
        components: Components,
        follow_count: &mut usize,
    ) -> VfsResult<Location> {
        if !is_within_root(root, base) {
            return Err(VfsError::PermissionDenied);
        }
        let mut current = base.clone();

        for comp in components {
            match comp {
                Component::CurDir => {
                    // `.` - stay in current directory
                }
                Component::ParentDir => {
                    // `..` cannot cross the process filesystem root.
                    if !current.ptr_eq(root)
                        && let Some(parent) = current.parent()
                    {
                        current = parent;
                    }
                }
                Component::RootDir => {
                    // `/` is relative to the process filesystem root.
                    current = root.clone();
                }
                Component::Normal(name) => {
                    // Regular component - lookup and potentially follow symlink
                    current = self.lookup(root, &current, name, follow_count)?;
                }
            }
        }

        Ok(current)
    }

    /// Looks up a name in a directory and follows symlinks if needed
    fn lookup(
        &self,
        root: &Location,
        dir: &Location,
        name: &str,
        follow_count: &mut usize,
    ) -> VfsResult<Location> {
        let loc = dir.lookup_no_follow(name)?;
        self.try_resolve_symlink(root, dir, loc, follow_count)
    }

    /// Attempts to resolve a symlink
    fn try_resolve_symlink(
        &self,
        root: &Location,
        base: &Location,
        loc: Location,
        follow_count: &mut usize,
    ) -> VfsResult<Location> {
        if loc.node_type() != NodeType::Symlink {
            return Ok(loc);
        }

        if *follow_count >= self.max_symlinks {
            return Err(VfsError::FilesystemLoop);
        }

        *follow_count += 1;
        let target = loc.read_link()?;
        if target.is_empty() {
            return Err(VfsError::NotFound);
        }

        // Resolve the symlink target
        self.resolve_components(root, base, PathBuf::from(target).components(), follow_count)
    }
}

impl Default for PathResolver {
    fn default() -> Self {
        Self::new()
    }
}
