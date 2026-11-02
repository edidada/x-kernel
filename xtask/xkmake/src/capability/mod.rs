// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Capability catalogue checks (`xkmake capability`).
//!
//! The catalogue under `docs/capabilities/` describes reusable capabilities
//! of the kernel and its locked dependencies, one YAML card each. This module
//! validates the cards, checks every card against the source tree without
//! any model, records reviewed state in `stamps.json`, renders the helper
//! page and writes the maintenance report. Cards cite items and quotes,
//! never line numbers, so code that merely moves never changes a card.

mod card;
mod items;
mod render;
mod report;
mod source;
mod stamps;
mod verify;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

use serde::Serialize;

use self::{
    card::{Card, CardSource, Catalogue, Level, Status},
    report::{CardReport, FindingReport, Report, StaleSince},
    source::{Reference, SourceTree},
    stamps::{STAMPS_FILE, Stamps},
    verify::{CardCheck, Drift},
};
use crate::{
    cli::{CapabilityCheckArgs, CapabilityStampArgs},
    context::workspace_root,
    error::{Error, IoResultExt, Result},
};

/// Catalogue directory relative to the repository root.
pub(crate) const CATALOGUE_DIR: &str = "docs/capabilities";

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

/// Card state reported to consumers.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum State {
    Fresh,
    Stale,
    Suspended,
    Retired,
    Unchecked,
}

impl State {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Stale => "stale",
            Self::Suspended => "suspended",
            Self::Retired => "retired",
            Self::Unchecked => "unchecked",
        }
    }
}

/// Result of checking the whole catalogue once.
struct Evaluation {
    catalogue: Catalogue,
    /// Parallel to `catalogue.cards`; `None` for retired cards.
    checks: Vec<Option<CardCheck>>,
    report: Report,
}

impl Evaluation {
    fn attention_count(&self) -> usize {
        self.catalogue
            .cards
            .iter()
            .zip(&self.checks)
            .filter(|(card, check)| needs_attention(card, check.as_ref()))
            .count()
    }
}

/// A card in use or awaiting its owner whose source no longer supports it;
/// suspended and retired cards are already out of use.
fn needs_attention(card: &Card, check: Option<&CardCheck>) -> bool {
    matches!(card.status, Status::Active | Status::NeedsOwner)
        && check.is_some_and(|check| matches!(check.drift, Drift::Stale | Drift::Unchecked))
}

/// `xkmake capability check`.
pub(crate) fn check(args: &CapabilityCheckArgs) -> Result<()> {
    let workspace = workspace_root()?;
    let evaluation = run_check(
        &workspace,
        SourceTree::default_registry_roots(),
        args.report.as_deref(),
    )?;
    print_evaluation(&evaluation);
    verdict(&evaluation)
}

/// `xkmake capability stamp`.
pub(crate) fn stamp(args: &CapabilityStampArgs) -> Result<()> {
    let workspace = workspace_root()?;
    let stamped = stamp_cards(
        &workspace,
        SourceTree::default_registry_roots(),
        &args.cards,
        args.all,
        args.discovery,
    )?;
    for name in stamped {
        println!("stamped {name}");
    }
    Ok(())
}

/// `xkmake capability render`.
pub(crate) fn render() -> Result<()> {
    let workspace = workspace_root()?;
    let directory = workspace.join(CATALOGUE_DIR);
    let catalogue = load_catalogue(&directory)?;
    let path = directory.join(render::PAGE_FILE);
    std::fs::write(&path, render::render(&catalogue)).with_path(&path)?;
    println!("wrote {}", path.display());
    Ok(())
}

fn load_catalogue(directory: &Path) -> Result<Catalogue> {
    card::load(directory).with_path(directory)
}

fn load_stamps(directory: &Path, strict: bool) -> Result<Stamps> {
    Stamps::load(directory, strict).map_err(Error::Message)
}

