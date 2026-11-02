// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! End-to-end checks on a miniature repository with one workspace card and
//! one dependency card.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use tempfile::TempDir;

use super::{CATALOGUE_DIR, Evaluation, State, render, run_check, stamp_cards, verify::Drift};

const LIB_RS: &str = r#"//! Fixed-capacity ring.
#![no_std]

/// A ring of bytes.
pub struct Ring {
    pub capacity: usize,
    head: usize,
}

impl Ring {
    /// Creates an empty ring; never allocates.
    pub const fn new(capacity: usize) -> Self {
        Self { capacity, head: 0 }
    }

    /// Appends a byte; returns `Err(byte)` when full.
    pub fn push(&mut self, byte: u8) -> Result<(), u8> {
        let _ = byte;
        self.head += 1;
        Ok(())
    }
}
"#;

const RING_CARD: &str = r##"capability: ring
title: Byte ring (kring::Ring)
family: container.ring
summary: Fixed-capacity byte ring.
source: {kind: workspace, crate: kring, path: util/kring}
symbols:
  - {fqn: kring::Ring, kind: struct, path: util/kring/src/lib.rs, role: primary}
  - {fqn: kring::Ring::new, kind: fn, path: util/kring/src/lib.rs, role: constructor}
  - {fqn: kring::Ring::push, kind: fn, path: util/kring/src/lib.rs, role: operation}
operations:
  - name: push
    symbols: [kring::Ring::push]
    effect: append one byte
    evidence:
      - {path: util/kring/src/lib.rs, item: kring::Ring::push, quote: "returns `Err(byte)` when full"}
contract:
  no_std: {value: "yes", evidence: [{path: util/kring/src/lib.rs, quote: "#![no_std]"}]}
  allocation:
    value: new never allocates
    evidence: [{path: util/kring/src/lib.rs, item: kring::Ring::new, quote: "never allocates"}]
  blocking: {value: unknown, evidence: []}
  irq_nmi: {value: unknown, evidence: []}
  ownership_lifetime: {value: unknown, evidence: []}
  errors_bounds: {value: unknown, evidence: []}
  concurrency: {value: unknown, evidence: []}
layers:
  depends_on: []
  used_by: []
  allowed_layers: unknown
  evidence: [{path: util/kring/Cargo.toml, quote: "name = \"kring\""}]
aliases: {en: [ring buffer], zh: [环形缓冲区]}
exclusions: [not for MPMC use]
"##;

const DEP_LIB_RS: &str = "//! Lock-free SPSC FIFO ring buffer\npub struct HeapRb;\n";

const DEP_CARD: &str = r#"capability: dep_ring
title: SPSC ring (ringbuf::HeapRb)
family: container.ring
summary: SPSC ring buffer.
source: {kind: dependency, crate: ringbuf, version: 0.4.8}
symbols:
  - {fqn: ringbuf::HeapRb, kind: struct, path: src/lib.rs, role: primary}
contract:
  no_std: {value: unknown, evidence: []}
  allocation: {value: unknown, evidence: []}
  blocking: {value: unknown, evidence: []}
  irq_nmi: {value: unknown, evidence: []}
  ownership_lifetime: {value: unknown, evidence: []}
  errors_bounds: {value: unknown, evidence: []}
  concurrency:
    value: SPSC
    evidence: [{path: ringbuf-0.4.8/src/lib.rs, quote: "Lock-free SPSC FIFO ring buffer"}]
layers: {depends_on: [], used_by: [], allowed_layers: unknown, evidence: []}
aliases: {en: [ring buffer], zh: [环形缓冲区]}
"#;

const CATALOGUE: &str = "schema: 2
baseline: {commit: test}
families: [container.ring]
absent: []
maintenance: {stale_grace_days: 14, model_draft: disabled}
";

const LOCK: &str = r#"version = 4
[[package]]
name = "kring"
version = "0.1.0"
[[package]]
name = "ringbuf"
version = "0.4.8"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;

