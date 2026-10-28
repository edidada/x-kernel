// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CG-047 checks (F1-F5) and report items (R1, R2).

use std::collections::{BTreeMap, BTreeSet};

use super::{
    kconfig::KconfigModel,
    model::{
        DepKind, Dependency, FeatureValue, KFEAT_PACKAGE, Ownership, Package, RULE_ID, Workspace,
    },
    resolve::Resolution,
};
use crate::hygiene::{Finding, Severity};

/// Check names; the kebab-case form matches `checks.lint` in the rule file.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Lint {
    FeatureMap,
    KconfigImplication,
    CapabilityBypass,
    PeerForwarding,
    BuildArgs,
    LinkedWithoutOwner,
    UnusedFeature,
    Diff,
}

impl Lint {
    /// Checks listed under `backend: xkmake` in the rule file.
    pub(crate) const CHECKS: [&'static str; 5] = [
        Self::FeatureMap.name(),
        Self::KconfigImplication.name(),
        Self::CapabilityBypass.name(),
        Self::PeerForwarding.name(),
        Self::BuildArgs.name(),
    ];
    /// Report-only checks (`--report`).
    pub(crate) const REPORT_CHECKS: [&'static str; 2] =
        [Self::LinkedWithoutOwner.name(), Self::UnusedFeature.name()];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::FeatureMap => "feature-map",
            Self::KconfigImplication => "kconfig-implication",
            Self::CapabilityBypass => "capability-bypass",
            Self::PeerForwarding => "peer-forwarding",
            Self::BuildArgs => "build-args",
            Self::LinkedWithoutOwner => "linked-without-owner",
            Self::UnusedFeature => "unused-feature",
            Self::Diff => "diff",
        }
    }

    pub(crate) const fn severity(self) -> Severity {
        match self {
            Self::LinkedWithoutOwner | Self::UnusedFeature => Severity::Report,
            Self::Diff => Severity::Diff,
            _ => Severity::Error,
        }
    }
}

/// A CG-047 finding of `lint`.
pub(crate) fn finding(
    lint: Lint,
    file: impl Into<String>,
    line: Option<usize>,
    subject: impl Into<String>,
    message: impl Into<String>,
) -> Finding {
    Finding::new(RULE_ID, lint.name(), file, line, subject, message).with_severity(lint.severity())
}

/// Static inputs shared by every check.
pub(crate) struct FeatureContext<'a> {
    pub(crate) workspace: &'a Workspace,
    pub(crate) kconfig: &'a KconfigModel,
    pub(crate) ownership: &'a Ownership,
    /// Package name of the kernel application crate (`entry`).
    pub(crate) app_package: String,
    /// Declared kfeat feature -> Kconfig symbols that select it.
    feature_symbols: BTreeMap<String, BTreeSet<String>>,
    /// Mapped kfeat feature -> other mapped features Kconfig forces with it.
    kconfig_implied: BTreeMap<String, BTreeSet<String>>,
    /// Workspace feature -> kfeat features whose Cargo feature closure reaches it.
    reached_from: BTreeMap<(String, String), BTreeSet<String>>,
}

impl<'a> FeatureContext<'a> {
    pub(crate) fn new(
        workspace: &'a Workspace,
        kconfig: &'a KconfigModel,
        ownership: &'a Ownership,
        app_package: String,
    ) -> Self {
        let feature_symbols: BTreeMap<_, _> = kconfig
            .feature_map()
            .into_iter()
            .filter(|(feature, _)| ownership.features.contains_key(feature))
            .collect();
        let implied_symbols: BTreeMap<&String, BTreeSet<String>> = feature_symbols
            .values()
            .flatten()
            .map(|symbol| {
                let mut implied = kconfig.implied_by(symbol);
                implied.insert(symbol.clone());
                (symbol, implied)
            })
            .collect();
        let mut kconfig_implied = BTreeMap::new();
        for (feature, symbols) in &feature_symbols {
            // A feature selected by several symbols (one per board) implies B
            // only when every one of those symbols does.
            let implied: BTreeSet<String> = feature_symbols
                .iter()
                .filter(|(other, other_symbols)| {
                    *other != feature
                        && symbols.iter().all(|symbol| {
                            implied_symbols[symbol]
                                .iter()
                                .any(|implied| other_symbols.contains(implied))
                        })
                })
                .map(|(other, _)| other.clone())
                .collect();
            kconfig_implied.insert(feature.clone(), implied);
        }
        let reached_from = reached_from(workspace, ownership);
        Self {
            workspace,
            kconfig,
            ownership,
            app_package,
            feature_symbols,
            kconfig_implied,
            reached_from,
        }
    }

