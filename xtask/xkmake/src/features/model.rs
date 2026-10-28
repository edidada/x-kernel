// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Structured Cargo feature graph of the root workspace and kfeat ownership.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde::Deserialize;

use crate::error::{Error, IoResultExt, Result};

/// The crate that assembles system capabilities from Kconfig selections.
pub(crate) const KFEAT_PACKAGE: &str = "kfeat";
/// Rule id of the CG-047 check.
pub(crate) const RULE_ID: &str = "CG-047-kconfig-feature-ownership";

/// One entry of a `[features]` list.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum FeatureValue {
    /// Another feature of the same package.
    Local(String),
    /// `dep:<key>` optional dependency activation.
    Dep(String),
    /// `<key>/<feature>` or the weak `<key>?/<feature>`.
    Forward {
        dep: String,
        is_weak: bool,
        feature: String,
    },
}

impl FeatureValue {
    pub(crate) fn parse(value: &str) -> Self {
        if let Some(dep) = value.strip_prefix("dep:") {
            return Self::Dep(dep.to_string());
        }
        match value.split_once('/') {
            Some((dep, feature)) => {
                let (dep, is_weak) = dep
                    .strip_suffix('?')
                    .map_or((dep, false), |dep| (dep, true));
                Self::Forward {
                    dep: dep.to_string(),
                    is_weak,
                    feature: feature.to_string(),
                }
            }
            None => Self::Local(value.to_string()),
        }
    }
}

/// Dependency kinds that matter for feature resolution of the kernel.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum DepKind {
    Normal,
    Build,
    Dev,
}

impl DepKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Build => "build",
            Self::Dev => "dev",
        }
    }
}

/// A dependency declaration after Cargo merged `workspace = true` inheritance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Dependency {
    /// Key used in feature strings (`rename` or package name).
    pub(crate) key: String,
    /// Package name of the dependency.
    pub(crate) package: String,
    pub(crate) kind: DepKind,
    pub(crate) target: Option<String>,
    pub(crate) is_optional: bool,
    pub(crate) uses_default_features: bool,
    pub(crate) features: Vec<String>,
}

/// One root workspace member.
#[derive(Clone, Debug)]
pub(crate) struct Package {
    pub(crate) name: String,
    pub(crate) manifest_path: PathBuf,
    /// Manifest text, kept for line lookups only.
    pub(crate) manifest_text: String,
    /// Features declared in `[features]`; excludes Cargo's implicit
    /// optional-dependency features.
    pub(crate) features: BTreeMap<String, Vec<FeatureValue>>,
    pub(crate) dependencies: Vec<Dependency>,
    /// Features named by `required-features` of any target.
    pub(crate) required_features: BTreeSet<String>,
    /// `[package.metadata]` as JSON.
    pub(crate) metadata: serde_json::Value,
}

impl Package {
    pub(crate) fn directory(&self) -> &Path {
        self.manifest_path.parent().unwrap_or(Path::new("."))
    }

    /// Resolves a dependency key used in a feature string.
    pub(crate) fn dependency(&self, key: &str) -> Option<&Dependency> {
        self.dependencies
            .iter()
            .find(|dependency| dependency.key == key && dependency.kind != DepKind::Dev)
            .or_else(|| {
                self.dependencies
                    .iter()
                    .find(|dependency| dependency.key == key)
            })
    }

    /// Package name behind a dependency key, falling back to the key itself.
    pub(crate) fn package_of(&self, key: &str) -> String {
        self.dependency(key)
            .map_or_else(|| key.to_string(), |dependency| dependency.package.clone())
    }

    /// Local features reachable from `feature`, including itself.
    pub(crate) fn local_closure(&self, feature: &str) -> BTreeSet<String> {
        let mut closure = BTreeSet::new();
        let mut pending = vec![feature.to_string()];
        while let Some(name) = pending.pop() {
            if !closure.insert(name.clone()) {
                continue;
            }
            for value in self.features.get(&name).into_iter().flatten() {
                if let FeatureValue::Local(next) = value {
                    pending.push(next.clone());
                }
            }
        }
        closure
    }
}

/// Root workspace members keyed by package name.
#[derive(Clone, Debug)]
pub(crate) struct Workspace {
    pub(crate) root: PathBuf,
    pub(crate) packages: BTreeMap<String, Package>,
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<MetadataPackage>,
    workspace_members: Vec<String>,
}

