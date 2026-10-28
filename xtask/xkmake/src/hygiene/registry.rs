// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Registry of xkmake rule checks, their shared inputs and the runner.
//!
//! Every registered `(rule, check)` pair is listed under `backend: xkmake` in
//! the rule's `docs/code-guidelines/CG-*.md` frontmatter and vice versa; a
//! unit test keeps the two in step.

use std::{
    cell::OnceCell,
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use super::{
    exceptions,
    report::{self, Finding, Severity},
    test_layout::TestSourceLayout,
};
use crate::{
    cli::ReportFormat,
    context::workspace_root,
    error::{Error, IoResultExt, Result},
    features::{FeatureOwnership, KconfigModel, Resolution, Workspace},
};

/// One rule of `docs/code-guidelines/` checked mechanically by xkmake.
pub(crate) trait RuleCheck: Sync {
    /// Full rule id, e.g. `CG-047-kconfig-feature-ownership`.
    fn rule(&self) -> &'static str;

    /// Check names; equal to the rule file's `backend: xkmake` lint list.
    /// Only findings of these checks fail the run or accept exceptions.
    fn checks(&self) -> &'static [&'static str];

    /// Report-only check names, run only with `--report`.
    fn report_checks(&self) -> &'static [&'static str] {
        &[]
    }

    /// Findings of the selected checks (see [`Context::wants`]).
    fn run(&self, context: &Context) -> Result<Vec<Finding>>;

    /// Adds costly diagnostics to findings that survived the exceptions.
    fn annotate(&self, _context: &Context, _findings: &mut [Finding]) -> Result<()> {
        Ok(())
    }
}

/// Every registered rule check, in rule id order.
pub(crate) static RULES: &[&dyn RuleCheck] = &[&FeatureOwnership, &TestSourceLayout];

/// Looks up a registered rule by its full id.
pub(crate) fn find_rule(id: &str) -> Option<&'static dyn RuleCheck> {
    RULES.iter().copied().find(|rule| rule.rule() == id)
}

/// What `hygiene check` (or one of its aliases) runs.
#[derive(Clone, Debug)]
pub(crate) struct CheckRequest {
    /// Rule ids or their `CG-NNN` prefix; empty selects every rule.
    pub(crate) rules: Vec<String>,
    /// Check names; empty selects every check of the selected rules.
    pub(crate) checks: Vec<String>,
    /// Defconfigs to resolve; empty means every `platforms/**/*defconfig`.
    pub(crate) defconfigs: Vec<PathBuf>,
    pub(crate) format: ReportFormat,
    /// Also run and print report-only checks.
    pub(crate) report: bool,
    /// Kernel application crate whose feature graph is resolved.
    pub(crate) app: PathBuf,
}

/// Inputs shared by rule checks, each computed on first use.
pub(crate) struct Context {
    root: PathBuf,
    app: PathBuf,
    defconfigs: Vec<PathBuf>,
    /// Selected `(rule, check)` pairs, report-only checks included.
    selected: BTreeSet<(&'static str, &'static str)>,
    workspace: OnceCell<Workspace>,
    files: OnceCell<Vec<String>>,
    kconfig: OnceCell<KconfigModel>,
    resolutions: OnceCell<Vec<Resolution>>,
}

impl Context {
    fn new(
        root: PathBuf,
        request: &CheckRequest,
        selected: BTreeSet<(&'static str, &'static str)>,
    ) -> Self {
        Self {
            defconfigs: request
                .defconfigs
                .iter()
                .map(|path| root.join(path))
                .collect(),
            root,
            app: request.app.clone(),
            selected,
            workspace: OnceCell::new(),
            files: OnceCell::new(),
            kconfig: OnceCell::new(),
            resolutions: OnceCell::new(),
        }
    }

    /// Repository root.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Kernel application crate, relative to the root.
    pub(crate) fn app(&self) -> &Path {
        &self.app
    }

    /// Whether `check` of `rule` runs; report-only checks need `--report`.
    pub(crate) fn wants(&self, rule: &str, check: &str) -> bool {
        self.selected.contains(&(rule, check))
    }

    /// Whether every platform defconfig is resolved.
    pub(crate) fn has_all_defconfigs(&self) -> bool {
        self.defconfigs.is_empty()
    }

    /// Root workspace members from `cargo metadata --no-deps`.
    pub(crate) fn workspace(&self) -> Result<&Workspace> {
        if let Some(workspace) = self.workspace.get() {
            return Ok(workspace);
        }
        let workspace = Workspace::load(&self.root)?;
        Ok(self.workspace.get_or_init(|| workspace))
    }