struct Fixture {
    _dir: TempDir,
    workspace: PathBuf,
    registry: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("repo");
        let registry = dir.path().join("registry");
        let fixture = Self {
            _dir: dir,
            workspace,
            registry,
        };
        fixture.write("Cargo.lock", LOCK);
        fixture.write("util/kring/Cargo.toml", "[package]\nname = \"kring\"\n");
        fixture.write("util/kring/src/lib.rs", LIB_RS);
        fixture.write(&format!("{CATALOGUE_DIR}/catalogue.yaml"), CATALOGUE);
        fixture.write(&format!("{CATALOGUE_DIR}/cards/ring.yaml"), RING_CARD);
        fixture.write(&format!("{CATALOGUE_DIR}/cards/dep_ring.yaml"), DEP_CARD);
        let dep = fixture.registry.join("ringbuf-0.4.8/src");
        fs::create_dir_all(&dep).unwrap();
        fs::write(dep.join("lib.rs"), DEP_LIB_RS).unwrap();
        fixture
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.workspace.join(relative)
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn read(&self, relative: &str) -> String {
        fs::read_to_string(self.path(relative)).unwrap()
    }

    fn edit(&self, relative: &str, from: &str, to: &str) {
        let text = self.read(relative);
        assert!(text.contains(from), "{from:?} not in {relative}");
        self.write(relative, &text.replacen(from, to, 1));
    }

    fn check(&self, report: Option<&Path>) -> Evaluation {
        run_check(&self.workspace, vec![self.registry.clone()], report).unwrap()
    }

    fn render(&self) {
        let directory = self.path(CATALOGUE_DIR);
        let catalogue = super::load_catalogue(&directory).unwrap();
        fs::write(
            directory.join(render::PAGE_FILE),
            render::render(&catalogue),
        )
        .unwrap();
    }

    fn stamp_all(&self) {
        stamp_cards(
            &self.workspace,
            vec![self.registry.clone()],
            &[],
            true,
            true,
        )
        .unwrap();
    }

    /// Stamped and rendered: every card fresh, page current.
    fn reviewed() -> Self {
        let fixture = Self::new();
        fixture.stamp_all();
        fixture.render();
        fixture
    }
}

fn state(evaluation: &Evaluation, capability: &str) -> State {
    evaluation
        .report
        .cards
        .iter()
        .find(|card| card.capability == capability)
        .unwrap()
        .state
}

fn reasons(evaluation: &Evaluation, capability: &str) -> String {
    let card = evaluation
        .report
        .cards
        .iter()
        .find(|card| card.capability == capability)
        .unwrap();
    card.reasons.join("\n")
}

fn card<'a>(evaluation: &'a Evaluation, capability: &str) -> &'a super::report::CardReport {
    evaluation
        .report
        .cards
        .iter()
        .find(|card| card.capability == capability)
        .unwrap()
}

fn assert_fresh(evaluation: &Evaluation) {
    for card in &evaluation.report.cards {
        assert_eq!(
            card.state,
            State::Fresh,
            "{}: {}",
            card.capability,
            card.reasons.join("\n")
        );
    }
}

#[test]
fn unstamped_cards_are_stale_until_stamped() {
    let fixture = Fixture::new();
    let evaluation = fixture.check(None);
    assert!(
        evaluation.catalogue.findings.is_empty(),
        "{:?}",
        evaluation.catalogue.findings
    );
    assert_eq!(state(&evaluation, "ring"), State::Stale);
    assert!(reasons(&evaluation, "ring").contains("not stamped"));
    assert!(!evaluation.report.page_current);
    assert_eq!(evaluation.attention_count(), 2);

    fixture.stamp_all();
    fixture.render();
    let evaluation = fixture.check(None);
    assert_fresh(&evaluation);
    assert!(evaluation.report.page_current);
    assert!(super::verdict(&evaluation).is_ok());
    let page = fixture.read(&format!("{CATALOGUE_DIR}/{}", render::PAGE_FILE));
    assert!(page.contains("### ring"));
    assert!(page.contains("[`kring::Ring`](../../util/kring/src/lib.rs)"));
    assert!(page.contains("allocation: new never allocates"));
}

#[test]
fn line_numbers_are_reported_but_never_written_to_cards() {
    let fixture = Fixture::reviewed();
    let report_dir = fixture.path("out");
    let evaluation = fixture.check(Some(&report_dir));
    let ring = card(&evaluation, "ring");
    let push = ring
        .symbols
        .iter()
        .find(|s| s.fqn == "kring::Ring::push")
        .unwrap();
    assert_eq!(
        (push.path.as_str(), push.line),
        ("util/kring/src/lib.rs", 17)
    );
    let quote = ring
        .evidence
        .iter()
        .find(|e| e.field == "operations.push")
        .unwrap();
    assert_eq!((quote.line, quote.last_line), (16, 16));
    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(report_dir.join("report.json")).unwrap()).unwrap();
    assert_eq!(report["schema"], 2);
    assert_eq!(report["cards"][1]["evidence"][0]["line"], 16);
    assert_eq!(report["cards"][1]["status"], "active");
    assert!(report["cards"][1].get("relocations").is_none());
}

