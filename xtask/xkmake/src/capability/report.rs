// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Machine-readable report and maintenance proposals of one check run.
//!
//! `report.json` is the contract consumers such as xreview read; the
//! proposals are Markdown for the humans who approve card changes.

use std::{fmt::Write, path::Path};

use serde::Serialize;

use super::{
    State,
    card::{Finding, OpenQuestion, Status},
    verify::{ChangedItem, QuoteLocation, SymbolLocation},
};

/// Lines of `git diff` kept per changed file in a proposal.
const MAX_DIFF_LINES: usize = 200;

#[derive(Debug, Serialize)]
pub(crate) struct StaleSince {
    pub(crate) commit: String,
    pub(crate) age_days: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct CardReport {
    pub(crate) capability: String,
    pub(crate) family: String,
    /// `status` as written in the card.
    pub(crate) status: Status,
    pub(crate) state: State,
    /// Whether consumers may suggest the card: status active and state
    /// fresh.
    pub(crate) usable: bool,
    pub(crate) reasons: Vec<String>,
    pub(crate) notes: Vec<String>,
    /// Questions an owner must answer (needs-owner cards only).
    pub(crate) open_questions: Vec<OpenQuestion>,
    pub(crate) changed_items: Vec<ChangedItem>,
    pub(crate) stale_since: Option<StaleSince>,
    /// Where each symbol and quote was found in the checked tree. Line
    /// numbers exist only here, never in the cards.
    pub(crate) symbols: Vec<SymbolLocation>,
    pub(crate) evidence: Vec<QuoteLocation>,
    /// Stamp commit, for diffs in proposals.
    #[serde(skip)]
    pub(crate) stamp_commit: Option<String>,
    /// Source references involved in a change, for proposals.
    #[serde(skip)]
    pub(crate) involved: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct FindingReport {
    pub(crate) card: String,
    pub(crate) level: &'static str,
    pub(crate) message: String,
}

impl From<&Finding> for FindingReport {
    fn from(finding: &Finding) -> Self {
        Self {
            card: finding.card.clone(),
            level: match finding.level {
                super::card::Level::Warning => "warning",
                super::card::Level::Error => "error",
            },
            message: finding.message.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct Report {
    pub(crate) schema: i64,
    /// `HEAD` of the checked tree; empty outside a Git checkout.
    pub(crate) commit: String,
    /// Whether the checked tree had uncommitted changes.
    pub(crate) dirty: bool,
    pub(crate) page_current: bool,
    pub(crate) counts: std::collections::BTreeMap<String, usize>,
    pub(crate) findings: Vec<FindingReport>,
    pub(crate) hints: Vec<String>,
    pub(crate) cards: Vec<CardReport>,
}

/// Writes `report.json` and `proposals/*.md` under `directory`. `diff`
/// returns `git diff <commit> -- <path>`.
pub(crate) fn write(
    directory: &Path,
    report: &Report,
    diff: &dyn Fn(&str, &str) -> Option<String>,
) -> std::io::Result<()> {
    let proposals = directory.join("proposals");
    std::fs::create_dir_all(&proposals)?;
    let mut json = serde_json::to_string_pretty(report).map_err(std::io::Error::other)?;
    json.push('\n');
    std::fs::write(directory.join("report.json"), json)?;
    for card in &report.cards {
        if matches!(card.state, State::Stale | State::Suspended) && !card.reasons.is_empty() {
            std::fs::write(
                proposals.join(format!("{}.md", card.capability)),
                proposal(report, card, diff),
            )?;
        }
    }
    if !report.hints.is_empty() {
        let mut text = String::from(
            "# Maintenance proposal: consider new cards\n\nDiscovery hints from the deterministic \
             check. Each needs a human decision:\ndraft a card (see README.md \"Adding a card\"), \
             or record that no card is\nneeded by re-baselining with `cargo xkmake capability \
             stamp --discovery`.\n\n",
        );
        for hint in &report.hints {
            let _ = writeln!(text, "- {hint}");
        }
        std::fs::write(proposals.join("discovery.md"), text)?;
    }
    Ok(())
}

fn proposal(
    report: &Report,
    card: &CardReport,
    diff: &dyn Fn(&str, &str) -> Option<String>,
) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "# Maintenance proposal: {}\n", card.capability);
    let _ = writeln!(text, "- State: {}", card.state.as_str());
    if let Some(since) = &card.stale_since {
        let _ = writeln!(
            text,
            "- Stale since: {} ({} days before the checked commit)",
            since.commit, since.age_days
        );
    }
    let _ = writeln!(
        text,
        "- Card: `docs/capabilities/cards/{}.yaml`",
        card.capability
    );
    let _ = writeln!(text, "- Checked at: {}", display_commit(report));
    text.push_str("\n## Why\n\n");
    for reason in &card.reasons {
        let _ = writeln!(text, "- {reason}");
    }
    for note in &card.notes {
        let _ = writeln!(text, "- note: {note}");
    }
    if !card.changed_items.is_empty() {
        text.push_str("\n## Changed cited items\n\n");
        for item in &card.changed_items {
            let _ = writeln!(
                text,
                "- `{}`: {} (stamped in {}, now {})",
                item.item,
                item.change,
                item.stamped_path.as_deref().unwrap_or("(not stamped)"),
                item.path.as_deref().unwrap_or("(not found)")
            );
        }
    }
    if let Some(commit) = &card.stamp_commit {
        for path in &card.involved {
            let Some(patch) = diff(commit, path).filter(|patch| !patch.is_empty()) else {
                continue;
            };
            let lines: Vec<&str> = patch.lines().collect();
            let _ = writeln!(text, "\n### `{path}` since {commit}\n\n```diff");
            for line in lines.iter().take(MAX_DIFF_LINES) {
                let _ = writeln!(text, "{line}");
            }
            if lines.len() > MAX_DIFF_LINES {
                let _ = writeln!(text, "... {} more lines", lines.len() - MAX_DIFF_LINES);
            }
            text.push_str("```\n");
        }
    }
    text.push_str(
        "\n## Model draft\n\nDisabled (`maintenance.model_draft: disabled` in `catalogue.yaml`). \
         Enabling a\ndrafting step spends model quota and needs explicit cost approval \
         first.\n\n## To resolve\n\n1. Re-read the cited code. Update symbols, evidence and \
         contract values; a value\n   the code no longer supports becomes `unknown` with no \
         evidence.\n2. If the capability is gone or the card is wrong, set `status` to \
         `retired`\n   or `suspended` with a `reason` instead.\n3. Run `make check_capabilities`, \
         then\n   `cargo xkmake capability stamp <capability>` to record the reviewed state.\n4. \
         Open a PR with the card, `stamps.json` and `helpers.md`; a catalogue\n   maintainer \
         approves it (contract changes also need a maintainer of the\n   cited crate).\n",
    );
    text
}

fn display_commit(report: &Report) -> String {
    match (report.commit.is_empty(), report.dirty) {
        (true, _) => "(not a Git checkout)".to_owned(),
        (false, true) => format!("{} with uncommitted changes", report.commit),
        (false, false) => report.commit.clone(),
    }
}