    fn is_mapped(&self, feature: &str) -> bool {
        self.feature_symbols.contains_key(feature)
    }

    fn symbols_of(&self, feature: &str) -> String {
        self.feature_symbols.get(feature).map_or_else(
            || format!("(no Kconfig symbol for `{feature}`)"),
            |symbols| symbols.iter().cloned().collect::<Vec<_>>().join("|"),
        )
    }

    /// kfeat features enabled whenever `feature` is: kfeat-internal edges plus
    /// Kconfig implications between mapped features, to a fixpoint.
    pub(crate) fn enabled_with(&self, feature: &str) -> BTreeSet<String> {
        let mut closure = BTreeSet::new();
        let mut pending = vec![feature.to_string()];
        while let Some(name) = pending.pop() {
            if !closure.insert(name.clone()) {
                continue;
            }
            pending.extend(self.ownership.internal_closure(&name));
            pending.extend(
                self.kconfig_implied
                    .get(&name)
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
        }
        closure
    }

    fn implies(&self, owner: &str, target: &str) -> bool {
        owner == target || self.enabled_with(owner).contains(target)
    }

    fn kfeat_file(&self) -> (String, Option<&Package>) {
        let kfeat = self.workspace.packages.get(KFEAT_PACKAGE);
        let file = kfeat.map_or_else(
            || "api/kfeat/Cargo.toml".to_string(),
            |kfeat| self.workspace.relative(&kfeat.manifest_path),
        );
        (file, kfeat)
    }
}

/// F1: KFEAT symbols and kfeat features map one-to-one, apart from
/// documented internal features reachable from a mapped feature.
pub(crate) fn check_feature_map(context: &FeatureContext<'_>) -> Vec<Finding> {
    let mut findings = Vec::new();
    let ownership = context.ownership;
    let (kfeat_file, kfeat) = context.kfeat_file();
    let feature_line = |feature: &str| kfeat.and_then(|kfeat| kfeat.feature_line(feature, None));

    for (symbol, feature) in context.kconfig.feature_symbols() {
        if !ownership.features.contains_key(&feature) {
            let location = context.kconfig.locations.get(symbol);
            findings.push(finding(
                Lint::FeatureMap,
                location.map_or("Kconfig", |location| location.file.as_str()),
                location.map(|location| location.line),
                symbol.clone(),
                format!(
                    "{symbol} has no kfeat feature `{feature}`; xkmake would pass an unknown \
                     feature"
                ),
            ));
        }
    }

    let roots = feature_map_roots(context);
    let reachable: BTreeSet<String> = roots
        .iter()
        .flat_map(|root| ownership.internal_closure(root))
        .collect();
    for feature in ownership.features.keys() {
        if feature == "default" || context.is_mapped(feature) || is_platform_feature(feature) {
            continue;
        }
        let message = if !ownership.internal.contains(feature) {
            format!(
                "no Kconfig symbol maps to kfeat feature `{feature}`; add a KFEAT_* symbol, or \
                 list it in [package.metadata.kfeat] internal if a mapped feature selects it"
            )
        } else if !reachable.contains(feature) {
            format!(
                "internal kfeat feature `{feature}` is not reachable from any Kconfig-mapped \
                 feature or `{}/unittest`",
                context.app_package
            )
        } else {
            continue;
        };
        findings.push(finding(
            Lint::FeatureMap,
            kfeat_file.clone(),
            feature_line(feature),
            feature.clone(),
            message,
        ));
    }

    for internal in &ownership.internal {
        let message = if !ownership.features.contains_key(internal) {
            format!("internal entry `{internal}` names no kfeat feature")
        } else if context.is_mapped(internal) {
            format!(
                "internal entry `{internal}` is selected by {}; remove it from the internal list",
                context.symbols_of(internal)
            )
        } else {
            continue;
        };
        findings.push(finding(
            Lint::FeatureMap,
            kfeat_file.clone(),
            None,
            internal.clone(),
            message,
        ));
    }
    findings
}

/// For every workspace feature, the kfeat features whose feature closure
/// (local and `crate/feat` edges across crates) enables it.
fn reached_from(
    workspace: &Workspace,
    ownership: &Ownership,
) -> BTreeMap<(String, String), BTreeSet<String>> {
    let mut reached: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for root in ownership.features.keys() {
        let mut visited = BTreeSet::new();
        let mut pending = vec![(KFEAT_PACKAGE.to_string(), root.clone())];
        while let Some(node) = pending.pop() {
            if !visited.insert(node.clone()) {
                continue;
            }
            let (package_name, feature) = &node;
            if let Some(package) = workspace.packages.get(package_name) {
                for value in package.features.get(feature).into_iter().flatten() {
                    match value {
                        FeatureValue::Local(next) => {
                            pending.push((package_name.clone(), next.clone()))
                        }
                        FeatureValue::Forward { dep, feature, .. } => {
                            pending.push((package.package_of(dep), feature.clone()));
                        }
                        FeatureValue::Dep(_) => {}
                    }
                }
            }
            reached.entry(node).or_default().insert(root.clone());
        }
    }
    reached
}

fn is_platform_feature(feature: &str) -> bool {
    feature.starts_with("platform_kplat_")
}

/// Features selected from outside kfeat: Kconfig-mapped features, platform
/// HAL features, and kfeat features the app's `unittest` feature forwards.
fn feature_map_roots(context: &FeatureContext<'_>) -> BTreeSet<String> {
    let mut roots: BTreeSet<String> = context
        .ownership
        .features
        .keys()
        .filter(|feature| context.is_mapped(feature) || is_platform_feature(feature))
        .cloned()
        .collect();
    if let Some(app) = context.workspace.packages.get(&context.app_package) {
        for value in app.features.get("unittest").into_iter().flatten() {
            if let FeatureValue::Forward { dep, feature, .. } = value
                && app.package_of(dep) == KFEAT_PACKAGE
            {
                roots.insert(feature.clone());
            }
        }
    }
    roots
}

/// F2: a mapped kfeat feature that enables another mapped feature (directly
/// or through internal features) needs the same implication in Kconfig.
pub(crate) fn check_kconfig_implication(context: &FeatureContext<'_>) -> Vec<Finding> {
    let mut findings = Vec::new();
    let (kfeat_file, kfeat) = context.kfeat_file();
    for feature in context.feature_symbols.keys() {
        for (via, target) in mapped_successors(context, feature) {
            if context.kconfig_implied[feature].contains(&target) {
                continue;
            }
            let line = kfeat.and_then(|kfeat| kfeat.feature_line(&via.0, Some(&via.1)));
            findings.push(finding(
                Lint::KconfigImplication,
                kfeat_file.clone(),
                line,
                format!("{feature} -> {target}"),
                format!(
                    "kfeat `{feature}` enables `{target}`, but {} does not imply {} in Kconfig \
                     (add `depends on` or `select`)",
                    context.symbols_of(feature),
                    context.symbols_of(&target)
                ),
            ));
        }
    }
    findings
}

/// Mapped features reachable from `feature` through unmapped kfeat features,
/// with the kfeat edge `(from, to)` that leaves the start feature's region.
fn mapped_successors(
    context: &FeatureContext<'_>,
    feature: &str,
) -> Vec<((String, String), String)> {
    let mut found = Vec::new();
    let mut visited = BTreeSet::from([feature.to_string()]);
    let mut pending = vec![feature.to_string()];
    while let Some(name) = pending.pop() {
        for value in context.ownership.features.get(&name).into_iter().flatten() {
            let FeatureValue::Local(next) = value else {
                continue;
            };
            if !visited.insert(next.clone()) {
                continue;
            }
            if context.is_mapped(next) {
                found.push(((name.clone(), next.clone()), next.clone()));
            } else {
                pending.push(next.clone());
            }
        }
    }
    found
}

/// A crate edge that turned on a capability feature in a resolved graph.
struct Enabler {
    subject: String,
    file: String,
    line: Option<usize>,
    via: &'static str,
}

/// F3: every capability feature Cargo enables has an enabled kfeat owner.
pub(crate) fn check_capability_bypass(
    context: &FeatureContext<'_>,
    resolution: &Resolution,
) -> Vec<Finding> {
    let empty = BTreeSet::new();
    let kfeat_enabled = resolution.packages.get(KFEAT_PACKAGE).unwrap_or(&empty);
    let mut findings = Vec::new();
    for (package, features) in &resolution.packages {
        for feature in features {
            let Some(owners) = context.ownership.owners(package, feature) else {
                continue;
            };
            if owners.iter().any(|owner| kfeat_enabled.contains(owner)) {
                continue;
            }
            let enablers = enablers(context, resolution, package, feature);
            let enablers = if enablers.is_empty() {
                vec![Enabler {
                    subject: "?".to_string(),
                    file: resolution.defconfig.clone(),
                    line: None,
                    via: "an unidentified edge (see path)",
                }]
            } else {
                enablers
            };
            for enabler in enablers {
                let mut finding = finding(
                    Lint::CapabilityBypass,
                    enabler.file,
                    enabler.line,
                    format!("{} -> {package}/{feature}", enabler.subject),
                    format!(
                        "`{package}/{feature}` is enabled by {} although no kfeat owner is enabled",
                        enabler.via
                    ),
                )
                .with_owners(owners);
                finding.defconfig = Some(resolution.defconfig.clone());
                findings.push(finding);
            }
        }
    }
    findings
}

fn enablers(
    context: &FeatureContext<'_>,
    resolution: &Resolution,
    package: &str,
    feature: &str,
) -> Vec<Enabler> {
    let mut found = Vec::new();
    let workspace = context.workspace;
    let target_package = workspace.packages.get(package);
    let is_in_default = target_package
        .is_some_and(|target| target.features.contains_key("default"))
        && target_package.is_some_and(|target| target.local_closure("default").contains(feature));
    for (name, enabled) in &resolution.packages {
        if name == KFEAT_PACKAGE {
            continue;
        }
        let Some(source) = workspace.packages.get(name) else {
            continue;
        };
        let file = workspace.relative(&source.manifest_path);
        for dependency in &source.dependencies {
            if dependency.package != package
                || dependency.kind == DepKind::Dev
                || !is_dependency_active(source, enabled, dependency)
            {
                continue;
            }
            let via = if dependency.features.iter().any(|value| value == feature) {
                "a dependency `features` list"
            } else if dependency.uses_default_features && is_in_default {
                "the dependency's default features"
            } else {
                continue;
            };
            found.push(Enabler {
                subject: name.clone(),
                file: file.clone(),
                line: source.dependency_line(&dependency.key),
                via,
            });
        }
        for source_feature in enabled {
            for value in source.features.get(source_feature).into_iter().flatten() {
                let is_edge = match value {
                    FeatureValue::Forward {
                        dep,
                        feature: target,
                        ..
                    } => target == feature && source.package_of(dep) == package,
                    FeatureValue::Local(target) => name == package && target == feature,
                    FeatureValue::Dep(_) => false,
                };
                if is_edge {
                    found.push(Enabler {
                        subject: format!("{name}/{source_feature}"),
                        file: file.clone(),
                        line: source.feature_line(source_feature, None),
                        via: "a feature edge",
                    });
                }
            }
        }
    }
    found
}

/// Whether an optional dependency is activated by the enabled features.
fn is_dependency_active(
    package: &Package,
    enabled: &BTreeSet<String>,
    dependency: &Dependency,
) -> bool {
    if !dependency.is_optional || enabled.contains(&dependency.key) {
        return true;
    }
    enabled.iter().any(|feature| {
        package
            .features
            .get(feature)
            .into_iter()
            .flatten()
            .any(|value| match value {
                FeatureValue::Dep(key) => key == &dependency.key,
                FeatureValue::Forward { dep, is_weak, .. } => !is_weak && dep == &dependency.key,
                FeatureValue::Local(_) => false,
            })
    })
}

/// R1: an optional kfeat dependency is linked although none of the kfeat
/// features that activate it is enabled.
pub(crate) fn report_linked_without_owner(
    context: &FeatureContext<'_>,
    resolution: &Resolution,
) -> Vec<Finding> {
    let empty = BTreeSet::new();
    let kfeat_enabled = resolution.packages.get(KFEAT_PACKAGE).unwrap_or(&empty);
    let (kfeat_file, kfeat) = context.kfeat_file();
    let mut findings = Vec::new();
    for (package, owners) in &context.ownership.dependency_owners {
        if !resolution.is_linked(package)
            || owners.iter().any(|owner| kfeat_enabled.contains(owner))
        {
            continue;
        }
        let mut finding = finding(
            Lint::LinkedWithoutOwner,
            kfeat_file.clone(),
            kfeat.and_then(|kfeat| {
                kfeat
                    .dependencies
                    .iter()
                    .find(|dependency| dependency.package == *package)
                    .and_then(|dependency| kfeat.dependency_line(&dependency.key))
            }),
            package.clone(),
            format!("`{package}` is linked although no kfeat feature that activates it is enabled"),
        )
        .with_owners(owners);
        finding.defconfig = Some(resolution.defconfig.clone());
        findings.push(finding);
    }
    findings
}

/// F4: ordinary crates forward capability features only under the same
/// Kconfig-backed ownership, and never enable one unconditionally.
pub(crate) fn check_peer_forwarding(context: &FeatureContext<'_>) -> Vec<Finding> {
    let mut findings = Vec::new();
    let ownership = context.ownership;
    for (name, package) in &context.workspace.packages {
        if name == KFEAT_PACKAGE {
            continue;
        }
        let file = context.workspace.relative(&package.manifest_path);
        if *name == context.app_package {
            findings.extend(app_forwarding(package, &file));
            continue;
        }
        for (feature, values) in &package.features {
            for value in values {
                let FeatureValue::Forward {
                    dep,
                    feature: target_feature,
                    ..
                } = value
                else {
                    continue;
                };
                let target = package.package_of(dep);
                let Some(target_owners) = ownership.owners(&target, target_feature) else {
                    continue;
                };
                let line = package.feature_line(feature, Some(&value_text(value)));
                let subject = format!("{name}/{feature} -> {target}/{target_feature}");
                let source = (name.as_str(), feature.as_str());
                let target_ref = (target.as_str(), target_feature.as_str());
                let Some(message) = forwarding_problem(context, source, target_ref, target_owners)
                else {
                    continue;
                };
                findings.push(
                    finding(Lint::PeerForwarding, file.clone(), line, subject, message)
                        .with_owners(target_owners),
                );
            }
        }
        for dependency in &package.dependencies {
            if dependency.kind == DepKind::Dev {
                continue;
            }
            for (target_feature, via) in unconditional_capabilities(context, dependency) {
                let owners = ownership
                    .owners(&dependency.package, &target_feature)
                    .into_iter()
                    .flatten();
                findings.push(
                    finding(
                        Lint::PeerForwarding,
                        file.clone(),
                        package.dependency_line(&dependency.key),
                        format!("{name} -> {}/{target_feature}", dependency.package),
                        format!(
                            "{via} of `{}` unconditionally enables the kfeat-owned capability \
                             `{}/{target_feature}`",
                            dependency.key, dependency.package
                        ),
                    )
                    .with_owners(owners),
                );
            }
        }
    }
    findings
}

/// Why the edge `source -> target` breaks ownership, if it does.
fn forwarding_problem(
    context: &FeatureContext<'_>,
    (name, feature): (&str, &str),
    (target, target_feature): (&str, &str),
    target_owners: &BTreeSet<String>,
) -> Option<String> {
    let implies_target = |owner: &String| {
        target_owners
            .iter()
            .any(|target_owner| context.implies(owner, target_owner))
    };
    if let Some(source_owners) = context.ownership.owners(name, feature) {
        let unrelated: Vec<&String> = source_owners
            .iter()
            .filter(|owner| !implies_target(owner))
            .collect();
        return (!unrelated.is_empty()).then(|| {
            format!(
                "kfeat owner(s) {} of `{name}/{feature}` do not imply an owner of \
                 `{target}/{target_feature}` in Kconfig",
                join_quoted(unrelated)
            )
        });
    }
    let reached = context
        .reached_from
        .get(&(name.to_string(), feature.to_string()))
        .cloned()
        .unwrap_or_default();
    let origin = if reached.is_empty() {
        "no kfeat feature reaches it".to_string()
    } else if reached.iter().all(implies_target) {
        format!(
            "it is reached from kfeat {} (an implementation chain under the same owner)",
            join_quoted(&reached)
        )
    } else {
        format!(
            "it is reached from kfeat {}, which does not imply the capability",
            join_quoted(&reached)
        )
    };
    Some(format!(
        "`{name}/{feature}` forwards the kfeat-owned capability `{target}/{target_feature}` but \
         kfeat does not assemble `{name}/{feature}`; {origin}"
    ))
}

/// Capability features a dependency declaration turns on by itself.
fn unconditional_capabilities(
    context: &FeatureContext<'_>,
    dependency: &Dependency,
) -> Vec<(String, &'static str)> {
    let ownership = context.ownership;
    let mut found: Vec<(String, &'static str)> = dependency
        .features
        .iter()
        .filter(|feature| ownership.owners(&dependency.package, feature).is_some())
        .map(|feature| (feature.clone(), "the `features` list"))
        .collect();
    if dependency.uses_default_features
        && let Some(target) = context.workspace.packages.get(&dependency.package)
        && target.features.contains_key("default")
    {
        for feature in target.local_closure("default") {
            if ownership.owners(&dependency.package, &feature).is_some()
                && !found.iter().any(|(known, _)| *known == feature)
            {
                found.push((feature, "the default features"));
            }
        }
    }
    found
}

/// The app crate may forward to kfeat only through `unittest`.
fn app_forwarding(app: &Package, file: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (feature, values) in &app.features {
        if feature == "unittest" {
            continue;
        }
        for value in values {
            if let FeatureValue::Forward {
                dep,
                feature: target,
                ..
            } = value
                && app.package_of(dep) == KFEAT_PACKAGE
            {
                findings.push(finding(
                    Lint::PeerForwarding,
                    file,
                    app.feature_line(feature, Some(&value_text(value))),
                    format!("{}/{feature} -> {KFEAT_PACKAGE}/{target}", app.name),
                    format!(
                        "`{}/{feature}` selects `{KFEAT_PACKAGE}/{target}` outside Kconfig; only \
                         `unittest` may forward to kfeat",
                        app.name
                    ),
                ));
            }
        }
    }
    findings
}

pub(super) fn value_text(value: &FeatureValue) -> String {
    match value {
        FeatureValue::Local(feature) => feature.clone(),
        FeatureValue::Dep(dep) => format!("dep:{dep}"),
        FeatureValue::Forward {
            dep,
            is_weak,
            feature,
        } => format!("{dep}{}/{feature}", if *is_weak { "?" } else { "" }),
    }
}

fn join_quoted<'a>(items: impl IntoIterator<Item = &'a String>) -> String {
    items
        .into_iter()
        .map(|item| format!("`{item}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Cargo flags that override the Kconfig-derived feature set.
const FEATURE_FLAGS: [&str; 4] = [
    "--features",
    "-F",
    "--all-features",
    "--no-default-features",
];

/// F5: build wiring passes no feature selection besides xkmake's own.
///
/// `files` holds `(relative path, text)` of the scanned build files.
pub(crate) fn check_build_args(workspace: &Workspace, files: &[(String, String)]) -> Vec<Finding> {
    let kernel_directories: BTreeSet<String> = workspace
        .packages
        .values()
        .map(|package| workspace.relative(package.directory()))
        .chain(std::iter::once(String::new()))
        .collect();
    let mut findings = Vec::new();
    for (file, text) in files {
        for (line, command) in logical_lines(text) {
            for literal in kfeat_literals(&command) {
                findings.push(finding(
                    Lint::BuildArgs,
                    file.clone(),
                    Some(line),
                    literal.clone(),
                    format!(
                        "`{literal}` selects a kfeat feature outside Kconfig; set the KFEAT_* \
                         symbol in the defconfig instead"
                    ),
                ));
            }
            for (subcommand, flag) in kernel_feature_flags(&command, &kernel_directories) {
                findings.push(finding(
                    Lint::BuildArgs,
                    file.clone(),
                    Some(line),
                    format!("cargo {subcommand} {flag}"),
                    format!(
                        "`cargo {subcommand} {flag}` on the kernel workspace overrides the \
                         Kconfig-derived feature set; build through make/xkmake"
                    ),
                ));
            }
        }
    }
    findings
}

/// Joins `\`-continued lines and drops comment lines; yields 1-based start lines.
fn logical_lines(text: &str) -> Vec<(usize, String)> {
    let mut lines = Vec::new();
    let mut current: Option<(usize, String)> = None;
    for (index, raw) in text.lines().enumerate() {
        let trimmed = raw.trim();
        let is_comment = trimmed.starts_with('#') || trimmed.starts_with("//");
        if is_comment && current.is_none() {
            continue;
        }
        let (body, continues) = match raw.trim_end().strip_suffix('\\') {
            Some(body) => (body, true),
            None => (raw, false),
        };
        let entry = current.get_or_insert_with(|| (index + 1, String::new()));
        if !is_comment {
            entry.1.push(' ');
            entry.1.push_str(body);
        }
        if !continues {
            lines.extend(current.take());
        }
    }
    lines.extend(current);
    lines
}

fn kfeat_literals(command: &str) -> Vec<String> {
    command
        .match_indices("kfeat/")
        .filter(|(offset, _)| {
            // `api/kfeat/Cargo.toml` is a path, not a feature string.
            !command[..*offset].ends_with(|ch: char| ch.is_alphanumeric() || ch == '/' || ch == '_')
        })
        .map(|(offset, _)| {
            let rest = &command[offset + "kfeat/".len()..];
            let feature: String = rest
                .chars()
                .take_while(|ch| {
                    ch.is_alphanumeric() || matches!(ch, '_' | '-' | '$' | '(' | ')' | '{' | '}')
                })
                .collect();
            format!("kfeat/{feature}")
        })
        .collect()
}

/// `(subcommand, flag)` for each kernel-workspace cargo call with feature flags.
fn kernel_feature_flags(
    command: &str,
    kernel_directories: &BTreeSet<String>,
) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut directory: Option<String> = None;
    for segment in command.split(['&', ';', '|']) {
        let tokens: Vec<&str> = segment
            .split_whitespace()
            .map(|token| token.trim_matches(['"', '\'']))
            .collect();
        if let [first, target, ..] = tokens.as_slice()
            && (*first == "cd" || *first == "pushd")
        {
            directory = Some((*target).to_string());
        }
        let Some(cargo) = tokens.iter().position(|token| is_cargo_program(token)) else {
            continue;
        };
        let arguments = &tokens[cargo + 1..];
        let Some(flag) = arguments.iter().find_map(|argument| feature_flag(argument)) else {
            continue;
        };
        let manifest = arguments.iter().enumerate().find_map(|(index, argument)| {
            argument.strip_prefix("--manifest-path=").or_else(|| {
                (*argument == "--manifest-path")
                    .then(|| arguments.get(index + 1).copied())
                    .flatten()
            })
        });
        let target_directory = match manifest {
            Some(manifest) => normalize_directory(manifest.trim_end_matches("Cargo.toml")),
            None => directory
                .as_deref()
                .map_or_else(String::new, normalize_directory),
        };
        if kernel_directories.contains(&target_directory) {
            let subcommand = arguments
                .iter()
                .find(|argument| !argument.starts_with(['-', '+']))
                .copied()
                .unwrap_or("");
            found.push((subcommand.to_string(), flag.to_string()));
        }
    }
    found
}

fn is_cargo_program(token: &str) -> bool {
    token == "cargo"
        || token.ends_with("/cargo")
        || matches!(token, "$(CARGO)" | "${CARGO}" | "$CARGO")
}

fn feature_flag(argument: &str) -> Option<&'static str> {
    FEATURE_FLAGS.into_iter().find(|flag| {
        argument == *flag
            || argument
                .strip_prefix(flag)
                .is_some_and(|rest| rest.starts_with('=') || (*flag == "-F" && !rest.is_empty()))
    })
}

/// Workspace-relative directory with `./`, `$(CURDIR)/` and similar prefixes removed.
fn normalize_directory(path: &str) -> String {
    let mut path = path.trim();
    for prefix in [
        "$(CURDIR)",
        "${CURDIR}",
        "$CURDIR",
        "${WORKSPACE}",
        "$WORKSPACE",
        "$(pwd)",
        "${PWD}",
        "$PWD",
    ] {
        path = path.strip_prefix(prefix).unwrap_or(path);
    }
    let mut path = path.trim_start_matches('/');
    while let Some(rest) = path.strip_prefix("./") {
        path = rest;
    }
    let path = path.trim_end_matches('/');
    if path == "." {
        String::new()
    } else {
        path.to_string()
    }
}

/// R2: declared features with no effect: empty list, no `feature = "x"` in
/// the crate's sources, and no reference from any manifest.
///
/// `cfg_features` maps a package name to the feature names its sources test.
pub(crate) fn report_unused_features(
    workspace: &Workspace,
    cfg_features: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<Finding> {
    let mut referenced: BTreeSet<(String, String)> = BTreeSet::new();
    for package in workspace.packages.values() {
        for values in package.features.values() {
            for value in values {
                if let FeatureValue::Forward { dep, feature, .. } = value {
                    referenced.insert((package.package_of(dep), feature.clone()));
                }
            }
        }
        for dependency in &package.dependencies {
            for feature in &dependency.features {
                referenced.insert((dependency.package.clone(), feature.clone()));
            }
        }
    }
    let mut findings = Vec::new();
    for (name, package) in &workspace.packages {
        if name == KFEAT_PACKAGE {
            continue;
        }
        let used_in_source = cfg_features.get(name);
        for (feature, values) in &package.features {
            if feature == "default"
                || !values.is_empty()
                || package.required_features.contains(feature)
                || used_in_source.is_some_and(|used| is_feature_in_source(used, feature))
                || referenced.contains(&(name.clone(), feature.clone()))
            {
                continue;
            }
            findings.push(finding(
                Lint::UnusedFeature,
                workspace.relative(&package.manifest_path),
                package.feature_line(feature, None),
                format!("{name}/{feature}"),
                format!(
                    "`{name}/{feature}` is empty, untested by `cfg(feature = \"{feature}\")` in \
                     its sources and referenced by no manifest"
                ),
            ));
        }
    }
    findings
}

/// Feature names tested in Rust source: `feature = "x"` and build-script
/// `CARGO_FEATURE_X` lookups (returned in that upper-case spelling).
pub(crate) fn source_feature_names(text: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for (offset, _) in text.match_indices("feature") {
        let rest = text[offset + "feature".len()..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('"') else {
            continue;
        };
        if let Some((name, _)) = rest.split_once('"') {
            names.insert(name.to_string());
        }
    }
    for (offset, _) in text.match_indices("CARGO_FEATURE_") {
        let name: String = text[offset..]
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
            .collect();
        names.insert(name);
    }
    names
}

/// Whether `feature` matches a name from [`source_feature_names`].
pub(crate) fn is_feature_in_source(names: &BTreeSet<String>, feature: &str) -> bool {
    names.contains(feature)
        || names.contains(&format!(
            "CARGO_FEATURE_{}",
            feature.to_ascii_uppercase().replace('-', "_")
        ))
}