#[test]
fn moved_code_stays_fresh() {
    let fixture = Fixture::reviewed();
    let lib = "util/kring/src/lib.rs";
    let card_before = fixture.read(&format!("{CATALOGUE_DIR}/cards/ring.yaml"));
    // Lines move: code inserted above every cited item.
    fixture.edit(
        lib,
        "#![no_std]\n",
        "#![no_std]\n\nuse core::fmt;\n\nfn helper() {}\n",
    );
    let evaluation = fixture.check(None);
    assert_fresh(&evaluation);
    assert_eq!(card(&evaluation, "ring").symbols[0].line, 9);
    assert!(super::verdict(&evaluation).is_ok());

    // An item moves to another file of the crate, quote and all.
    fixture.edit(
        lib,
        "    /// Appends a byte; returns `Err(byte)` when full.\n    pub fn push(&mut self, byte: \
         u8) -> Result<(), u8> {\n        let _ = byte;\n        self.head += 1;\n        \
         Ok(())\n    }\n",
        "",
    );
    fixture.write(
        "util/kring/src/push.rs",
        "impl super::Ring {\n    /// Appends a byte; returns `Err(byte)` when full.\n    pub fn \
         push(&mut self, byte: u8) -> Result<(), u8> {\n        let _ = byte;\n        self.head \
         += 1;\n        Ok(())\n    }\n}\n",
    );
    let evaluation = fixture.check(None);
    assert_fresh(&evaluation);
    let ring = card(&evaluation, "ring");
    assert!(ring.usable);
    let push = ring
        .symbols
        .iter()
        .find(|s| s.fqn == "kring::Ring::push")
        .unwrap();
    assert_eq!(
        (push.path.as_str(), push.line),
        ("util/kring/src/push.rs", 3)
    );
    assert!(
        ring.notes
            .iter()
            .any(|n| n.contains("moved from util/kring/src/lib.rs")),
        "{:?}",
        ring.notes
    );
    // The card is never rewritten.
    assert_eq!(
        fixture.read(&format!("{CATALOGUE_DIR}/cards/ring.yaml")),
        card_before
    );
}

#[test]
fn unrelated_edits_in_a_cited_file_stay_fresh() {
    let fixture = Fixture::reviewed();
    fixture.edit(
        "util/kring/src/lib.rs",
        "impl Ring {",
        "pub fn unrelated(x: u32) -> u32 {\n    x + 1\n}\n\nimpl Ring {",
    );
    fixture.edit(
        "util/kring/Cargo.toml",
        "[package]\n",
        "[package]\nedition = \"2024\"\n",
    );
    // Comments, formatting and doc comments of cited items are not substance.
    fixture.edit(
        "util/kring/src/lib.rs",
        "self.head += 1;",
        "// bump\n        self.head\n            += 1;",
    );
    fixture.edit(
        "util/kring/src/lib.rs",
        "/// A ring of bytes.",
        "/// A ring of bytes, reworded.",
    );
    assert_fresh(&fixture.check(None));
}

#[test]
fn changed_bodies_and_signatures_make_cards_stale() {
    let fixture = Fixture::reviewed();
    fixture.edit(
        "util/kring/src/lib.rs",
        "self.head += 1;",
        "self.head += 2;",
    );
    let report_dir = fixture.path("out");
    let evaluation = fixture.check(Some(&report_dir));
    assert_eq!(state(&evaluation, "ring"), State::Stale);
    assert!(reasons(&evaluation, "ring").contains("symbol kring::Ring::push: item changed"));
    assert!(reasons(&evaluation, "ring").contains("kring::Ring::push: item changed"));
    let ring = card(&evaluation, "ring");
    assert!(!ring.usable);
    assert!(ring.changed_items.iter().any(|c| c.change == "body"));
    assert!(report_dir.join("proposals/ring.md").is_file());

    let fixture = Fixture::reviewed();
    fixture.edit(
        "util/kring/src/lib.rs",
        "pub const fn new(capacity: usize)",
        "pub const fn new(capacity: u32)",
    );
    let evaluation = fixture.check(None);
    assert!(
        reasons(&evaluation, "ring")
            .contains("symbol kring::Ring::new: signature or visibility changed")
    );
    assert!(super::verdict(&evaluation).is_err());
}

