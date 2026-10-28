// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mechanical part of CG-047 (Kconfig feature ownership): a repository rule
//! check run by `xkmake hygiene check`, plus the `hygiene features --base`
//! difference mode.

mod checks;
mod diff;
mod kconfig;
mod model;
mod resolve;
#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use self::{
    checks::{FeatureContext, Lint},
    model::{Ownership, RULE_ID},
};
pub(crate) use self::{kconfig::KconfigModel, model::Workspace, resolve::Resolution};
use crate::{
    cli::HygieneFeaturesArgs,
    context::workspace_root,
    error::{Error, IoResultExt, Result},
    hygiene::{CheckRequest, Context, Finding, RuleCheck, print_findings, run_rule_checks},
};

/// The CG-047 rule check.
pub(crate) struct FeatureOwnership;

impl RuleCheck for FeatureOwnership {
    fn rule(&self) -> &'static str {
        RULE_ID
    }

    fn checks(&self) -> &'static [&'static str] {
        &Lint::CHECKS
    }

    fn report_checks(&self) -> &'static [&'static str] {
        &Lint::REPORT_CHECKS
    }

    fn run(&self, context: &Context) -> Result<Vec<Finding>> {
        let wants = |lint: Lint| context.wants(RULE_ID, lint.name());
        let workspace = context.workspace()?;
        let ownership = Ownership::from_package(workspace.kfeat()?)?;
        let app_package = app_package(workspace, &context.root().join(context.app()))?;
        let graph = FeatureContext::new(workspace, context.kconfig()?, &ownership, app_package);

        let mut findings = Vec::new();
        if wants(Lint::FeatureMap) {
            findings.extend(checks::check_feature_map(&graph));
        }
        if wants(Lint::KconfigImplication) {
            findings.extend(checks::check_kconfig_implication(&graph));
        }
        if wants(Lint::CapabilityBypass) {
            for resolution in context.resolutions()? {
                findings.extend(checks::check_capability_bypass(&graph, resolution));
            }
        }
        if wants(Lint::PeerForwarding) {
            findings.extend(checks::check_peer_forwarding(&graph));
        }
        if wants(Lint::BuildArgs) {
            findings.extend(checks::check_build_args(
                workspace,
                &read_build_files(context)?,
            ));
        }
        if wants(Lint::LinkedWithoutOwner) {
            for resolution in context.resolutions()? {
                findings.extend(checks::report_linked_without_owner(&graph, resolution));
            }
        }
        if wants(Lint::UnusedFeature) {
            findings.extend(checks::report_unused_features(
                workspace,
                &source_feature_names(context, workspace)?,
            ));
        }
        Ok(findings)
    }

    /// Explains each reported capability bypass with its `cargo tree -i`
    /// enable path; excepted findings never pay for the extra Cargo run.
    fn annotate(&self, context: &Context, findings: &mut [Finding]) -> Result<()> {
        for finding in findings {
            if finding.check != Lint::CapabilityBypass.name() {
                continue;
            }
            let resolutions = context.resolutions()?;
            if let Some(resolution) = resolutions
                .iter()
                .find(|resolution| finding.defconfig.as_ref() == Some(&resolution.defconfig))
                && let Some((package, feature)) = finding
                    .subject
                    .rsplit_once(" -> ")
                    .and_then(|(_, target)| target.split_once('/'))
            {
                finding.path = resolve::enable_path(
                    context.root(),
                    context.app(),
                    resolution,
                    package,
                    feature,
                )?;
            }
        }
        Ok(())
    }
}

