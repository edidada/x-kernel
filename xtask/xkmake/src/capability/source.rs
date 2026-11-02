// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Read-only access to the files cards cite.
//!
//! A source reference is either a repository path (read from the working
//! tree) or `<crate>-<version>/<path>` for a crates.io package locked in
//! `Cargo.lock` (read from the local Cargo registry source cache).

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    rc::Rc,
};

use super::{card::is_relative_path, items::ItemIndex};

/// Contents of one cited file.
#[derive(Debug)]
pub(crate) struct SourceFile {
    pub(crate) lines: Vec<String>,
    text: String,
    items: std::cell::OnceCell<Result<Rc<ItemIndex>, String>>,
}

impl SourceFile {
    pub(crate) fn new(bytes: &[u8]) -> Self {
        let text = String::from_utf8_lossy(bytes).into_owned();
        Self {
            lines: text.lines().map(str::to_owned).collect(),
            text,
            items: std::cell::OnceCell::new(),
        }
    }

    /// Item index of a Rust file, built on first use.
    pub(crate) fn items(&self) -> Result<Rc<ItemIndex>, String> {
        self.items
            .get_or_init(|| ItemIndex::parse(&self.text).map(Rc::new))
            .clone()
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// Packages of `Cargo.lock`.
#[derive(Clone, Debug, Default)]
pub(crate) struct LockedPackages {
    /// crates.io (or other registry) packages: name -> versions.
    pub(crate) registry: BTreeMap<String, BTreeSet<String>>,
    /// Path packages of the workspace.
    pub(crate) workspace: BTreeSet<String>,
}

impl LockedPackages {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        #[derive(serde::Deserialize)]
        struct Lock {
            #[serde(default)]
            package: Vec<Package>,
        }
        #[derive(serde::Deserialize)]
        struct Package {
            name: String,
            version: String,
            source: Option<String>,
        }
        let lock: Lock = toml::from_str(text).map_err(|error| error.to_string())?;
        let mut out = Self::default();
        for package in lock.package {
            if package.source.is_some() {
                out.registry
                    .entry(package.name)
                    .or_default()
                    .insert(package.version);
            } else {
                out.workspace.insert(package.name);
            }
        }
        Ok(out)
    }

    pub(crate) fn is_locked(&self, name: &str, version: &str) -> bool {
        self.registry
            .get(name)
            .is_some_and(|versions| versions.contains(version))
    }
}

/// A parsed source reference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Reference<'a> {
    Repository(&'a str),
    Dependency {
        name: &'a str,
        version: &'a str,
        path: &'a str,
    },
}

/// Splits `<name>-<x.y.z...>/<path>`; the name is the shortest prefix
/// followed by `-<digits>.<digits>.<digits>`, as crate names may contain `-`.
pub(crate) fn split_dependency_reference(reference: &str) -> Option<(&str, &str, &str)> {
    let (first, path) = reference.split_once('/')?;
    first.match_indices('-').find_map(|(index, _)| {
        let version = &first[index + 1..];
        let mut parts = version.splitn(3, '.');
        let is_semver = parts.next().is_some_and(is_digits)
            && parts.next().is_some_and(is_digits)
            && parts
                .next()
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()));
        (index > 0 && is_semver).then(|| (&first[..index], version, path))
    })
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// The repository working tree plus locked dependency sources.
pub(crate) struct SourceTree {
    root: PathBuf,
    registry_roots: Vec<PathBuf>,
    pub(crate) locked: LockedPackages,
    files: HashMap<String, Option<Rc<SourceFile>>>,
}

impl SourceTree {
    pub(crate) fn new(root: &Path, registry_roots: Vec<PathBuf>) -> Result<Self, String> {
        let lock_path = root.join("Cargo.lock");
        let locked = match fs::read_to_string(&lock_path) {
            Ok(text) => LockedPackages::parse(&text)
                .map_err(|error| format!("{}: {error}", lock_path.display()))?,
            Err(_) => LockedPackages::default(),
        };
        Ok(Self {
            root: root.to_owned(),
            registry_roots,
            locked,
            files: HashMap::new(),
        })
    }

