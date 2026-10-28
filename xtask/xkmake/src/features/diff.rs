// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! F6: difference of resolved features and feature structure against a base.
//!
//! Deletions carry no added line for a reviewer or keyword trigger to see, so
//! this mode reports removed participants, edges and Kconfig relations too.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use super::{
    Model,
    checks::{Lint, finding},
    model::Workspace,
    resolve_all, selected_defconfigs,
};
use crate::{
    cli::HygieneFeaturesArgs,
    error::{Error, IoResultExt, Result},
    hygiene::Finding,
};

/// `(file, line)` entries of one side, compared as sets.
#[derive(Default)]
struct Snapshot {
    /// defconfig -> enabled `crate` / `crate/feature` of workspace members.
    enabled: BTreeMap<String, BTreeSet<String>>,
    /// Manifest feature tables and dependency declarations.
    graph: BTreeSet<(String, String)>,
    /// `KFEAT_*` declarations and their requirements/selects.
    kconfig: BTreeSet<(String, String)>,
}

/// Removes the exported base tree when dropped.
struct ExportedTree(PathBuf);

impl Drop for ExportedTree {
    fn drop(&mut self) {
        // This directory was created by `export` from `git archive`; it is
        // not a Git checkout, so removing it cannot discard user work.
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(super) fn run(root: &Path, args: &HygieneFeaturesArgs, base: &str) -> Result<Vec<Finding>> {
    let tree = export(root, base)?;
    let base_snapshot = snapshot(&tree.0, args)?;
    let head_snapshot = snapshot(root, args)?;

    let mut findings = Vec::new();
    let defconfigs: BTreeSet<&String> = base_snapshot
        .enabled
        .keys()
        .chain(head_snapshot.enabled.keys())
        .collect();
    let empty = BTreeSet::new();
    for defconfig in defconfigs {
        let before = base_snapshot.enabled.get(defconfig).unwrap_or(&empty);
        let after = head_snapshot.enabled.get(defconfig).unwrap_or(&empty);
        for (sign, entry) in changes(before, after) {
            findings.push(
                finding(
                    Lint::Diff,
                    defconfig.clone(),
                    None,
                    format!("{sign} {entry}"),
                    "resolved feature set",
                )
                .with_defconfig(defconfig.clone()),
            );
        }
    }
    for (category, before, after) in [
        (
            "manifest feature graph",
            &base_snapshot.graph,
            &head_snapshot.graph,
        ),
        (
            "Kconfig KFEAT relation",
            &base_snapshot.kconfig,
            &head_snapshot.kconfig,
        ),
    ] {
        for (sign, (file, entry)) in changes(before, after) {
            findings.push(finding(
                Lint::Diff,
                file.clone(),
                None,
                format!("{sign} {entry}"),
                category,
            ));
        }
    }
    Ok(findings)
}

fn changes<'a, T: Ord>(before: &'a BTreeSet<T>, after: &'a BTreeSet<T>) -> Vec<(char, &'a T)> {
    before
        .difference(after)
        .map(|entry| ('-', entry))
        .chain(after.difference(before).map(|entry| ('+', entry)))
        .collect()
}

/// Exports `revision` with `git archive` under the xkmake target directory.
fn export(root: &Path, revision: &str) -> Result<ExportedTree> {
    let directory = root
        .join("target")
        .join("xkmake")
        .join(format!("features-base-{}", std::process::id()));
    if directory.exists() {
        fs::remove_dir_all(&directory).with_path(&directory)?;
    }
    fs::create_dir_all(&directory).with_path(&directory)?;
    let tree = ExportedTree(directory);
    let mut archive = Command::new("git")
        .current_dir(root)
        .args(["archive", "--format=tar", revision])
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|source| Error::Spawn {
            program: "git".into(),
            source,
        })?;
    let archive_stdout = archive
        .stdout
        .take()
        .ok_or_else(|| Error::Message("failed to capture git archive output".into()))?;
    let unpack = Command::new("tar")
        .arg("-x")
        .arg("-C")
        .arg(&tree.0)
        .stdin(Stdio::from(archive_stdout))
        .status()
        .map_err(|source| Error::Spawn {
            program: "tar".into(),
            source,
        })?;
    let archived = archive.wait().map_err(|source| Error::Spawn {
        program: "git".into(),
        source,
    })?;
    if !archived.success() {
        return Err(Error::CommandFailed {
            program: format!("git archive {revision}"),
            status: archived,
        });
    }
    if !unpack.success() {
        return Err(Error::CommandFailed {
            program: "tar".into(),
            status: unpack,
        });
    }
    Ok(tree)
}

fn snapshot(root: &Path, args: &HygieneFeaturesArgs) -> Result<Snapshot> {
    let model = Model::load(root, &args.app)?;
    let defconfigs = selected_defconfigs(root, args)?
        .into_iter()
        .filter(|defconfig| defconfig.is_file())
        .collect::<Vec<_>>();
    let mut snapshot = Snapshot::default();
    for resolution in resolve_all(root, &args.app, &defconfigs)? {
        let mut enabled = BTreeSet::new();
        for (package, features) in &resolution.packages {
            if !model.workspace.packages.contains_key(package) {
                continue;
            }
            enabled.insert(package.clone());
            enabled.extend(
                features
                    .iter()
                    .map(|feature| format!("{package}/{feature}")),
            );
        }
        snapshot.enabled.insert(resolution.defconfig, enabled);
    }
    snapshot.graph = manifest_graph(&model.workspace);
    for (symbol, facts) in &model.kconfig.symbols {
        if !symbol.starts_with("KFEAT_") {
            continue;
        }
        let file = model
            .kconfig
            .locations
            .get(symbol)
            .map_or_else(|| "Kconfig".to_string(), |location| location.file.clone());
        let kind = if facts.is_bool { "bool" } else { "value" };
        snapshot
            .kconfig
            .insert((file.clone(), format!("{symbol} ({kind})")));
        for required in &facts.requires {
            snapshot
                .kconfig
                .insert((file.clone(), format!("{symbol} depends on {required}")));
        }
        for selected in &facts.selects {
            snapshot
                .kconfig
                .insert((file.clone(), format!("{symbol} selects {selected}")));
        }
    }
    Ok(snapshot)
}

fn manifest_graph(workspace: &Workspace) -> BTreeSet<(String, String)> {
    let mut graph = BTreeSet::new();
    for (name, package) in &workspace.packages {
        let file = workspace.relative(&package.manifest_path);
        for (feature, values) in &package.features {
            graph.insert((file.clone(), format!("feature {name}/{feature}")));
            for value in values {
                let value = super::checks::value_text(value);
                graph.insert((file.clone(), format!("edge {name}/{feature} -> {value}")));
            }
        }
        for dependency in &package.dependencies {
            let mut traits = vec![dependency.kind.as_str().to_string()];
            if let Some(target) = &dependency.target {
                traits.push(format!("target={target}"));
            }
            if dependency.is_optional {
                traits.push("optional".to_string());
            }
            if !dependency.uses_default_features {
                traits.push("default-features=false".to_string());
            }
            if !dependency.features.is_empty() {
                traits.push(format!("features=[{}]", dependency.features.join(",")));
            }
            let renamed = if dependency.key == dependency.package {
                String::new()
            } else {
                format!(" as {}", dependency.key)
            };
            graph.insert((
                file.clone(),
                format!(
                    "dependency {name} -> {}{renamed} [{}]",
                    dependency.package,
                    traits.join(", ")
                ),
            ));
        }
    }
    graph
}