/// `xkmake hygiene features`: the CG-047 rule check, or with `--base` the
/// feature difference against a revision.
pub(crate) fn run(args: &HygieneFeaturesArgs) -> Result<()> {
    let Some(base) = &args.base else {
        return run_rule_checks(&CheckRequest {
            rules: vec![RULE_ID.to_string()],
            checks: Vec::new(),
            defconfigs: args.defconfigs.clone(),
            format: args.format,
            report: args.report,
            app: args.app.clone(),
        });
    };
    let environment = |error: Error| Error::Environment(Box::new(error));
    let started = Instant::now();
    let root = workspace_root().map_err(environment)?;
    let mut findings = diff::run(&root, args, base).map_err(environment)?;
    findings.sort();
    print_findings(&findings, args.format, true)?;
    eprintln!(
        "feature ownership: {} diff line(s) in {:.1}s",
        findings.len(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Package name of the root workspace member whose crate directory is `app`.
fn app_package(workspace: &Workspace, app: &Path) -> Result<String> {
    let app_manifest = app.join("Cargo.toml");
    workspace
        .packages
        .values()
        .find(|package| same_file(&package.manifest_path, &app_manifest))
        .map(|package| package.name.clone())
        .ok_or_else(|| {
            Error::Message(format!(
                "{} is not a root workspace member",
                app_manifest.display()
            ))
        })
}

/// Cargo feature resolution of `defconfigs`, or of every platform defconfig.
pub(crate) fn resolve_defconfigs(
    root: &Path,
    app: &Path,
    defconfigs: &[PathBuf],
) -> Result<Vec<Resolution>> {
    if defconfigs.is_empty() {
        return resolve_all(root, app, &resolve::default_defconfigs(root)?);
    }
    resolve_all(root, app, defconfigs)
}

/// Static model of one tree for the `--base` difference mode.
struct Model {
    workspace: Workspace,
    kconfig: KconfigModel,
}

impl Model {
    fn load(root: &Path, app: &Path) -> Result<Self> {
        let workspace = Workspace::load(root)?;
        app_package(&workspace, &root.join(app))?;
        let kconfig = KconfigModel::load(root)?;
        Ok(Self { workspace, kconfig })
    }
}

fn same_file(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// Defconfigs named on the command line, or every platform defconfig.
fn selected_defconfigs(root: &Path, args: &HygieneFeaturesArgs) -> Result<Vec<PathBuf>> {
    if args.defconfigs.is_empty() {
        return resolve::default_defconfigs(root);
    }
    Ok(args.defconfigs.iter().map(|path| root.join(path)).collect())
}

fn resolve_all(root: &Path, app: &Path, defconfigs: &[PathBuf]) -> Result<Vec<Resolution>> {
    defconfigs
        .iter()
        .map(|defconfig| {
            resolve::resolve(root, app, defconfig)
                .map_err(|error| Error::Message(format!("{}: {error}", defconfig.display())))
        })
        .collect()
}

/// Build wiring scanned by the build-args check.
fn read_build_files(context: &Context) -> Result<Vec<(String, String)>> {
    let mut files = Vec::new();
    for file in context.files()? {
        let is_build_wiring = file == "Makefile"
            || file == "Jenkinsfile"
            || file.starts_with("scripts/")
            || file.starts_with(".githooks/")
            || file
                .strip_prefix(".cargo/")
                .is_some_and(|name| !name.contains('/') && name.ends_with(".toml"));
        if !is_build_wiring {
            continue;
        }
        // Binary fixtures are not build wiring.
        if let Ok(text) = fs::read_to_string(context.root().join(file)) {
            files.push((file.clone(), text));
        }
    }
    Ok(files)
}

/// Package name -> feature names its Rust sources test.
fn source_feature_names(
    context: &Context,
    workspace: &Workspace,
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut directories: Vec<(String, &str)> = workspace
        .packages
        .values()
        .map(|package| {
            (
                workspace.relative(package.directory()),
                package.name.as_str(),
            )
        })
        .collect();
    // Longest directory first so nested member crates claim their own files.
    directories.sort_by_key(|(directory, _)| std::cmp::Reverse(directory.len()));
    let mut names: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for file in context.files()?.iter().filter(|file| file.ends_with(".rs")) {
        let Some((_, package)) = directories.iter().find(|(directory, _)| {
            directory.is_empty() || file.starts_with(&format!("{directory}/"))
        }) else {
            continue;
        };
        let path = context.root().join(file);
        let text = fs::read_to_string(&path).with_path(&path)?;
        names
            .entry((*package).to_string())
            .or_default()
            .extend(checks::source_feature_names(&text));
    }
    Ok(names)
}
