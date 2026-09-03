// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{borrow::ToOwned, string::String};
use core::ffi::c_int;

use kerrno::{KError, KResult};
use kfs::FsContext;
use kvfs::{
    Location, Metadata,
    path::{Component, Path},
};
use linux_raw_sys::general::{AT_FDCWD, AT_SYMLINK_NOFOLLOW};

const TEE_FS_ROOT: &str = "/tee/";
const TEE_TMP_ROOT: &str = "/tmp/";

/// True when `normalized` is exactly `root` or a child path under `root/`.
fn is_under_root(normalized: &str, root: &str) -> bool {
    debug_assert!(root.ends_with('/'));
    let root_dir = root.trim_end_matches('/');
    normalized == root_dir || normalized.starts_with(root)
}

fn is_under_allowed_root(normalized: &str) -> bool {
    is_under_root(normalized, TEE_FS_ROOT) || is_under_root(normalized, TEE_TMP_ROOT)
}

/// True when `normalized` is exactly `/tee` or `/tmp` (the jail roots).
pub(crate) fn is_allowed_root_dir(normalized: &str) -> bool {
    normalized == TEE_FS_ROOT.trim_end_matches('/')
        || normalized == TEE_TMP_ROOT.trim_end_matches('/')
}

/// Normalize and validate a path before TEE filesystem access.
///
/// Rejects `..` traversal and confines absolute paths to `/tee/` or `/tmp/`.
/// This is lexical only. Callers must still
/// [`require_allowed_location`] after VFS resolution: intermediate components
/// are followed, so a symlink under `/tmp/` (world-writable) can otherwise
/// escape the jail.
pub(crate) fn validate_tee_path(path: &str) -> KResult<String> {
    if path.is_empty() || path.as_bytes().contains(&0) {
        return Err(KError::InvalidInput);
    }

    for component in Path::new(path).components() {
        if matches!(component, Component::ParentDir) {
            return Err(KError::InvalidInput);
        }
    }

    let normalized = Path::new(path).normalize().ok_or(KError::InvalidInput)?;
    let normalized = normalized.as_str();

    if !is_under_allowed_root(normalized) {
        return Err(KError::InvalidInput);
    }

    Ok(normalized.to_owned())
}

/// Reject a resolved location whose absolute path left `/tee/` or `/tmp/`.
///
/// VFS `resolve` / `resolve_no_follow` follow intermediate directory
/// symlinks. Lexical [`validate_tee_path`] cannot see that, so every TEE
/// lookup must re-check the location that was actually reached.
pub(crate) fn require_allowed_location(loc: &Location) -> KResult<()> {
    let abs = loc.absolute_path()?;
    if !is_under_allowed_root(abs.as_str()) {
        return Err(KError::PermissionDenied);
    }
    Ok(())
}

/// Ensure `path`'s parent is still inside the TEE jail after symlink following.
///
/// Must run before create/open so a directory symlink to `/etc` cannot be
/// used to create or truncate a file outside the jail. The jail roots
/// themselves have parent `/` and are allowed without treating `/` as in-jail.
pub(crate) fn require_confined_parent(fs: &FsContext, path: &str) -> KResult<()> {
    if is_allowed_root_dir(path) {
        return Ok(());
    }
    let (parent, _) = fs.resolve_parent(Path::new(path))?;
    require_allowed_location(&parent)
}

pub fn with_fs<R>(dirfd: c_int, f: impl FnOnce(&mut FsContext) -> KResult<R>) -> KResult<R> {
    if dirfd != AT_FDCWD {
        return Err(KError::InvalidInput);
    }

    // TEE file helpers are callable from both process and kernel-task paths,
    // so they must use the shared current-path filesystem view.
    let fs_context = kthread::current_fs_context();
    let mut fs = fs_context.lock();
    f(&mut fs)
}

pub enum ResolveAtResult {
    File(Location),
}

impl ResolveAtResult {
    pub fn stat(&self) -> KResult<Metadata> {
        match self {
            Self::File(file) => file.metadata(),
        }
    }
}

/// Resolve `path` relative to `dirfd` (must be `AT_FDCWD`) and keep the
/// result inside `/tee/` or `/tmp/`.
///
/// Lexical validation happens first. After VFS lookup, the location's
/// absolute path is checked again so a symlink under the jail cannot
/// redirect into `/etc` or other host paths.
pub fn resolve_at(dirfd: c_int, path: Option<&str>, flags: u32) -> KResult<ResolveAtResult> {
    let path = path
        .filter(|path| !path.is_empty())
        .ok_or(KError::NotFound)?;
    let path = validate_tee_path(path)?;

    with_fs(dirfd, |fs| {
        let loc = if flags & AT_SYMLINK_NOFOLLOW != 0 {
            fs.resolve_no_follow(&path)?
        } else {
            fs.resolve(&path)?
        };
        require_allowed_location(&loc)?;
        Ok(ResolveAtResult::File(loc))
    })
}

#[cfg(unittest)]
mod tests {
    #[test]
    fn validate_tee_path_allows_tee_root() {
        assert_eq!(
            validate_tee_path("/tee/object.bin").unwrap(),
            "/tee/object.bin"
        );
    }

    #[test]
    fn validate_tee_path_allows_tee_directory_root() {
        // Trailing slash is stripped by normalize(); the root itself must still match.
        assert_eq!(validate_tee_path("/tee/").unwrap(), "/tee");
    }

    #[test]
    fn validate_tee_path_allows_tmp_root() {
        assert_eq!(validate_tee_path("/tmp/test.txt").unwrap(), "/tmp/test.txt");
    }

    #[test]
    fn validate_tee_path_allows_tmp_directory_root() {
        assert_eq!(validate_tee_path("/tmp/").unwrap(), "/tmp");
    }

    #[test]
    fn validate_tee_path_rejects_parent_dir() {
        assert!(validate_tee_path("/tee/../etc/passwd").is_err());
        assert!(validate_tee_path("../etc/passwd").is_err());
    }

    #[test]
    fn validate_tee_path_rejects_outside_roots() {
        assert!(validate_tee_path("/etc/passwd").is_err());
        assert!(validate_tee_path("/var/log/messages").is_err());
    }

    #[test]
    fn validate_tee_path_rejects_relative_paths() {
        assert!(validate_tee_path("test.txt").is_err());
    }
}