#[test]
fn quotes_must_stay_unique() {
    let fixture = Fixture::reviewed();
    // A second copy of the quote inside the same item.
    fixture.edit(
        "util/kring/src/lib.rs",
        "/// Appends a byte; returns `Err(byte)` when full.",
        "/// Appends a byte; returns `Err(byte)` when full.\n    /// (It returns `Err(byte)` when \
         full.)",
    );
    let evaluation = fixture.check(None);
    assert_eq!(state(&evaluation, "ring"), State::Stale);
    assert!(reasons(&evaluation, "ring").contains("appears 2 times in the item; extend the quote"));

    // A file-level quote that becomes ambiguous.
    let fixture = Fixture::reviewed();
    fixture.edit(
        "util/kring/src/lib.rs",
        "#![no_std]\n",
        "#![no_std]\n// #![no_std]\n",
    );
    let evaluation = fixture.check(None);
    assert!(reasons(&evaluation, "ring").contains("appears 2 times in util/kring/src/lib.rs"));

    // Stamping refuses a card whose quote is not unique as written.
    let error = stamp_cards(
        &fixture.workspace,
        vec![fixture.registry.clone()],
        &["ring".to_owned()],
        false,
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("appears 2 times"), "{error}");
}

#[test]
fn quotes_inside_items_must_name_the_item() {
    let fixture = Fixture::new();
    fixture.edit(
        &format!("{CATALOGUE_DIR}/cards/ring.yaml"),
        "{path: util/kring/src/lib.rs, item: kring::Ring::new, quote: \"never allocates\"}",
        "{path: util/kring/src/lib.rs, quote: \"never allocates\"}",
    );
    let error = stamp_cards(
        &fixture.workspace,
        vec![fixture.registry.clone()],
        &["ring".to_owned()],
        false,
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("lies inside item Ring::new; name it"),
        "{error}"
    );
}

#[test]
fn removed_quotes_and_symbols_are_stale_with_a_proposal() {
    let fixture = Fixture::reviewed();
    fixture.edit(
        "util/kring/src/lib.rs",
        "never allocates",
        "allocates a buffer",
    );
    fixture.edit(
        "util/kring/src/lib.rs",
        "pub struct Ring {",
        "pub struct ByteRing {",
    );
    let report_dir = fixture.path("out");
    let evaluation = fixture.check(Some(&report_dir));
    let reasons = reasons(&evaluation, "ring");
    assert!(
        reasons.contains("contract.allocation: item kring::Ring::new: quote not found"),
        "{reasons}"
    );
    assert!(
        reasons.contains("symbol kring::Ring (struct) not found"),
        "{reasons}"
    );
    let notes = &card(&evaluation, "ring").notes;
    assert!(
        notes
            .iter()
            .any(|n| n.contains("possibly renamed to ByteRing")),
        "{notes:?}"
    );

    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(report_dir.join("report.json")).unwrap()).unwrap();
    assert_eq!(report["cards"][1]["state"], "stale");
    assert_eq!(report["cards"][1]["usable"], false);
    let proposal = fs::read_to_string(report_dir.join("proposals/ring.md")).unwrap();
    assert!(proposal.contains("quote not found") && proposal.contains("model_draft: disabled"));
}

#[test]
fn old_schema_is_refused() {
    let fixture = Fixture::reviewed();
    fixture.edit(
        &format!("{CATALOGUE_DIR}/catalogue.yaml"),
        "schema: 2",
        "schema: 1",
    );
    fixture.edit(
        &format!("{CATALOGUE_DIR}/cards/ring.yaml"),
        "{path: util/kring/src/lib.rs, quote: \"#![no_std]\"}",
        "{at: \"util/kring/src/lib.rs:2\", quote: \"#![no_std]\"}",
    );
    let evaluation = fixture.check(None);
    let messages: Vec<String> = evaluation
        .catalogue
        .findings
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(
        messages
            .iter()
            .any(|m| m.contains("catalogue.yaml is schema 1")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("ring: schema 1 card")),
        "{messages:?}"
    );
    assert!(super::verdict(&evaluation).is_err());

    // Schema 1 stamps are refused by `check` and dropped by `stamp`.
    let fixture = Fixture::reviewed();
    let stamps = fixture.path(&format!("{CATALOGUE_DIR}/stamps.json"));
    let text = fs::read_to_string(&stamps)
        .unwrap()
        .replacen("\"schema\": 2", "\"schema\": 1", 1);
    fs::write(&stamps, text).unwrap();
    let error = run_check(&fixture.workspace, vec![fixture.registry.clone()], None)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("schema 1, expected 2"), "{error}");
    fixture.stamp_all();
    assert_fresh(&fixture.check(None));
}