#[derive(Deserialize)]
struct MetadataPackage {
    id: String,
    name: String,
    manifest_path: PathBuf,
    #[serde(default)]
    dependencies: Vec<MetadataDependency>,
    #[serde(default)]
    targets: Vec<MetadataTarget>,
    #[serde(default)]
    metadata: serde_json::Value,
}

#[derive(Deserialize)]
struct MetadataDependency {
    name: String,
    rename: Option<String>,
    kind: Option<String>,
    target: Option<String>,
    optional: bool,
    uses_default_features: bool,
    features: Vec<String>,
}

#[derive(Deserialize)]
struct MetadataTarget {
    #[serde(default, rename = "required-features")]
    required_features: Vec<String>,
}

impl Workspace {
    /// Loads the root workspace through `cargo metadata --no-deps`.
    pub(crate) fn load(root: &Path) -> Result<Self> {
        let output = Command::new("cargo")
            .current_dir(root)
            .args(["metadata", "--no-deps", "--format-version", "1"])
            .output()
            .map_err(|source| Error::Spawn {
                program: "cargo".into(),
                source,
            })?;
        if !output.status.success() {
            return Err(Error::Message(format!(
                "cargo metadata failed in {}: {}",
                root.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let metadata: Metadata = serde_json::from_slice(&output.stdout)
            .map_err(|error| Error::Message(format!("failed to parse cargo metadata: {error}")))?;
        let members: BTreeSet<_> = metadata.workspace_members.into_iter().collect();
        let mut packages = BTreeMap::new();
        for package in metadata.packages {
            if !members.contains(&package.id) {
                continue;
            }
            let package = Package::from_metadata(package)?;
            packages.insert(package.name.clone(), package);
        }
        Ok(Self {
            root: root.to_path_buf(),
            packages,
        })
    }

    pub(crate) fn kfeat(&self) -> Result<&Package> {
        self.packages
            .get(KFEAT_PACKAGE)
            .ok_or_else(|| Error::Message(format!("workspace has no `{KFEAT_PACKAGE}` package")))
    }

    /// Path relative to the workspace root, for diagnostics.
    pub(crate) fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }
}

impl Package {
    fn from_metadata(package: MetadataPackage) -> Result<Self> {
        let manifest_text =
            fs::read_to_string(&package.manifest_path).with_path(&package.manifest_path)?;
        let manifest: toml::Table = toml::from_str(&manifest_text).map_err(|error| {
            Error::Message(format!(
                "failed to parse {}: {error}",
                package.manifest_path.display()
            ))
        })?;
        // Cargo metadata also lists implicit `name = ["dep:name"]` features;
        // only the manifest's own table says which ones were declared.
        let features = manifest
            .get("features")
            .and_then(toml::Value::as_table)
            .map(|table| {
                table
                    .iter()
                    .map(|(name, values)| {
                        let values = values
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(toml::Value::as_str)
                            .map(FeatureValue::parse)
                            .collect();
                        (name.clone(), values)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let dependencies = package
            .dependencies
            .into_iter()
            .map(|dependency| Dependency {
                key: dependency.rename.unwrap_or_else(|| dependency.name.clone()),
                package: dependency.name,
                kind: match dependency.kind.as_deref() {
                    Some("dev") => DepKind::Dev,
                    Some("build") => DepKind::Build,
                    _ => DepKind::Normal,
                },
                target: dependency.target,
                is_optional: dependency.optional,
                uses_default_features: dependency.uses_default_features,
                features: dependency.features,
            })
            .collect();
        let required_features = package
            .targets
            .into_iter()
            .flat_map(|target| target.required_features)
            .collect();
        Ok(Self {
            name: package.name,
            manifest_path: package.manifest_path,
            manifest_text,
            features,
            dependencies,
            required_features,
            metadata: package.metadata,
        })
    }

    /// 1-based line of `feature = [...]` in `[features]`, or of `needle`
    /// inside that list when given.
    pub(crate) fn feature_line(&self, feature: &str, needle: Option<&str>) -> Option<usize> {
        let mut in_features = false;
        let mut start = None;
        for (index, line) in self.manifest_text.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                if start.is_some() {
                    break;
                }
                in_features = trimmed == "[features]";
                continue;
            }
            if !in_features {
                continue;
            }
            if start.is_none() && toml_key(trimmed) == Some(feature) {
                start = Some(index + 1);
            } else if start.is_some() && toml_key(trimmed).is_some() {
                break;
            }
            if let (Some(_), Some(needle)) = (start, needle)
                && line.contains(&format!("\"{needle}\""))
            {
                return Some(index + 1);
            }
        }
        start
    }

    /// 1-based line declaring the dependency `key` outside dev-dependencies.
    pub(crate) fn dependency_line(&self, key: &str) -> Option<usize> {
        let mut section = String::new();
        for (index, line) in self.manifest_text.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                section = trimmed.trim_matches(['[', ']']).to_string();
                if !section.contains("dev-dependencies")
                    && section.rsplit_once('.').is_some_and(|(table, name)| {
                        table.ends_with("dependencies") && name.trim_matches('"') == key
                    })
                {
                    return Some(index + 1);
                }
                continue;
            }
            if section.ends_with("dependencies")
                && !section.contains("dev-dependencies")
                && toml_key(trimmed) == Some(key)
            {
                return Some(index + 1);
            }
        }
        None
    }
}

/// Returns the bare or quoted key of a `key = value` line.
fn toml_key(line: &str) -> Option<&str> {
    let (key, _) = line.split_once('=')?;
    let key = key.trim().trim_matches('"');
    (!key.is_empty() && !key.starts_with('#')).then_some(key)
}

/// A `(package, feature)` pair.
pub(crate) type FeatureRef = (String, String);

/// `[package.metadata.kfeat]`: configuration, not exceptions. Accepted
/// findings live in `docs/code-guidelines/exceptions.toml`; unknown keys (such
/// as a former `exceptions` table) are rejected.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct KfeatMetadata {
    #[serde(default)]
    internal: Vec<String>,
}

/// Ownership derived from `api/kfeat/Cargo.toml`.
#[derive(Clone, Debug, Default)]
pub(crate) struct Ownership {
    /// Declared kfeat features (implicit optional-dependency features excluded).
    pub(crate) features: BTreeMap<String, Vec<FeatureValue>>,
    /// Capability features: every `crate/feat` or `crate?/feat` target.
    pub(crate) capabilities: BTreeMap<FeatureRef, BTreeSet<String>>,
    /// kfeat features that activate each optional dependency package.
    pub(crate) dependency_owners: BTreeMap<String, BTreeSet<String>>,
    pub(crate) internal: BTreeSet<String>,
}

impl Ownership {
    pub(crate) fn from_package(kfeat: &Package) -> Result<Self> {
        let metadata: KfeatMetadata = kfeat
            .metadata
            .get("kfeat")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| {
                Error::Message(format!(
                    "invalid [package.metadata.kfeat] in {}: {error}",
                    kfeat.manifest_path.display()
                ))
            })?
            .unwrap_or_default();
        let mut ownership = Self {
            features: kfeat.features.clone(),
            internal: metadata.internal.into_iter().collect(),
            ..Self::default()
        };
        for (feature, values) in &kfeat.features {
            for value in values {
                match value {
                    FeatureValue::Local(_) => {}
                    FeatureValue::Dep(key) => {
                        ownership.add_dependency_owner(kfeat, key, feature);
                    }
                    FeatureValue::Forward {
                        dep,
                        is_weak,
                        feature: target,
                    } => {
                        ownership
                            .capabilities
                            .entry((kfeat.package_of(dep), target.clone()))
                            .or_default()
                            .insert(feature.clone());
                        // A strong `crate/feat` also activates an optional crate.
                        if !is_weak {
                            ownership.add_dependency_owner(kfeat, dep, feature);
                        }
                    }
                }
            }
        }
        Ok(ownership)
    }

    fn add_dependency_owner(&mut self, kfeat: &Package, key: &str, feature: &str) {
        if kfeat.dependency(key).is_some_and(|dep| dep.is_optional) {
            self.dependency_owners
                .entry(kfeat.package_of(key))
                .or_default()
                .insert(feature.to_string());
        }
    }

    pub(crate) fn owners(&self, package: &str, feature: &str) -> Option<&BTreeSet<String>> {
        self.capabilities
            .get(&(package.to_string(), feature.to_string()))
    }

    /// kfeat features reachable from `feature` through kfeat-internal edges.
    pub(crate) fn internal_closure(&self, feature: &str) -> BTreeSet<String> {
        let mut closure = BTreeSet::new();
        let mut pending = vec![feature.to_string()];
        while let Some(name) = pending.pop() {
            if !closure.insert(name.clone()) {
                continue;
            }
            for value in self.features.get(&name).into_iter().flatten() {
                if let FeatureValue::Local(next) = value {
                    pending.push(next.clone());
                }
            }
        }
        closure
    }
}