fn write_stamps(directory: &Path, stamps: &Stamps) -> Result<()> {
    let path = directory.join(STAMPS_FILE);
    let text = stamps.to_json().map_err(Error::Message)?;
    std::fs::write(&path, text).with_path(&path)
}

fn run_check(
    workspace: &Path,
    registry_roots: Vec<PathBuf>,
    report_dir: Option<&Path>,
) -> Result<Evaluation> {
    let evaluation = evaluate(workspace, registry_roots)?;
    if let Some(directory) = report_dir {
        let diff =
            |commit: &str, path: &str| git(workspace, &["diff", "--no-color", commit, "--", path]);
        report::write(directory, &evaluation.report, &diff).with_path(directory)?;
    }
    Ok(evaluation)
}

fn evaluate(workspace: &Path, registry_roots: Vec<PathBuf>) -> Result<Evaluation> {
    let directory = workspace.join(CATALOGUE_DIR);
    let catalogue = load_catalogue(&directory)?;
    let stamps = load_stamps(&directory, true)?;
    let mut tree = SourceTree::new(workspace, registry_roots).map_err(Error::Message)?;
    let commit = git(workspace, &["rev-parse", "HEAD"]).unwrap_or_default();
    let head_time = git(workspace, &["log", "-1", "--format=%ct", "HEAD"])
        .and_then(|time| time.parse::<u64>().ok());
    let dirty = git(
        workspace,
        &["status", "--porcelain", "--untracked-files=no"],
    )
    .is_some_and(|status| !status.is_empty());

    let mut checks = Vec::new();
    let mut cards = Vec::new();
    for card in &catalogue.cards {
        if card.status == Status::Retired {
            checks.push(None);
            cards.push(card_report(card, None, State::Retired, None));
            continue;
        }
        let card_stamp = stamps.cards.get(&card.capability);
        let check = verify::check_card(card, card_stamp, &mut tree);
        let mut state = match check.drift {
            Drift::Fresh => State::Fresh,
            Drift::Stale => State::Stale,
            Drift::Unchecked => State::Unchecked,
        };
        let mut since = None;
        if check.drift == Drift::Stale
            && let (Some(card_stamp), Some(head_time)) = (card_stamp, head_time)
        {
            since = stale_since(workspace, card, card_stamp, &check, &tree, head_time);
        }
        let mut extra_reason = None;
        if card.status == Status::Suspended {
            state = State::Suspended;
            extra_reason = Some(format!("suspended: {}", card.reason.trim()));
        } else if let Some(since) = &since
            && since.age_days > catalogue.meta.stale_grace_days
        {
            state = State::Suspended;
            extra_reason = Some(format!(
                "stale for {} days, longer than the {}-day grace period",
                since.age_days, catalogue.meta.stale_grace_days
            ));
        }
        let mut report = card_report(card, Some(&check), state, since);
        report.stamp_commit = card_stamp
            .map(|stamp| stamp.commit.clone())
            .filter(|commit| !commit.is_empty());
        report.reasons.extend(extra_reason);
        cards.push(report);
        checks.push(Some(check));
    }

    let page_current = std::fs::read_to_string(directory.join(render::PAGE_FILE))
        .is_ok_and(|text| text == render::render(&catalogue));
    let mut counts = BTreeMap::new();
    for card in &cards {
        *counts.entry(card.state.as_str().to_owned()).or_insert(0) += 1;
    }
    let report = Report {
        schema: card::SCHEMA_VERSION,
        commit,
        dirty,
        page_current,
        counts,
        findings: catalogue.findings.iter().map(FindingReport::from).collect(),
        hints: discovery_hints(&catalogue, &stamps, &tree),
        cards,
    };
    Ok(Evaluation {
        catalogue,
        checks,
        report,
    })
}