#[test]
fn dependency_cards_follow_cargo_lock_and_the_registry() {
    let fixture = Fixture::reviewed();
    fixture.edit("Cargo.lock", "version = \"0.4.8\"", "version = \"0.4.9\"");
    let evaluation = fixture.check(None);
    assert_eq!(state(&evaluation, "dep_ring"), State::Stale);
    assert!(reasons(&evaluation, "dep_ring").contains("no longer in Cargo.lock (locked: 0.4.9)"));

    let fixture = Fixture::reviewed();
    fs::remove_dir_all(fixture.registry.join("ringbuf-0.4.8")).unwrap();
    let evaluation = fixture.check(None);
    assert_eq!(state(&evaluation, "dep_ring"), State::Unchecked);
    assert!(reasons(&evaluation, "dep_ring").contains("cargo fetch --locked"));
}

#[test]
fn stamp_refuses_cards_that_do_not_match_the_tree() {
    let fixture = Fixture::new();
    fixture.edit(
        &format!("{CATALOGUE_DIR}/cards/ring.yaml"),
        "never allocates\"",
        "always allocates\"",
    );
    let error = stamp_cards(
        &fixture.workspace,
        vec![fixture.registry.clone()],
        &["ring".to_owned(), "dep_ring".to_owned()],
        false,
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("stamped 1 card(s); refused 1"), "{error}");
    assert!(error.contains("quote not found"), "{error}");
    let evaluation = fixture.check(None);
    assert_eq!(state(&evaluation, "dep_ring"), State::Fresh);
    assert!(
        stamp_cards(
            &fixture.workspace,
            vec![],
            &["nope".to_owned()],
            false,
            false
        )
        .is_err()
    );
}

#[test]
fn status_takes_cards_out_of_use() {
    let fixture = Fixture::reviewed();
    let card_file = format!("{CATALOGUE_DIR}/cards/ring.yaml");
    fixture.write(
        &card_file,
        &format!("{RING_CARD}status: suspended\nreason: reported wrong in review\n"),
    );
    fixture.edit(
        "util/kring/src/lib.rs",
        "self.head += 1;",
        "self.head += 2;",
    );
    fixture.render();
    let evaluation = fixture.check(None);
    assert_eq!(state(&evaluation, "ring"), State::Suspended);
    assert_eq!(evaluation.attention_count(), 0);
    let page = fixture.read(&format!("{CATALOGUE_DIR}/{}", render::PAGE_FILE));
    assert!(page.contains("**Suspended:** reported wrong in review"));

    fixture.write(
        &card_file,
        &format!("{RING_CARD}status: retired\nreason: crate removed\n"),
    );
    fixture.render();
    let evaluation = fixture.check(None);
    assert_eq!(state(&evaluation, "ring"), State::Retired);
    assert!(evaluation.checks[1].is_none());
    let page = fixture.read(&format!("{CATALOGUE_DIR}/{}", render::PAGE_FILE));
    assert!(!page.contains("### ring\n"));
}

#[test]
fn schema_problems_and_model_drafting_fail_the_check() {
    let fixture = Fixture::reviewed();
    fixture.edit(
        &format!("{CATALOGUE_DIR}/catalogue.yaml"),
        "model_draft: disabled",
        "model_draft: enabled",
    );
    let evaluation = fixture.check(None);
    assert!(evaluation.catalogue.has_errors());
    assert!(
        super::verdict(&evaluation)
            .unwrap_err()
            .to_string()
            .contains("schema errors")
    );
}

