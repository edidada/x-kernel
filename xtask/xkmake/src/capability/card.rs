// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Capability card and catalogue schema (schema 2).
//!
//! The format is shared with xreview: a field this module does not know is
//! ignored by both readers, so optional additions stay compatible. The
//! normative description is `docs/capabilities/README.md`.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use yaml_rust2::{Yaml, YamlLoader};

/// Catalogue and card schema version understood by this checker.
pub(crate) const SCHEMA_VERSION: i64 = 2;
/// Contract value meaning "not established by evidence".
pub(crate) const UNKNOWN: &str = "unknown";
/// Contract fields every card must state, each with evidence or `unknown`.
pub(crate) const CONTRACT_FIELDS: &[&str] = &[
    "no_std",
    "allocation",
    "blocking",
    "irq_nmi",
    "ownership_lifetime",
    "errors_bounds",
    "concurrency",
];
const SYMBOL_KINDS: &[&str] = &[
    "struct", "enum", "union", "trait", "fn", "macro", "type", "mod", "const", "static",
];
const SYMBOL_ROLES: &[&str] = &["primary", "constructor", "operation", "helper", "reexport"];
const REQUIRED_FIELDS: &[&str] = &[
    "capability",
    "title",
    "family",
    "summary",
    "source",
    "symbols",
    "contract",
    "aliases",
];
/// The only accepted value of `maintenance.model_draft` (see README).
pub(crate) const MODEL_DRAFT_DISABLED: &str = "disabled";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) enum Level {
    Warning,
    Error,
}

/// One schema or consistency finding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Finding {
    pub(crate) card: String,
    pub(crate) level: Level,
    pub(crate) message: String,
}

impl Finding {
    fn error(card: &str, message: impl Into<String>) -> Self {
        Self {
            card: card.to_owned(),
            level: Level::Error,
            message: message.into(),
        }
    }

    fn warning(card: &str, message: impl Into<String>) -> Self {
        Self {
            card: card.to_owned(),
            level: Level::Warning,
            message: message.into(),
        }
    }
}

impl fmt::Display for Finding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let level = match self.level {
            Level::Warning => "warning",
            Level::Error => "error",
        };
        write!(formatter, "{level}: {}: {}", self.card, self.message)
    }
}

/// A verbatim quote cited by a card: `path` + optional owning `item` +
/// `quote`. Never a line number; lines are resolved at check time.
#[derive(Clone, Debug)]
pub(crate) struct Evidence {
    /// Card field the evidence supports, e.g. `contract.allocation`.
    pub(crate) field: String,
    /// Source reference: repository path or `<crate>-<version>/<path>`.
    pub(crate) path: String,
    /// Path of the item that owns the quote (`kirq::manager::register`),
    /// or `None` for a quote outside any item.
    pub(crate) item: Option<String>,
    pub(crate) quote: String,
}

/// Location of a counter-example: a file and optionally an item in it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Place {
    pub(crate) path: String,
    pub(crate) item: Option<String>,
}

impl fmt::Display for Place {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.item {
            Some(item) => write!(formatter, "{} ({item})", self.path),
            None => formatter.write_str(&self.path),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CardSource {
    /// A crate of this repository at `path`.
    Workspace { crate_name: String, path: String },
    /// A crates.io package locked in `Cargo.lock`.
    Dependency { crate_name: String, version: String },
}

impl CardSource {
    pub(crate) fn crate_name(&self) -> &str {
        match self {
            Self::Workspace { crate_name, .. } | Self::Dependency { crate_name, .. } => crate_name,
        }
    }

    /// Prefix of source references: `<crate>-<version>/` for dependencies,
    /// empty for repository paths.
    pub(crate) fn reference_prefix(&self) -> String {
        match self {
            Self::Workspace { .. } => String::new(),
            Self::Dependency {
                crate_name,
                version,
            } => format!("{crate_name}-{version}/"),
        }
    }

