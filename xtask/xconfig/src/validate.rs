// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Input validation helpers for paths that cross the xconfig trust boundary
//! (CLI arguments, environment variables, configuration file references).
//!
//! These checks harden the tool against malformed or traversal-bearing path
//! inputs (CWE-22). They intentionally allow absolute paths and `.` (current
//! directory) since those are legitimate for a host-side build tool, but reject
//! `..` (parent-directory) components so a configuration path cannot be made to
//! escape its intended location.

use std::{
    fs::{self, File},
    path::{Component, Path},
};

use tempfile::NamedTempFile;

use crate::error::{KconfigError, Result};

/// Validate a host-side input path supplied via the CLI, an environment
/// variable, or a configuration file reference.
///
/// Rejects empty paths, NUL bytes, non-UTF-8 paths, and any `..` (parent)
/// component. Absolute paths and `.` are allowed.
pub fn validate_input_path(path: &Path) -> Result<()> {
    let Some(text) = path.to_str() else {
        return Err(KconfigError::Config(format!(
            "path must be valid UTF-8: {}",
            path.display()
        )));
    };
    if text.is_empty() {
        return Err(KconfigError::Config("path must not be empty".to_string()));
    }
    if text.contains('\0') {
        return Err(KconfigError::Config(format!(
            "path must not contain NUL bytes: {}",
            path.display()
        )));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(KconfigError::Config(format!(
            "path must not contain parent-directory (..) components: {}",
            path.display()
        )));
    }
    Ok(())
}

/// Validates an existing regular input file without following a final symlink.
pub fn validate_input_file(path: &Path) -> Result<()> {
    validate_input_path(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(KconfigError::Config(format!(
            "input must be a regular file and not a symlink: {}",
            path.display()
        )));
    }
    Ok(())
}

/// Writes a generated file through a same-directory temporary file and then
/// atomically replaces the destination.
///
/// Replacing the directory entry prevents a pre-existing final-component
/// symlink from redirecting generated configuration into another file.
pub(crate) fn write_file_atomically(
    path: &Path,
    write_fn: impl FnOnce(&mut File) -> std::io::Result<()>,
) -> Result<()> {
    validate_input_path(path)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = NamedTempFile::new_in(parent)?;
    write_fn(temporary.as_file_mut())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|err| KconfigError::Io(err.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{io::Write, path::Path};

    use super::{validate_input_file, validate_input_path, write_file_atomically};

    #[test]
    fn accepts_clean_relative_and_absolute_paths() {
        assert!(validate_input_path(Path::new("Kconfig")).is_ok());
        assert!(validate_input_path(Path::new(".config")).is_ok());
        assert!(validate_input_path(Path::new(".")).is_ok());
        assert!(validate_input_path(Path::new("/etc/kbuild/Kconfig")).is_ok());
        assert!(validate_input_path(Path::new("sub/dir/config")).is_ok());
    }

    #[test]
    fn rejects_parent_directory_components() {
        assert!(validate_input_path(Path::new("../etc/passwd")).is_err());
        assert!(validate_input_path(Path::new("a/../../b")).is_err());
        assert!(validate_input_path(Path::new("foo/..")).is_err());
    }

    #[test]
    fn rejects_empty_paths() {
        assert!(validate_input_path(Path::new("")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_output_replaces_symlink_without_overwriting_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("target");
        let output = dir.path().join("output");
        std::fs::write(&target, b"keep").unwrap();
        symlink(&target, &output).unwrap();

        assert!(validate_input_file(&output).is_err());
        write_file_atomically(&output, |file| file.write_all(b"generated")).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        assert_eq!(std::fs::read(&output).unwrap(), b"generated");
        assert!(
            !std::fs::symlink_metadata(&output)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