fn card_report(
    card: &Card,
    check: Option<&CardCheck>,
    state: State,
    stale_since: Option<StaleSince>,
) -> CardReport {
    CardReport {
        capability: card.capability.clone(),
        family: card.family.clone(),
        status: card.status,
        state,
        usable: state == State::Fresh && card.status == Status::Active,
        reasons: check.map(|c| c.reasons.clone()).unwrap_or_default(),
        notes: check.map(|c| c.notes.clone()).unwrap_or_default(),
        open_questions: card.open_questions.clone(),
        changed_items: check.map(|c| c.changed_items.clone()).unwrap_or_default(),
        stale_since,
        symbols: check.map(|c| c.symbols.clone()).unwrap_or_default(),
        evidence: check.map(|c| c.evidence.clone()).unwrap_or_default(),
        stamp_commit: None,
        involved: check
            .map(|c| c.involved.iter().cloned().collect())
            .unwrap_or_default(),
    }
}

/// First commit after the card's stamp that touched a repository file
/// holding a changed cited item or a lost quote (or `Cargo.lock` for
/// dependency cards), measured in commit time so the result depends only
/// on the checked history.
fn stale_since(
    workspace: &Path,
    card: &Card,
    card_stamp: &stamps::CardStamp,
    check: &CardCheck,
    tree: &SourceTree,
    head_time: u64,
) -> Option<StaleSince> {
    if card_stamp.commit.is_empty() {
        return None;
    }
    let mut paths: Vec<String> = check
        .involved
        .iter()
        .filter(|path| matches!(tree.classify(path), Reference::Repository(_)))
        .cloned()
        .collect();
    if matches!(card.source, CardSource::Dependency { .. }) {
        paths.push("Cargo.lock".to_owned());
    }
    if paths.is_empty() {
        return None;
    }
    git(
        workspace,
        &["merge-base", "--is-ancestor", &card_stamp.commit, "HEAD"],
    )?;
    let range = format!("{}..HEAD", card_stamp.commit);
    let mut args = vec!["log", "--reverse", "--format=%H %ct", range.as_str(), "--"];
    args.extend(paths.iter().map(String::as_str));
    let log = git(workspace, &args)?;
    let (commit, time) = log.lines().next()?.split_once(' ')?;
    let time: u64 = time.parse().ok()?;
    Some(StaleSince {
        commit: commit.to_owned(),
        age_days: head_time.saturating_sub(time) / SECONDS_PER_DAY,
    })
}

fn discovery_hints(catalogue: &Catalogue, stamps: &Stamps, tree: &SourceTree) -> Vec<String> {
    let baseline = &stamps.discovery;
    if baseline.registry_packages.is_empty() && baseline.workspace_packages.is_empty() {
        return Vec::new();
    }
    let covered = |name: &str| {
        catalogue
            .cards
            .iter()
            .any(|card| card.source.crate_name() == name)
    };
    let mut hints = Vec::new();
    for name in tree.locked.registry.keys() {
        if !baseline.registry_packages.contains(name) && !covered(name) {
            hints.push(format!(
                "new crates.io package `{name}` in Cargo.lock: does it offer a reusable \
                 capability?"
            ));
        }
    }
    for name in &tree.locked.workspace {
        if !baseline.workspace_packages.contains(name) && !covered(name) {
            hints.push(format!(
                "new workspace crate `{name}`: does it offer a reusable capability?"
            ));
        }
    }
    hints
}