    /// Reference of the crate root directory, used when a cited item moved.
    pub(crate) fn root_reference(&self) -> String {
        match self {
            Self::Workspace { path, .. } => format!("{}/", path.trim_end_matches('/')),
            Self::Dependency { .. } => self.reference_prefix(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Symbol {
    pub(crate) fqn: String,
    pub(crate) kind: String,
    /// Repository path, or path inside the dependency crate.
    pub(crate) path: String,
    pub(crate) role: String,
}

#[derive(Clone, Debug)]
pub(crate) struct Operation {
    pub(crate) name: String,
    pub(crate) effect: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ContractField {
    pub(crate) name: String,
    pub(crate) value: String,
}

/// Card status, set by people; the checker derives fresh / stale.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Status {
    /// In use.
    #[default]
    Active,
    /// Kept as a record but not suggested until the open questions are
    /// answered by an owner.
    NeedsOwner,
    /// Reported wrong; not used until fixed.
    Suspended,
    /// Withdrawn; not checked or rendered.
    Retired,
}

/// One question an owner must answer before a `needs-owner` card is used.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub(crate) struct OpenQuestion {
    /// Card field the question is about, e.g. `symbols`.
    pub(crate) field: String,
    pub(crate) question: String,
}

#[derive(Clone, Debug)]
pub(crate) struct Card {
    pub(crate) capability: String,
    pub(crate) title: String,
    pub(crate) family: String,
    pub(crate) summary: String,
    pub(crate) source: CardSource,
    pub(crate) symbols: Vec<Symbol>,
    pub(crate) operations: Vec<Operation>,
    pub(crate) usage_protocol: Vec<String>,
    pub(crate) contract: Vec<ContractField>,
    pub(crate) exclusions: Vec<String>,
    pub(crate) evidence: Vec<Evidence>,
    /// Counter-example places; checked for existence only.
    pub(crate) counter_examples: Vec<Place>,
    pub(crate) status: Status,
    /// Why the card is suspended or retired.
    pub(crate) reason: String,
    pub(crate) open_questions: Vec<OpenQuestion>,
}

impl Card {
    /// Source reference of a symbol's file.
    pub(crate) fn symbol_reference(&self, symbol: &Symbol) -> String {
        format!("{}{}", self.source.reference_prefix(), symbol.path)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct AbsentFamily {
    pub(crate) family: String,
    pub(crate) note: String,
}

/// `catalogue.yaml`: catalogue-wide metadata.
#[derive(Clone, Debug)]
pub(crate) struct CatalogueMeta {
    /// Commit the imported or reviewed card set was verified at.
    pub(crate) baseline_commit: String,
    /// Optional banner rendered at the top of the helper page.
    pub(crate) notice: String,
    pub(crate) families: Vec<String>,
    pub(crate) absent: Vec<AbsentFamily>,
    pub(crate) stale_grace_days: u64,
}

/// Loaded catalogue with every schema finding.
#[derive(Debug)]
pub(crate) struct Catalogue {
    pub(crate) meta: CatalogueMeta,
    pub(crate) cards: Vec<Card>,
    pub(crate) findings: Vec<Finding>,
}

impl Catalogue {
    pub(crate) fn has_errors(&self) -> bool {
        self.findings.iter().any(|f| f.level == Level::Error)
    }
}

/// Reads `catalogue.yaml` and `cards/*.yaml` under `root`.
///
/// Malformed cards are reported as findings and left out of `cards`;
/// only I/O failures on the catalogue directory itself are errors.
pub(crate) fn load(root: &Path) -> std::io::Result<Catalogue> {
    let mut findings = Vec::new();
    let meta_text = std::fs::read_to_string(root.join("catalogue.yaml"))?;
    let meta = parse_meta(&meta_text, &mut findings);
    let mut files: Vec<PathBuf> = std::fs::read_dir(root.join("cards"))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "yaml"))
        .collect();
    files.sort();
    let mut cards = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file)?;
        if let Some(card) = parse_card(&text, &file, &meta, &mut findings) {
            cards.push(card);
        }
    }
    for absent in &meta.absent {
        if let Some(card) = cards.iter().find(|card| card.family == absent.family) {
            findings.push(Finding::error(
                "catalogue",
                format!(
                    "family {} is listed as absent but card {} covers it",
                    absent.family, card.capability
                ),
            ));
        }
    }
    Ok(Catalogue {
        meta,
        cards,
        findings,
    })
}

fn load_document(text: &str) -> Result<Yaml, String> {
    let mut documents = YamlLoader::load_from_str(text).map_err(|error| error.to_string())?;
    match documents.len() {
        1 => Ok(documents.remove(0)),
        count => Err(format!("expected one YAML document, found {count}")),
    }
}

fn parse_meta(text: &str, findings: &mut Vec<Finding>) -> CatalogueMeta {
    const NAME: &str = "catalogue";
    let mut meta = CatalogueMeta {
        baseline_commit: String::new(),
        notice: String::new(),
        families: Vec::new(),
        absent: Vec::new(),
        stale_grace_days: 14,
    };
    let document = match load_document(text) {
        Ok(document) => document,
        Err(error) => {
            findings.push(Finding::error(NAME, format!("catalogue.yaml: {error}")));
            return meta;
        }
    };
    match document["schema"].as_i64() {
        Some(SCHEMA_VERSION) => {}
        Some(1) => findings.push(Finding::error(
            NAME,
            format!(
                "catalogue.yaml is schema 1 (line-anchored cards); this checker reads schema \
                 {SCHEMA_VERSION}: convert the cards as described in docs/capabilities/README.md"
            ),
        )),
        other => findings.push(Finding::error(
            NAME,
            format!(
                "catalogue.yaml schema {}: this checker reads schema {SCHEMA_VERSION}",
                other.map_or_else(|| "missing".to_owned(), |v| v.to_string())
            ),
        )),
    }
    meta.baseline_commit = scalar(&document["baseline"]["commit"]).unwrap_or_default();
    meta.notice = scalar(&document["notice"]).unwrap_or_default();
    meta.families = strings(&document["families"], "families", NAME, findings);
    if meta.families.is_empty() {
        findings.push(Finding::error(
            NAME,
            "families must list the known families",
        ));
    }
    for (index, entry) in list(&document["absent"]).iter().enumerate() {
        match scalar(&entry["family"]) {
            Some(family) => meta.absent.push(AbsentFamily {
                family,
                note: scalar(&entry["note"]).unwrap_or_default(),
            }),
            None => findings.push(Finding::error(
                NAME,
                format!("absent[{index}] needs a family"),
            )),
        }
    }
    let maintenance = &document["maintenance"];
    if let Some(days) = maintenance["stale_grace_days"].as_i64() {
        match u64::try_from(days) {
            Ok(days) => meta.stale_grace_days = days,
            Err(_) => findings.push(Finding::error(
                NAME,
                "maintenance.stale_grace_days must not be negative",
            )),
        }
    }
    let model_draft = scalar(&maintenance["model_draft"]);
    if model_draft.as_deref() != Some(MODEL_DRAFT_DISABLED) {
        findings.push(Finding::error(
            NAME,
            format!(
                "maintenance.model_draft must be `{MODEL_DRAFT_DISABLED}`: model drafting is not \
                 implemented, and enabling it spends model quota and needs cost approval"
            ),
        ));
    }
    meta
}

/// Schema 1 fields: line-anchored evidence and the review record.
const SCHEMA_1_HINT: &str = "schema 1 card: schema 2 cites `{path, item, quote}` without line \
                             numbers and replaces `review` / `lifecycle` with `status`; see \
                             docs/capabilities/README.md";

/// Parses one card; returns `None` when required fields are missing.
pub(crate) fn parse_card(
    text: &str,
    file: &Path,
    meta: &CatalogueMeta,
    findings: &mut Vec<Finding>,
) -> Option<Card> {
    let name = file
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let document = match load_document(text) {
        Ok(document) => document,
        Err(error) => {
            findings.push(Finding::error(&name, format!("YAML: {error}")));
            return None;
        }
    };
    if document.as_hash().is_none() {
        findings.push(Finding::error(&name, "card is not a mapping"));
        return None;
    }
    if is_schema_1(&document) {
        findings.push(Finding::error(&name, SCHEMA_1_HINT));
        return None;
    }
    let missing: Vec<&str> = REQUIRED_FIELDS
        .iter()
        .copied()
        .filter(|field| document[*field].is_badvalue())
        .collect();
    if !missing.is_empty() {
        findings.push(Finding::error(
            &name,
            format!("missing fields: {}", missing.join(", ")),
        ));
        return None;
    }
    let field = |key: &str, findings: &mut Vec<Finding>| {
        scalar(&document[key]).unwrap_or_else(|| {
            findings.push(Finding::error(&name, format!("{key} must be a string")));
            String::new()
        })
    };
    let capability = field("capability", findings);
    if capability != name {
        findings.push(Finding::error(
            &name,
            format!("capability `{capability}` does not match the file name"),
        ));
    }
    let family = field("family", findings);
    if !meta.families.is_empty() && !meta.families.contains(&family) {
        findings.push(Finding::warning(
            &name,
            format!("family `{family}` is not listed in catalogue.yaml"),
        ));
    }
    let source = parse_source(&document["source"], &name, findings)?;

    let mut symbols = Vec::new();
    for (index, raw) in list(&document["symbols"]).iter().enumerate() {
        let (Some(fqn), Some(kind), Some(path)) = (
            scalar(&raw["fqn"]),
            scalar(&raw["kind"]),
            scalar(&raw["path"]),
        ) else {
            findings.push(Finding::error(
                &name,
                format!("symbols[{index}] needs fqn, kind and path"),
            ));
            continue;
        };
        let role = scalar(&raw["role"]).unwrap_or_else(|| "primary".to_owned());
        if !SYMBOL_KINDS.contains(&kind.as_str()) {
            findings.push(Finding::error(
                &name,
                format!("symbol {fqn}: kind `{kind}`"),
            ));
        }
        if !SYMBOL_ROLES.contains(&role.as_str()) {
            findings.push(Finding::error(
                &name,
                format!("symbol {fqn}: role `{role}`"),
            ));
        }
        if !is_relative_path(&path) {
            findings.push(Finding::error(
                &name,
                format!("symbol {fqn}: path must name a file in the crate"),
            ));
            continue;
        }
        let first_segment = fqn.split("::").next().unwrap_or_default();
        if first_segment != source.crate_name().replace('-', "_") {
            findings.push(Finding::warning(
                &name,
                format!(
                    "symbol {fqn}: first segment is not {}",
                    source.crate_name().replace('-', "_")
                ),
            ));
        }
        symbols.push(Symbol {
            fqn,
            kind,
            path,
            role,
        });
    }
    if symbols.is_empty() {
        findings.push(Finding::error(&name, "no symbols"));
    }

    let mut evidence = Vec::new();
    let mut operations = Vec::new();
    for raw in list(&document["operations"]) {
        let op_name = scalar(&raw["name"]).unwrap_or_default();
        for fqn in strings(&raw["symbols"], "operations.symbols", &name, findings) {
            if !symbols.iter().any(|symbol| symbol.fqn == fqn) {
                findings.push(Finding::error(
                    &name,
                    format!("operations.{op_name}: {fqn} is not in symbols"),
                ));
            }
        }
        collect_evidence(
            &raw["evidence"],
            &format!("operations.{op_name}"),
            &name,
            &mut evidence,
            findings,
        );
        operations.push(Operation {
            name: op_name,
            effect: scalar(&raw["effect"]).unwrap_or_default(),
        });
    }
    let mut usage_protocol = Vec::new();
    for (index, raw) in list(&document["usage_protocol"]).iter().enumerate() {
        usage_protocol.push(scalar(&raw["step"]).unwrap_or_default());
        collect_evidence(
            &raw["evidence"],
            &format!("usage_protocol[{index}]"),
            &name,
            &mut evidence,
            findings,
        );
    }

    let mut contract = Vec::new();
    let raw_contract = &document["contract"];
    for key in CONTRACT_FIELDS {
        let raw = &raw_contract[*key];
        let Some(value) = scalar(&raw["value"]) else {
            findings.push(Finding::error(
                &name,
                format!("contract.{key} needs {{value, evidence}}"),
            ));
            continue;
        };
        let before = evidence.len();
        collect_evidence(
            &raw["evidence"],
            &format!("contract.{key}"),
            &name,
            &mut evidence,
            findings,
        );
        if value.trim() != UNKNOWN && evidence.len() == before {
            findings.push(Finding::error(
                &name,
                format!("contract.{key} = {value:?} without evidence (write `unknown`)"),
            ));
        }
        contract.push(ContractField {
            name: (*key).to_owned(),
            value,
        });
    }
    if let Some(hash) = raw_contract.as_hash() {
        for key in hash.keys().filter_map(Yaml::as_str) {
            if !CONTRACT_FIELDS.contains(&key) {
                findings.push(Finding::warning(
                    &name,
                    format!("contract.{key} is not a schema field"),
                ));
            }
        }
    }
    collect_evidence(
        &document["layers"]["evidence"],
        "layers",
        &name,
        &mut evidence,
        findings,
    );

    let mut counter_examples = Vec::new();
    for (index, raw) in list(&document["counter_examples"]).iter().enumerate() {
        match scalar(&raw["path"]) {
            Some(path) if is_relative_path(&path) => counter_examples.push(Place {
                path,
                item: scalar(&raw["item"]),
            }),
            _ => findings.push(Finding::error(
                &name,
                format!("counter_examples[{index}] needs a relative path"),
            )),
        }
        collect_evidence(
            &raw["evidence"],
            &format!("counter_examples[{index}]"),
            &name,
            &mut evidence,
            findings,
        );
    }

    let aliases = &document["aliases"];
    let aliases_en = strings(&aliases["en"], "aliases.en", &name, findings);
    let aliases_zh = strings(&aliases["zh"], "aliases.zh", &name, findings);
    if aliases_en.is_empty() || aliases_zh.is_empty() {
        findings.push(Finding::error(&name, "aliases need both en and zh"));
    }
    strings(&document["identifiers"], "identifiers", &name, findings);
    let exclusions = strings(&document["exclusions"], "exclusions", &name, findings);
    let status = parse_status(&document["status"], &name, findings);
    let reason = scalar(&document["reason"]).unwrap_or_default();
    if matches!(status, Status::Suspended | Status::Retired) && reason.trim().is_empty() {
        findings.push(Finding::error(
            &name,
            "a suspended or retired card needs a reason",
        ));
    }
    let mut open_questions = Vec::new();
    for (index, raw) in list(&document["open_questions"]).iter().enumerate() {
        let question = scalar(&raw["question"]).unwrap_or_default();
        if question.trim().is_empty() {
            findings.push(Finding::error(
                &name,
                format!("open_questions[{index}] needs a question"),
            ));
        }
        open_questions.push(OpenQuestion {
            field: scalar(&raw["field"]).unwrap_or_default(),
            question,
        });
        collect_evidence(
            &raw["evidence"],
            &format!("open_questions[{index}]"),
            &name,
            &mut evidence,
            findings,
        );
    }
    match (status, open_questions.is_empty()) {
        (Status::NeedsOwner, true) => findings.push(Finding::error(
            &name,
            "a needs-owner card needs at least one open question",
        )),
        (Status::Active, false) => findings.push(Finding::error(
            &name,
            "open_questions belong to a needs-owner card",
        )),
        _ => {}
    }

    Some(Card {
        capability,
        title: field("title", findings),
        family,
        summary: field("summary", findings),
        source,
        symbols,
        operations,
        usage_protocol,
        contract,
        exclusions,
        evidence,
        counter_examples,
        status,
        reason,
        open_questions,
    })
}

/// Whether the card still uses schema 1 fields anywhere.
fn is_schema_1(document: &Yaml) -> bool {
    fn has_at(raw: &Yaml) -> bool {
        match raw {
            Yaml::Hash(hash) => hash
                .iter()
                .any(|(key, value)| matches!(key.as_str(), Some("at" | "line")) || has_at(value)),
            Yaml::Array(items) => items.iter().any(has_at),
            _ => false,
        }
    }
    !document["review"].is_badvalue() || !document["lifecycle"].is_badvalue() || has_at(document)
}

fn parse_status(raw: &Yaml, name: &str, findings: &mut Vec<Finding>) -> Status {
    match scalar(raw).as_deref() {
        None | Some("active") => Status::Active,
        Some("needs-owner") => Status::NeedsOwner,
        Some("suspended") => Status::Suspended,
        Some("retired") => Status::Retired,
        Some(other) => {
            findings.push(Finding::error(name, format!("status `{other}`")));
            // Unknown status: keep the card out of use.
            Status::Suspended
        }
    }
}

fn parse_source(raw: &Yaml, name: &str, findings: &mut Vec<Finding>) -> Option<CardSource> {
    let kind = scalar(&raw["kind"]).unwrap_or_default();
    let Some(crate_name) = scalar(&raw["crate"]) else {
        findings.push(Finding::error(name, "source.crate is required"));
        return None;
    };
    match kind.as_str() {
        "workspace" => match scalar(&raw["path"]) {
            Some(path) if is_relative_path(&path) => {
                Some(CardSource::Workspace { crate_name, path })
            }
            _ => {
                findings.push(Finding::error(
                    name,
                    "workspace source needs a repository path",
                ));
                None
            }
        },
        "dependency" => match scalar(&raw["version"]) {
            Some(version) => Some(CardSource::Dependency {
                crate_name,
                version,
            }),
            None => {
                findings.push(Finding::error(name, "dependency source needs a version"));
                None
            }
        },
        other => {
            findings.push(Finding::error(name, format!("source.kind `{other}`")));
            None
        }
    }
}

fn collect_evidence(
    raw: &Yaml,
    field: &str,
    name: &str,
    out: &mut Vec<Evidence>,
    findings: &mut Vec<Finding>,
) {
    for entry in list(raw) {
        let path = scalar(&entry["path"]).unwrap_or_default();
        let quote = scalar(&entry["quote"]).unwrap_or_default();
        if !is_relative_path(&path) {
            findings.push(Finding::error(
                name,
                format!("{field}: evidence path `{path}` is not a relative file path"),
            ));
            continue;
        }
        if quote.trim().is_empty() {
            findings.push(Finding::error(
                name,
                format!("{field}: evidence in {path} has no quote"),
            ));
            continue;
        }
        let item = scalar(&entry["item"]);
        if item
            .as_deref()
            .is_some_and(|item| item.split("::").any(|segment| segment.trim().is_empty()))
        {
            findings.push(Finding::error(
                name,
                format!(
                    "{field}: evidence item `{}` is not a path",
                    item.unwrap_or_default()
                ),
            ));
            continue;
        }
        out.push(Evidence {
            field: field.to_owned(),
            path,
            item,
            quote,
        });
    }
}

/// Rejects absolute paths and `..` so a card cannot cite files outside the
/// repository or the dependency crate.
pub(crate) fn is_relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && path.split('/').all(|part| part != ".." && !part.is_empty())
}

/// String form of a scalar: strings as-is, numbers and booleans as written.
fn scalar(raw: &Yaml) -> Option<String> {
    match raw {
        Yaml::String(text) | Yaml::Real(text) => Some(text.clone()),
        Yaml::Integer(value) => Some(value.to_string()),
        Yaml::Boolean(value) => Some(value.to_string()),
        _ => None,
    }
}

fn list(raw: &Yaml) -> &[Yaml] {
    raw.as_vec().map_or(&[], Vec::as_slice)
}

fn strings(raw: &Yaml, field: &str, name: &str, findings: &mut Vec<Finding>) -> Vec<String> {
    if raw.is_badvalue() || raw.is_null() {
        return Vec::new();
    }
    let Some(items) = raw.as_vec() else {
        findings.push(Finding::error(name, format!("{field} must be a list")));
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let value = scalar(item);
            if value.is_none() {
                findings.push(Finding::error(name, format!("{field} must hold strings")));
            }
            value
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{CatalogueMeta, Level, Status, parse_card};

    fn meta() -> CatalogueMeta {
        CatalogueMeta {
            baseline_commit: String::new(),
            notice: String::new(),
            families: vec!["container.ring".into()],
            absent: Vec::new(),
            stale_grace_days: 14,
        }
    }

    const CARD: &str = r##"
capability: ring
title: Ring
family: container.ring
summary: A ring.
source: {kind: workspace, crate: kring, path: util/kring}
symbols:
  - {fqn: kring::Ring, kind: struct, path: util/kring/src/lib.rs, role: primary}
operations:
  - name: push
    symbols: [kring::Ring]
    effect: append
    evidence: [{path: util/kring/src/lib.rs, item: kring::Ring::push, quote: "pub fn push"}]
contract:
  no_std: {value: "yes", evidence: [{path: util/kring/src/lib.rs, quote: "#![no_std]"}]}
  allocation: {value: unknown, evidence: []}
  blocking: {value: unknown, evidence: []}
  irq_nmi: {value: unknown, evidence: []}
  ownership_lifetime: {value: unknown, evidence: []}
  errors_bounds: {value: unknown, evidence: []}
  concurrency: {value: unknown, evidence: []}
layers: {depends_on: [], used_by: [], allowed_layers: unknown, evidence: []}
aliases: {en: [ring buffer], zh: [环形缓冲区]}
"##;

    fn parse(text: &str, file: &str) -> (Option<super::Card>, Vec<String>) {
        let mut findings = Vec::new();
        let card = parse_card(text, Path::new(file), &meta(), &mut findings);
        let errors = findings
            .iter()
            .filter(|f| f.level == Level::Error)
            .map(ToString::to_string)
            .collect();
        (card, errors)
    }

    #[test]
    fn valid_card_parses_without_findings() {
        let mut findings = Vec::new();
        let card = parse_card(CARD, Path::new("cards/ring.yaml"), &meta(), &mut findings).unwrap();
        assert!(findings.is_empty(), "{findings:?}");
        assert_eq!(card.evidence.len(), 2);
        assert_eq!(card.evidence[0].item.as_deref(), Some("kring::Ring::push"));
        assert_eq!(card.evidence[1].item, None);
        assert_eq!(card.status, Status::Active);
    }

    #[test]
    fn claims_without_evidence_and_bad_paths_are_errors() {
        let text = CARD
            .replace(
                r##"no_std: {value: "yes", evidence: [{path: util/kring/src/lib.rs, quote: "#![no_std]"}]}"##,
                r#"no_std: {value: "yes", evidence: []}"#,
            )
            .replace("path: util/kring/src/lib.rs, item", "path: ../outside.rs, item");
        let (_, errors) = parse(&text, "cards/ring.yaml");
        assert!(errors.iter().any(|m| m.contains("without evidence")));
        assert!(
            errors
                .iter()
                .any(|m| m.contains("not a relative file path"))
        );
    }

    #[test]
    fn schema_1_cards_are_refused() {
        for text in [
            CARD.replace(
                "path: util/kring/src/lib.rs, role",
                "path: util/kring/src/lib.rs, line: 3, role",
            ),
            CARD.replace(
                "{path: util/kring/src/lib.rs, quote: \"#![no_std]\"}",
                "{at: \"util/kring/src/lib.rs:1\", quote: \"#![no_std]\"}",
            ),
            format!("{CARD}review: {{reviewer: R, date: x, verdict: ok}}\n"),
            format!("{CARD}lifecycle: {{state: active}}\n"),
        ] {
            let (card, errors) = parse(&text, "cards/ring.yaml");
            assert!(card.is_none());
            assert!(
                errors.iter().any(|m| m.contains("schema 1 card")),
                "{errors:?}"
            );
        }
    }

    #[test]
    fn name_mismatch_unknown_family_and_bad_status_are_reported() {
        let text = CARD.replace("family: container.ring", "family: container.other")
            + "status: suspended\n";
        let mut findings = Vec::new();
        parse_card(&text, Path::new("cards/other.yaml"), &meta(), &mut findings).unwrap();
        let text: Vec<String> = findings.iter().map(ToString::to_string).collect();
        assert!(
            text.iter()
                .any(|m| m.contains("does not match the file name"))
        );
        assert!(
            text.iter()
                .any(|m| m.starts_with("warning") && m.contains("container.other"))
        );
        assert!(text.iter().any(|m| m.contains("needs a reason")));
    }

    #[test]
    fn needs_owner_cards_carry_open_questions() {
        let (card, errors) = parse(
            &format!(
                "{CARD}status: needs-owner\nopen_questions:\n  - field: symbols\n    question: \
                 Should Ring be public?\n"
            ),
            "cards/ring.yaml",
        );
        assert!(errors.is_empty(), "{errors:?}");
        let card = card.unwrap();
        assert_eq!(card.status, Status::NeedsOwner);
        assert_eq!(card.open_questions[0].field, "symbols");

        let (_, errors) = parse(&format!("{CARD}status: needs-owner\n"), "cards/ring.yaml");
        assert!(
            errors
                .iter()
                .any(|m| m.contains("at least one open question"))
        );
        let (_, errors) = parse(
            &format!("{CARD}open_questions: [{{field: x, question: y}}]\n"),
            "cards/ring.yaml",
        );
        assert!(errors.iter().any(|m| m.contains("belong to a needs-owner")));
        let (card, errors) = parse(&format!("{CARD}status: maybe\n"), "cards/ring.yaml");
        assert!(!errors.is_empty());
        assert_eq!(card.unwrap().status, Status::Suspended);
    }
}