    /// `$CARGO_HOME/registry/src/*`, where Cargo unpacks crates.io sources.
    pub(crate) fn default_registry_roots() -> Vec<PathBuf> {
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")));
        let Some(cargo_home) = cargo_home else {
            return Vec::new();
        };
        let mut roots: Vec<PathBuf> = fs::read_dir(cargo_home.join("registry/src"))
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                    .filter(|path| path.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        roots.sort();
        roots
    }

    pub(crate) fn classify<'a>(&self, reference: &'a str) -> Reference<'a> {
        match split_dependency_reference(reference) {
            Some((name, version, path)) if self.locked.is_locked(name, version) => {
                Reference::Dependency {
                    name,
                    version,
                    path,
                }
            }
            _ => Reference::Repository(reference),
        }
    }

    /// Unpacked source directory of a locked package, if Cargo fetched it.
    pub(crate) fn dependency_dir(&self, name: &str, version: &str) -> Option<PathBuf> {
        self.registry_roots
            .iter()
            .map(|root| root.join(format!("{name}-{version}")))
            .find(|dir| dir.is_dir())
    }

    /// Filesystem path of a reference, or `None` if it cannot exist.
    pub(crate) fn path_of(&self, reference: &str) -> Option<PathBuf> {
        match self.classify(reference) {
            Reference::Repository(path) => {
                is_relative_path(path.trim_end_matches('/')).then(|| self.root.join(path))
            }
            Reference::Dependency {
                name,
                version,
                path,
            } => is_relative_path(path)
                .then(|| self.dependency_dir(name, version).map(|dir| dir.join(path)))
                .flatten(),
        }
    }

    pub(crate) fn file(&mut self, reference: &str) -> Option<Rc<SourceFile>> {
        if let Some(cached) = self.files.get(reference) {
            return cached.clone();
        }
        let file = self
            .path_of(reference)
            .and_then(|path| fs::read(path).ok())
            .map(|bytes| Rc::new(SourceFile::new(&bytes)));
        self.files.insert(reference.to_owned(), file.clone());
        file
    }

    /// References of the `.rs` files below a directory reference ending in
    /// `/`, skipping `target` directories, sorted.
    pub(crate) fn rust_files_under(&self, directory_reference: &str) -> Vec<String> {
        let Some(directory) = self.path_of(directory_reference) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut pending = vec![directory.clone()];
        while let Some(current) = pending.pop() {
            let Ok(entries) = fs::read_dir(&current) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_dir() {
                    if entry.file_name() != "target" && entry.file_name() != ".git" {
                        pending.push(path);
                    }
                } else if file_type.is_file()
                    && path.extension().is_some_and(|ext| ext == "rs")
                    && let Ok(relative) = path.strip_prefix(&directory)
                {
                    out.push(format!(
                        "{directory_reference}{}",
                        relative.to_string_lossy()
                    ));
                }
            }
        }
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{LockedPackages, split_dependency_reference};

    #[test]
    fn dependency_references_split_at_the_version() {
        assert_eq!(
            split_dependency_reference("intrusive-collections-0.10.2/src/lib.rs"),
            Some(("intrusive-collections", "0.10.2", "src/lib.rs"))
        );
        assert_eq!(
            split_dependency_reference("safe-mmio-0.3.2-rc.1/src/a.rs"),
            Some(("safe-mmio", "0.3.2-rc.1", "src/a.rs"))
        );
        assert_eq!(split_dependency_reference("util/kxarray/src/lib.rs"), None);
        assert_eq!(split_dependency_reference("ktime-types/src/lib.rs"), None);
    }

    #[test]
    fn lockfile_separates_registry_and_workspace_packages() {
        let lock = LockedPackages::parse(
            r#"
version = 4
[[package]]
name = "ringbuf"
version = "0.4.8"
source = "registry+https://github.com/rust-lang/crates.io-index"
[[package]]
name = "kxarray"
version = "0.1.0"
"#,
        )
        .unwrap();
        assert!(lock.is_locked("ringbuf", "0.4.8"));
        assert!(!lock.is_locked("ringbuf", "0.4.7"));
        assert!(lock.workspace.contains("kxarray"));
    }
}