    /// Tracked and unignored files, relative to the root with `/` separators.
    pub(crate) fn files(&self) -> Result<&[String]> {
        if let Some(files) = self.files.get() {
            return Ok(files);
        }
        let files = list_files(&self.root)?;
        Ok(self.files.get_or_init(|| files))
    }

    /// Parsed Kconfig tree.
    pub(crate) fn kconfig(&self) -> Result<&KconfigModel> {
        if let Some(kconfig) = self.kconfig.get() {
            return Ok(kconfig);
        }
        let kconfig = KconfigModel::load(&self.root)?;
        Ok(self.kconfig.get_or_init(|| kconfig))
    }

    /// Cargo feature resolution of each selected defconfig.
    pub(crate) fn resolutions(&self) -> Result<&[Resolution]> {
        if let Some(resolutions) = self.resolutions.get() {
            return Ok(resolutions);
        }
        let resolutions =
            crate::features::resolve_defconfigs(&self.root, &self.app, &self.defconfigs)?;
        Ok(self.resolutions.get_or_init(|| resolutions))
    }
}

/// Selected `(rule, check)` pairs of `request`; unknown names are an error.
fn select(request: &CheckRequest) -> Result<BTreeSet<(&'static str, &'static str)>> {
    let mut rules = Vec::new();
    for wanted in &request.rules {
        let matched: Vec<_> = RULES
            .iter()
            .copied()
            .filter(|rule| {
                rule.rule() == wanted
                    || rule
                        .rule()
                        .strip_prefix(wanted.as_str())
                        .is_some_and(|rest| rest.starts_with('-'))
            })
            .collect();
        if matched.is_empty() {
            return Err(Error::Message(format!(
                "unknown rule `{wanted}`; registered rules: {}",
                RULES
                    .iter()
                    .map(|rule| rule.rule())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        rules.extend(matched);
    }
    if request.rules.is_empty() {
        rules.extend(RULES.iter().copied());
    }
    let mut selected = BTreeSet::new();
    for rule in &rules {
        let report_checks = if request.report {
            rule.report_checks()
        } else {
            &[]
        };
        for &check in rule.checks().iter().chain(report_checks) {
            if request.checks.is_empty() || request.checks.iter().any(|name| name == check) {
                selected.insert((rule.rule(), check));
            }
        }
    }
    for name in &request.checks {
        let is_known = rules.iter().any(|rule| {
            rule.checks()
                .iter()
                .chain(rule.report_checks())
                .any(|check| check == name)
        });
        if !is_known {
            return Err(Error::Message(format!(
                "unknown check `{name}` for the selected rules; see the `backend: xkmake` lint \
                 lists in docs/code-guidelines/CG-*.md"
            )));
        }
    }
    Ok(selected)
}

/// Runs the selected rule checks, prints their findings and fails when a
/// violation is not excepted. Findings exit 1; a check that cannot run exits 2.
pub(crate) fn run_rule_checks(request: &CheckRequest) -> Result<()> {
    let environment = |error: Error| Error::Environment(Box::new(error));
    let started = Instant::now();
    let root = workspace_root().map_err(environment)?;
    let selected = select(request).map_err(environment)?;
    let exceptions = exceptions::load(&root).map_err(environment)?;
    let context = Context::new(root, request, selected);

    let rules: Vec<&dyn RuleCheck> = RULES
        .iter()
        .copied()
        .filter(|rule| context.selected.iter().any(|(id, _)| *id == rule.rule()))
        .collect();
    let mut findings = Vec::new();
    for rule in &rules {
        let found = rule
            .run(&context)
            .map_err(|error| environment(Error::Message(format!("{}: {error}", rule.rule()))))?;
        findings.extend(found.into_iter().filter(|finding| {
            finding.severity == Severity::Diff || context.wants(finding.rule, finding.check)
        }));
    }
    let applied = exceptions::apply(
        findings,
        &exceptions,
        &exceptions::Scope {
            is_rule_selected: &|rule| rules.iter().any(|selected| selected.rule() == rule),
            is_complete: &|rule, check| context.wants(rule, check) && context.has_all_defconfigs(),
        },
    );
    let mut findings = applied.kept;
    for rule in &rules {
        let start = findings.partition_point(|finding| finding.rule < rule.rule());
        let end = findings.partition_point(|finding| finding.rule <= rule.rule());
        rule.annotate(&context, &mut findings[start..end])
            .map_err(|error| environment(Error::Message(format!("{}: {error}", rule.rule()))))?;
    }

    report::print_findings(&findings, request.format, request.report)?;
    let errors = report::count(&findings, Severity::Error);
    eprintln!(
        "repository hygiene: {} rule(s), {errors} violation(s), {} excepted, {} report item(s) in \
         {:.1}s",
        rules.len(),
        applied.suppressed,
        report::count(&findings, Severity::Report),
        started.elapsed().as_secs_f64()
    );
    if errors == 0 {
        return Ok(());
    }
    Err(Error::Message(format!(
        "{errors} repository rule violation(s); fix them or register an exception in {}",
        exceptions::EXCEPTIONS_FILE
    )))
}

/// Tracked and unignored files, relative to `root`.
///
/// Falls back to a directory walk when `root` is not a Git checkout (for
/// example an exported tree).
fn list_files(root: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .current_dir(root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .output();
    if let Ok(output) = output
        && output.status.success()
    {
        let stdout = String::from_utf8(output.stdout)
            .map_err(|error| Error::Message(format!("git returned a non-UTF-8 path: {error}")))?;
        return Ok(stdout
            .split('\0')
            .filter(|path| !path.is_empty() && root.join(path).is_file())
            .map(str::to_string)
            .collect());
    }
    let mut files = Vec::new();
    walk_files(root, root, &mut files)?;
    files.sort();
    Ok(files)
}

fn walk_files(root: &Path, path: &Path, files: &mut Vec<String>) -> Result<()> {
    if path.is_file() {
        if let Ok(relative) = path.strip_prefix(root) {
            files.push(relative.to_string_lossy().replace('\\', "/"));
        }
        return Ok(());
    }
    if !path.is_dir()
        || path
            .file_name()
            .is_some_and(|name| name == "target" || name == ".git")
    {
        return Ok(());
    }
    for entry in fs::read_dir(path).with_path(path)? {
        walk_files(root, &entry.with_path(path)?.path(), files)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs,
        path::Path,
    };

    use super::RULES;

    /// `id` and `backend: xkmake` lint names of one rule file's frontmatter.
    fn xkmake_lints(text: &str) -> Option<(String, Vec<String>)> {
        let frontmatter = text.strip_prefix("---\n")?.split("\n---").next()?;
        let mut id = None;
        let mut lints = Vec::new();
        let mut is_xkmake = false;
        let mut pending: Option<String> = None;
        for line in frontmatter.lines() {
            let trimmed = line.trim();
            if let Some(list) = pending.as_mut() {
                list.push_str(trimmed);
            } else if let Some(value) = trimmed.strip_prefix("id:") {
                id = Some(value.trim().to_string());
            } else if let Some(backend) = trimmed.strip_prefix("- backend:") {
                is_xkmake = backend.trim() == "xkmake";
            } else if let Some(list) = trimmed.strip_prefix("lint:")
                && is_xkmake
            {
                pending = Some(list.trim().to_string());
            }
            if let Some(list) = pending.take_if(|list| list.ends_with(']')) {
                let inner = list.trim_start_matches('[').trim_end_matches(']');
                lints.extend(
                    inner
                        .split(',')
                        .map(|name| name.trim().trim_matches(['"', '\'']).to_string())
                        .filter(|name| !name.is_empty()),
                );
            }
        }
        Some((id?, lints))
    }

    #[test]
    fn frontmatter_parser_reads_only_xkmake_lists() {
        let text = "---\nid: CG-1-x\nchecks:\n  - backend: clippy\n    lint: [panic]\n  - \
                    backend: xkmake\n    lint: [a, \"b\",\n      c]\n  - backend: ai\n---\n# \
                    body\n";
        assert_eq!(
            xkmake_lints(text),
            Some((
                "CG-1-x".to_string(),
                vec!["a".into(), "b".into(), "c".into()]
            ))
        );
    }

    #[test]
    fn registry_matches_rule_file_xkmake_lints() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/code-guidelines");
        let mut documented: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for entry in fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if !(name.starts_with("CG-") && name.ends_with(".md")) {
                continue;
            }
            let text = fs::read_to_string(&path).unwrap();
            let (id, lints) =
                xkmake_lints(&text).unwrap_or_else(|| panic!("{name}: no frontmatter id"));
            assert!(
                name.starts_with(&id),
                "{name}: id `{id}` does not match the file name"
            );
            if !lints.is_empty() {
                documented.insert(id, lints.into_iter().collect());
            }
        }
        let mut registered: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for rule in RULES {
            let checks: BTreeSet<String> = rule
                .checks()
                .iter()
                .map(|check| check.to_string())
                .collect();
            assert_eq!(
                checks.len(),
                rule.checks().len(),
                "{}: duplicate check",
                rule.rule()
            );
            for report in rule.report_checks() {
                assert!(
                    !checks.contains(*report),
                    "{}: {report} is both kinds",
                    rule.rule()
                );
            }
            assert!(
                registered.insert(rule.rule().to_string(), checks).is_none(),
                "{} registered twice",
                rule.rule()
            );
        }
        assert_eq!(registered, documented);
    }
}