fn stamp_cards(
    workspace: &Path,
    registry_roots: Vec<PathBuf>,
    names: &[String],
    all: bool,
    discovery: bool,
) -> Result<Vec<String>> {
    if names.is_empty() && !all && !discovery {
        return Err(Error::Message(
            "name the cards to stamp, or pass --all or --discovery".into(),
        ));
    }
    let directory = workspace.join(CATALOGUE_DIR);
    let catalogue = load_catalogue(&directory)?;
    let mut stamps = load_stamps(&directory, false)?;
    stamps.schema = card::SCHEMA_VERSION;
    let mut tree = SourceTree::new(workspace, registry_roots).map_err(Error::Message)?;
    let commit = git(workspace, &["rev-parse", "HEAD"]).unwrap_or_default();
    for name in names {
        if !catalogue.cards.iter().any(|card| &card.capability == name) {
            return Err(Error::Message(format!("no card named {name}")));
        }
    }
    let selected: Vec<&Card> = catalogue
        .cards
        .iter()
        .filter(|card| card.status != Status::Retired)
        .filter(|card| all || names.contains(&card.capability))
        .collect();
    let mut stamped = Vec::new();
    let mut refused = Vec::new();
    for card in selected {
        let mut problems: Vec<String> = catalogue
            .findings
            .iter()
            .filter(|finding| finding.card == card.capability && finding.level == Level::Error)
            .map(|finding| finding.message.clone())
            .collect();
        // Without a stamp the only expected reason is "not stamped"; anything
        // else means the card does not match the tree it would be stamped on.
        let check = verify::check_card(card, None, &mut tree);
        problems.extend(
            check
                .reasons
                .iter()
                .filter(|reason| !reason.starts_with("not stamped"))
                .cloned(),
        );
        if problems.is_empty() {
            let mut card_stamp = check.current;
            card_stamp.commit.clone_from(&commit);
            stamps.cards.insert(card.capability.clone(), card_stamp);
            stamped.push(card.capability.clone());
        } else {
            refused.push(format!(
                "{}:\n    {}",
                card.capability,
                problems.join("\n    ")
            ));
        }
    }
    let stamped_cards = stamped.len();
    if discovery {
        stamps.discovery.registry_packages = tree.locked.registry.keys().cloned().collect();
        stamps.discovery.workspace_packages = tree.locked.workspace.clone();
        stamped.push("discovery baseline".to_owned());
    }
    write_stamps(&directory, &stamps)?;
    if refused.is_empty() {
        Ok(stamped)
    } else {
        Err(Error::Message(format!(
            "stamped {} card(s); refused {} that do not match the tree:\n  {}",
            stamped_cards,
            refused.len(),
            refused.join("\n  ")
        )))
    }
}

fn print_evaluation(evaluation: &Evaluation) {
    for finding in &evaluation.catalogue.findings {
        match finding.level {
            Level::Error => log::error!("{}: {}", finding.card, finding.message),
            Level::Warning => log::warn!("{}: {}", finding.card, finding.message),
        }
    }
    for card in &evaluation.report.cards {
        if matches!(card.state, State::Fresh | State::Retired) {
            continue;
        }
        println!("{:<10} {}", card.state.as_str(), card.capability);
        for line in card.reasons.iter().chain(&card.notes) {
            println!("           {line}");
        }
    }
    for hint in &evaluation.report.hints {
        println!("hint       {hint}");
    }
    if !evaluation.report.page_current {
        println!(
            "outdated   {CATALOGUE_DIR}/{}: run `make capabilities`",
            render::PAGE_FILE
        );
    }
    let counts: Vec<String> = evaluation
        .report
        .counts
        .iter()
        .map(|(state, count)| format!("{count} {state}"))
        .collect();
    println!(
        "capability catalogue: {} card(s): {}",
        evaluation.catalogue.cards.len(),
        counts.join(", ")
    );
}

fn verdict(evaluation: &Evaluation) -> Result<()> {
    let attention = evaluation.attention_count();
    let mut problems = Vec::new();
    if evaluation.catalogue.has_errors() {
        problems.push("schema errors".to_owned());
    }
    if attention > 0 {
        problems.push(format!("{attention} stale or unchecked card(s)"));
    }
    if !evaluation.report.page_current {
        problems.push(format!("{} is outdated", render::PAGE_FILE));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Error::Message(format!(
            "capability catalogue: {}; see docs/capabilities/README.md",
            problems.join(", ")
        )))
    }
}

/// Runs `git -C <root> <args>`; `None` when Git fails or is missing.
fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned()
    })
}

#[cfg(test)]
mod tests;