#[test]
fn new_lockfile_packages_become_discovery_hints() {
    let fixture = Fixture::reviewed();
    assert!(fixture.check(None).report.hints.is_empty());
    fixture.write(
        "Cargo.lock",
        &format!(
            "{LOCK}[[package]]\nname = \"idalloc\"\nversion = \"1.0.0\"\nsource = \"registry+x\"\n"
        ),
    );
    let report_dir = fixture.path("out");
    let evaluation = fixture.check(Some(&report_dir));
    assert_eq!(evaluation.report.hints.len(), 1);
    assert!(evaluation.report.hints[0].contains("`idalloc`"));
    assert!(report_dir.join("proposals/discovery.md").is_file());
    // Hints are advisory: they do not fail the check.
    assert!(super::verdict(&evaluation).is_ok());
}

fn git(workspace: &Path, args: &[&str], date: Option<&str>) -> bool {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(workspace)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args);
    if let Some(date) = date {
        command
            .env("GIT_COMMITTER_DATE", date)
            .env("GIT_AUTHOR_DATE", date);
    }
    command.output().is_ok_and(|output| output.status.success())
}

#[test]
fn stale_cards_beyond_the_grace_period_are_suspended() {
    let fixture = Fixture::new();
    let workspace = &fixture.workspace;
    if !git(workspace, &["init", "-q"], None) {
        return; // Git is not available on this host.
    }
    assert!(git(workspace, &["add", "-A"], None));
    assert!(git(
        workspace,
        &["commit", "-qm", "base"],
        Some("2026-01-01T00:00:00Z")
    ));
    fixture.stamp_all();
    fixture.check(None);
    assert!(git(workspace, &["add", "-A"], None));
    assert!(git(
        workspace,
        &["commit", "-qm", "stamp"],
        Some("2026-01-02T00:00:00Z")
    ));

    fixture.edit(
        "util/kring/src/lib.rs",
        "self.head += 1;",
        "self.head += 2;",
    );
    assert!(git(
        workspace,
        &["commit", "-qam", "change"],
        Some("2026-01-03T00:00:00Z")
    ));
    let evaluation = fixture.check(None);
    assert_eq!(state(&evaluation, "ring"), State::Stale);
    let since = evaluation.report.cards[1].stale_since.as_ref().unwrap();
    assert_eq!(since.age_days, 0);

    fixture.write("README", "later\n");
    assert!(git(workspace, &["add", "README"], None));
    assert!(git(
        workspace,
        &["commit", "-qm", "later"],
        Some("2026-02-01T00:00:00Z")
    ));
    let evaluation = fixture.check(None);
    assert_eq!(state(&evaluation, "ring"), State::Suspended);
    assert!(reasons(&evaluation, "ring").contains("stale for 29 days"));
    assert_eq!(evaluation.checks[1].as_ref().unwrap().drift, Drift::Stale);
    assert_eq!(evaluation.attention_count(), 1);
}

#[test]
fn needs_owner_cards_are_checked_but_not_usable() {
    let fixture = Fixture::reviewed();
    let report_dir = fixture.path("out");
    fixture.write(
        &format!("{CATALOGUE_DIR}/cards/ring.yaml"),
        &format!(
            "{RING_CARD}status: needs-owner\nopen_questions:\n- field: symbols\n  question: \
             Should Ring be public?\n  evidence: [{{path: util/kring/src/lib.rs, item: \
             kring::Ring, quote: \"pub struct Ring\"}}]\n"
        ),
    );
    fixture.stamp_all();
    fixture.render();
    let evaluation = fixture.check(Some(&report_dir));
    assert!(
        evaluation.catalogue.findings.is_empty(),
        "{:?}",
        evaluation.catalogue.findings
    );
    assert_eq!(state(&evaluation, "ring"), State::Fresh);
    assert!(!card(&evaluation, "ring").usable);
    assert!(card(&evaluation, "dep_ring").usable);
    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(report_dir.join("report.json")).unwrap()).unwrap();
    assert_eq!(report["cards"][1]["status"], "needs-owner");
    assert_eq!(
        report["cards"][1]["open_questions"][0]["question"],
        "Should Ring be public?"
    );
    let page = fixture.read(&format!("{CATALOGUE_DIR}/{}", render::PAGE_FILE));
    assert!(page.contains("**Needs an owner decision.**"));
    assert!(page.contains("> - `symbols`: Should Ring be public?"));

    // Open-question evidence is checked like any other.
    fixture.edit(
        "util/kring/src/lib.rs",
        "pub struct Ring {",
        "pub struct Ring {\n    pub id: u8,",
    );
    let evaluation = fixture.check(None);
    assert_eq!(state(&evaluation, "ring"), State::Stale);
    assert_eq!(evaluation.attention_count(), 1);
}
