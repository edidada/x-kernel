// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Checks one card against the source tree and classifies its drift.
//!
//! Cards cite items and quotes, never lines. The checker locates every
//! cited item with `syn` (in the card's file, or anywhere in the crate when
//! it moved), compares its token fingerprint with the stamp, and finds each
//! quote exactly once inside its item (or inside its file for a quote
//! outside any item). Line numbers are computed here and only reported.
//!
//! * fresh: every cited item is found and matches its stamp, every quote is
//!   present exactly once; moved code stays fresh;
//! * stale: a cited item's signature or body changed, an item is missing or
//!   renamed, a quote is gone or no longer unique, the locked dependency
//!   version changed, or the card was never stamped;
//! * unchecked: the source could not be read or parsed here.

use std::{collections::BTreeSet, rc::Rc};

use serde::Serialize;

use super::{
    card::{Card, CardSource, Evidence, Symbol},
    items::{ItemHashes, ItemIndex, RustItem},
    source::{SourceFile, SourceTree},
    stamps::{CardStamp, ItemStamp},
};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Drift {
    Fresh,
    Stale,
    Unchecked,
}

/// Where a symbol was found at check time.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct SymbolLocation {
    pub(crate) fqn: String,
    /// Source reference of the file that declares it now.
    pub(crate) path: String,
    pub(crate) line: usize,
}

/// Where a quote was found at check time.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct QuoteLocation {
    pub(crate) field: String,
    /// Source reference of the file the quote is in now.
    pub(crate) path: String,
    pub(crate) item: Option<String>,
    pub(crate) line: usize,
    pub(crate) last_line: usize,
}

/// A cited item that no longer matches its stamp.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ChangedItem {
    /// Symbol FQN or evidence item path, as written in the card.
    pub(crate) item: String,
    /// `signature`, `body`, `missing` or `ambiguous`.
    pub(crate) change: &'static str,
    /// File the item was in when stamped.
    pub(crate) stamped_path: Option<String>,
    /// File it is in now, when found.
    pub(crate) path: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct CardCheck {
    pub(crate) drift: Drift,
    /// Why the card is stale or unchecked.
    pub(crate) reasons: Vec<String>,
    /// Non-blocking observations (moved items, rename candidates,
    /// counter-example drift).
    pub(crate) notes: Vec<String>,
    pub(crate) changed_items: Vec<ChangedItem>,
    pub(crate) symbols: Vec<SymbolLocation>,
    pub(crate) evidence: Vec<QuoteLocation>,
    /// Source references involved in a change, for history and diffs.
    pub(crate) involved: BTreeSet<String>,
    /// Stamp of the card as the tree is now.
    pub(crate) current: CardStamp,
}

/// Declarations one card path resolved to, all in one file.
struct Found {
    reference: String,
    file: Rc<SourceFile>,
    items: Vec<RustItem>,
}

impl Found {
    fn hashes(&self) -> ItemHashes {
        ItemHashes::combine(self.items.iter().map(|item| &item.hashes))
    }

    fn stamp(&self) -> ItemStamp {
        ItemStamp {
            path: self.reference.clone(),
            chain: self.items[0].chain.join("::"),
            hashes: self.hashes(),
        }
    }
}

enum Lookup {
    Found(Found),
    Missing,
    /// Message for the reasons.
    Problem(Drift, String),
}

struct Checker<'a> {
    card: &'a Card,
    stamp: Option<&'a CardStamp>,
    tree: &'a mut SourceTree,
    stale: Vec<String>,
    unchecked: Vec<String>,
    notes: Vec<String>,
    changed_items: Vec<ChangedItem>,
    symbols: Vec<SymbolLocation>,
    evidence: Vec<QuoteLocation>,
    involved: BTreeSet<String>,
    current: CardStamp,
}

/// Checks `card` against `tree`, comparing with its `stamp` when present.
pub(crate) fn check_card(
    card: &Card,
    stamp: Option<&CardStamp>,
    tree: &mut SourceTree,
) -> CardCheck {
    let mut checker = Checker {
        card,
        stamp,
        tree,
        stale: Vec::new(),
        unchecked: Vec::new(),
        notes: Vec::new(),
        changed_items: Vec::new(),
        symbols: Vec::new(),
        evidence: Vec::new(),
        involved: BTreeSet::new(),
        current: CardStamp::default(),
    };
    if checker.check_source() {
        for symbol in &card.symbols {
            checker.check_symbol(symbol);
        }
        for evidence in &card.evidence {
            checker.check_evidence(evidence);
        }
        checker.check_counter_examples();
    }
    checker.finish()
}

impl Checker<'_> {
    /// Returns whether the card's sources can be read at all.
    fn check_source(&mut self) -> bool {
        match &self.card.source {
            CardSource::Dependency {
                crate_name,
                version,
            } => {
                if !self.tree.locked.is_locked(crate_name, version) {
                    let locked: Vec<&str> = self
                        .tree
                        .locked
                        .registry
                        .get(crate_name)
                        .map(|versions| versions.iter().map(String::as_str).collect())
                        .unwrap_or_default();
                    self.stale.push(format!(
                        "dependency {crate_name} {version} is no longer in Cargo.lock (locked: {})",
                        if locked.is_empty() {
                            "none".to_owned()
                        } else {
                            locked.join(", ")
                        }
                    ));
                    return false;
                }
                if self.tree.dependency_dir(crate_name, version).is_none() {
                    self.unchecked.push(format!(
                        "source of {crate_name}-{version} is not in the Cargo registry cache; run \
                         `cargo fetch --locked`"
                    ));
                    return false;
                }
            }
            CardSource::Workspace { path, .. } => {
                if self.tree.file(&format!("{path}/Cargo.toml")).is_none() {
                    self.stale
                        .push(format!("crate directory {path} has no Cargo.toml"));
                    return false;
                }
            }
        }
        true
    }

    fn problem(&mut self, drift: Drift, message: String) {
        match drift {
            Drift::Unchecked => self.unchecked.push(message),
            _ => self.stale.push(message),
        }
    }

    /// Finds the declarations `path` names, starting in `reference`. With a
    /// stamp, the stamped chain is looked up first in that file and then in
    /// the rest of the crate, so moved code is found.
    fn lookup(
        &mut self,
        reference: &str,
        path: &str,
        kind: Option<&str>,
        stamped: Option<&ItemStamp>,
    ) -> Lookup {
        let file = self.tree.file(reference);
        let index = match file.as_ref().map(|file| file.items()) {
            Some(Ok(index)) => Some(index),
            Some(Err(error)) => {
                return Lookup::Problem(
                    Drift::Unchecked,
                    format!("cannot parse {reference}: {error}"),
                );
            }
            None => None,
        };
        let found_in = |file: &Rc<SourceFile>, reference: &str, items: Vec<&RustItem>| Found {
            reference: reference.to_owned(),
            file: Rc::clone(file),
            items: items.into_iter().cloned().collect(),
        };
        if let Some(stamped) = stamped {
            let chain: Vec<String> = stamped.chain.split("::").map(str::to_owned).collect();
            if let (Some(file), Some(index)) = (&file, &index) {
                let items: Vec<&RustItem> = index.exact(&chain, kind).collect();
                if !items.is_empty() {
                    return Lookup::Found(found_in(file, reference, items));
                }
            }
            let mut elsewhere = Vec::new();
            for (other, other_file) in self.crate_rust_files() {
                if other == reference {
                    continue;
                }
                let Ok(other_index) = other_file.items() else {
                    continue;
                };
                let items: Vec<&RustItem> = other_index.exact(&chain, kind).collect();
                if !items.is_empty() {
                    elsewhere.push(found_in(&other_file, &other, items));
                }
            }
            match elsewhere.len() {
                0 => {}
                1 => return Lookup::Found(elsewhere.remove(0)),
                count => {
                    let files: Vec<&str> = elsewhere.iter().map(|f| f.reference.as_str()).collect();
                    return Lookup::Problem(
                        Drift::Stale,
                        format!(
                            "{path}: moved out of {reference}, now declared in {count} files ({})",
                            files.join(", ")
                        ),
                    );
                }
            }
        }
        if let (Some(file), Some(index)) = (&file, &index) {
            let items = index.resolve(path, kind);
            if !items.is_empty() {
                return Lookup::Found(found_in(file, reference, items));
            }
        }
        Lookup::Missing
    }

    /// Compares a found item with its stamp; returns whether it matches.
    fn compare(&mut self, name: &str, found: &Found, stamped: Option<&ItemStamp>) -> bool {
        let Some(stamped) = stamped else {
            return true;
        };
        let hashes = found.hashes();
        let change = if stamped.hashes.signature != hashes.signature {
            Some(("signature", "signature or visibility changed"))
        } else if stamped.hashes.item != hashes.item {
            Some(("body", "item changed (body, fields or bounds)"))
        } else {
            None
        };
        let Some((change, message)) = change else {
            return true;
        };
        self.stale.push(format!("{name}: {message}"));
        self.changed(name, change, Some(stamped), Some(&found.reference));
        false
    }

    fn changed(
        &mut self,
        name: &str,
        change: &'static str,
        stamped: Option<&ItemStamp>,
        path: Option<&str>,
    ) {
        self.involved.extend(
            stamped
                .map(|s| s.path.clone())
                .into_iter()
                .chain(path.map(str::to_owned)),
        );
        self.changed_items.push(ChangedItem {
            item: name.to_owned(),
            change,
            stamped_path: stamped.map(|s| s.path.clone()),
            path: path.map(str::to_owned),
        });
    }

    fn note_moved(&mut self, name: &str, cited: &str, found: &Found) {
        if found.reference != cited {
            self.notes.push(format!(
                "{name}: moved from {cited} to {}; the card still names the old file",
                found.reference
            ));
        }
    }

    fn check_symbol(&mut self, symbol: &Symbol) {
        let reference = self.card.symbol_reference(symbol);
        let stamped = self
            .stamp
            .and_then(|stamp| stamp.symbols.get(&symbol.fqn))
            .cloned();
        let name = format!("symbol {}", symbol.fqn);
        match self.lookup(
            &reference,
            &symbol.fqn,
            Some(&symbol.kind),
            stamped.as_ref(),
        ) {
            Lookup::Found(found) => {
                self.note_moved(&name, &reference, &found);
                self.compare(&name, &found, stamped.as_ref());
                self.symbols.push(SymbolLocation {
                    fqn: symbol.fqn.clone(),
                    path: found.reference.clone(),
                    line: found.items[0].name_line,
                });
                self.current
                    .symbols
                    .insert(symbol.fqn.clone(), found.stamp());
            }
            Lookup::Missing => {
                self.stale.push(format!(
                    "{name} ({}) not found in {reference} or elsewhere in the crate (removed or \
                     renamed)",
                    symbol.kind
                ));
                self.changed(&name, "missing", stamped.as_ref(), None);
                self.involved.insert(reference);
                self.suggest_renames(&name, Some(&symbol.kind), stamped.as_ref());
            }
            Lookup::Problem(drift, message) => {
                if drift == Drift::Stale {
                    self.changed(&name, "ambiguous", stamped.as_ref(), None);
                }
                self.problem(drift, format!("{name}: {message}"));
            }
        }
    }

    fn suggest_renames(&mut self, name: &str, kind: Option<&str>, stamped: Option<&ItemStamp>) {
        let Some(shape) = stamped.map(|stamped| stamped.hashes.shape.clone()) else {
            return;
        };
        for (reference, file) in self.crate_rust_files() {
            let Ok(items) = file.items() else {
                continue;
            };
            for item in items.items.iter().filter(|item| {
                kind.is_none_or(|kind| item.kind == kind) && item.hashes.shape == shape
            }) {
                self.notes.push(format!(
                    "{name}: possibly renamed to {} in {reference}",
                    item.chain.join("::")
                ));
            }
        }
    }

    fn crate_rust_files(&mut self) -> Vec<(String, Rc<SourceFile>)> {
        let root = self.card.source.root_reference();
        self.tree
            .rust_files_under(&root)
            .into_iter()
            .filter_map(|reference| {
                let file = self.tree.file(&reference)?;
                Some((reference, file))
            })
            .collect()
    }

    fn check_evidence(&mut self, evidence: &Evidence) {
        match &evidence.item {
            Some(item) => self.check_item_quote(evidence, item),
            None => self.check_file_quote(evidence),
        }
    }

    fn check_item_quote(&mut self, evidence: &Evidence, item: &str) {
        let field = &evidence.field;
        let stamped = self.stamp.and_then(|stamp| stamp.items.get(item)).cloned();
        let name = format!("{field}: item {item}");
        let found = match self.lookup(&evidence.path, item, None, stamped.as_ref()) {
            Lookup::Found(found) => found,
            Lookup::Missing => {
                self.stale.push(format!(
                    "{name} not found in {} or elsewhere in the crate (removed or renamed)",
                    evidence.path
                ));
                self.changed(item, "missing", stamped.as_ref(), None);
                self.involved.insert(evidence.path.clone());
                self.suggest_renames(&name, None, stamped.as_ref());
                return;
            }
            Lookup::Problem(drift, message) => {
                if drift == Drift::Stale {
                    self.changed(item, "ambiguous", stamped.as_ref(), None);
                }
                self.problem(drift, format!("{name}: {message}"));
                return;
            }
        };
        self.note_moved(&name, &evidence.path, &found);
        self.compare(item, &found, stamped.as_ref());
        self.current.items.insert(item.to_owned(), found.stamp());
        let lines = &found.file.lines;
        let hits: Vec<(&RustItem, usize)> = found
            .items
            .iter()
            .map(|it| {
                (
                    it,
                    count_quote(lines, it.start_line, it.end_line, &evidence.quote),
                )
            })
            .filter(|(_, count)| *count > 0)
            .collect();
        // Several declarations of one path (cfg variants, impl blocks) are
        // one item: the quote may appear once in each of them.
        match (hits.iter().map(|(_, count)| *count).max(), hits.first()) {
            (Some(1), Some((it, _))) => {
                if let Some((line, last_line)) =
                    locate_quote(lines, it.start_line, it.end_line, &evidence.quote)
                {
                    self.evidence.push(QuoteLocation {
                        field: field.clone(),
                        path: found.reference.clone(),
                        item: Some(item.to_owned()),
                        line,
                        last_line,
                    });
                }
            }
            (None, _) | (_, None) => {
                self.involved.insert(found.reference.clone());
                self.stale.push(format!(
                    "{name}: quote not found in the item: {:?}",
                    truncate(&evidence.quote, 60)
                ));
            }
            (Some(count), _) => self.stale.push(format!(
                "{name}: quote appears {count} times in the item; extend the quote: {:?}",
                truncate(&evidence.quote, 60)
            )),
        }
    }

    fn check_file_quote(&mut self, evidence: &Evidence) {
        let field = &evidence.field;
        let path = &evidence.path;
        let Some(file) = self.tree.file(path) else {
            self.involved.insert(path.clone());
            self.stale.push(format!("{field}: {path} no longer exists"));
            return;
        };
        let lines = &file.lines;
        match count_quote(lines, 1, lines.len(), &evidence.quote) {
            0 => {
                self.involved.insert(path.clone());
                self.stale.push(format!(
                    "{field}: quote not found in {path}: {:?}",
                    truncate(&evidence.quote, 60)
                ));
                return;
            }
            1 => {}
            count => {
                self.stale.push(format!(
                    "{field}: quote appears {count} times in {path}; extend the quote or name its \
                     item: {:?}",
                    truncate(&evidence.quote, 60)
                ));
                return;
            }
        }
        let Some((line, last_line)) = locate_quote(lines, 1, lines.len(), &evidence.quote) else {
            return;
        };
        if is_rust(path) {
            match file.items() {
                Ok(index) => {
                    if let Some(owner) = owner_name(&index, line, last_line) {
                        self.stale.push(format!(
                            "{field}: quote in {path} lies inside item {owner}; name it as `item`"
                        ));
                        return;
                    }
                }
                Err(error) => {
                    self.unchecked.push(format!("cannot parse {path}: {error}"));
                    return;
                }
            }
        }
        self.evidence.push(QuoteLocation {
            field: field.clone(),
            path: path.clone(),
            item: None,
            line,
            last_line,
        });
    }

    /// Counter-examples carry no fingerprint; only note when their file or
    /// item is gone.
    fn check_counter_examples(&mut self) {
        for place in &self.card.counter_examples {
            let Some(file) = self.tree.file(&place.path) else {
                self.notes.push(format!(
                    "counter-example {place}: the file no longer exists"
                ));
                continue;
            };
            if let Some(item) = &place.item
                && file
                    .items()
                    .map_or(true, |index| index.resolve(item, None).is_empty())
            {
                self.notes.push(format!(
                    "counter-example {place}: the item is no longer there"
                ));
            }
        }
    }

    fn finish(mut self) -> CardCheck {
        if self.stamp.is_none() {
            self.stale
                .push("not stamped: review it, then run `xkmake capability stamp`".to_owned());
        }
        let drift = if !self.stale.is_empty() {
            Drift::Stale
        } else if !self.unchecked.is_empty() {
            Drift::Unchecked
        } else {
            Drift::Fresh
        };
        let mut reasons = self.stale;
        reasons.append(&mut self.unchecked);
        CardCheck {
            drift,
            reasons,
            notes: self.notes,
            changed_items: self.changed_items,
            symbols: self.symbols,
            evidence: self.evidence,
            involved: self.involved,
            current: self.current,
        }
    }
}

fn is_rust(path: &str) -> bool {
    path.ends_with(".rs")
}

/// Path inside the file of the innermost item spanning the lines.
pub(crate) fn owner_name(index: &ItemIndex, first: usize, last: usize) -> Option<String> {
    index
        .owner_of(first, last)
        .map(|item| item.chain.join("::"))
}

fn truncate(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((index, _)) => format!("{}...", &text[..index]),
        None => text.to_owned(),
    }
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Removes one leading comment marker (`//`, `///`, `//!`, `/*`, `*`, `*/`,
/// `#`) and one following space, so quotes may span comment lines.
fn strip_comment_marker(line: &str) -> &str {
    let rest = line.trim_start();
    let rest = if let Some(after) = rest.strip_prefix("//") {
        let after = after.strip_prefix('/').unwrap_or(after);
        after.strip_prefix('!').unwrap_or(after)
    } else if let Some(after) = rest.strip_prefix("/*") {
        after.trim_start_matches('*')
    } else if rest.starts_with('*') {
        let after = rest.trim_start_matches('*');
        after.strip_prefix('/').unwrap_or(after)
    } else if let Some(after) = rest.strip_prefix('#') {
        after
    } else {
        rest
    };
    rest.strip_prefix(char::is_whitespace).unwrap_or(rest)
}

/// The two forms a quote is compared against: lines joined with collapsed
/// whitespace, and the same with leading comment markers removed.
fn haystacks(lines: &[String], first: usize, last: usize) -> Option<[String; 2]> {
    if first == 0 || first > last || last > lines.len() {
        return None;
    }
    let chunk = &lines[first - 1..last];
    Some([
        collapse_whitespace(&chunk.join(" ")),
        collapse_whitespace(
            &chunk
                .iter()
                .map(|line| strip_comment_marker(line))
                .collect::<Vec<_>>()
                .join(" "),
        ),
    ])
}

/// Whether `quote` appears in lines `first..=last` (1-based).
pub(crate) fn quote_in(lines: &[String], first: usize, last: usize, quote: &str) -> bool {
    let wanted = collapse_whitespace(quote);
    haystacks(lines, first, last).is_some_and(|forms| forms.iter().any(|f| f.contains(&wanted)))
}

/// Occurrences of `quote` in lines `first..=last`: the larger count of the
/// two compared forms.
pub(crate) fn count_quote(lines: &[String], first: usize, last: usize, quote: &str) -> usize {
    let wanted = collapse_whitespace(quote);
    if wanted.is_empty() {
        return 0;
    }
    haystacks(lines, first, last).map_or(0, |forms| {
        forms
            .iter()
            .map(|form| form.matches(wanted.as_str()).count())
            .max()
            .unwrap_or(0)
    })
}

/// Minimal line range (1-based, inclusive) within `first..=last` holding
/// the first occurrence of `quote`.
pub(crate) fn locate_quote(
    lines: &[String],
    first: usize,
    last: usize,
    quote: &str,
) -> Option<(usize, usize)> {
    if !quote_in(lines, first, last, quote) {
        return None;
    }
    // Smallest end with the quote in first..=end, then the largest start.
    let (mut low, mut high) = (first, last);
    while low < high {
        let middle = low + (high - low) / 2;
        if quote_in(lines, first, middle, quote) {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    let end = low;
    let (mut low, mut high) = (first, end);
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        if quote_in(lines, middle, end, quote) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    Some((low, end))
}

#[cfg(test)]
mod tests {
    use super::{count_quote, locate_quote, quote_in, strip_comment_marker};

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    #[test]
    fn quotes_match_across_lines_and_comment_markers() {
        let text =
            lines("/// Returns `Err(value)` only when\n/// the key is occupied.\nfn insert() {}\n");
        assert!(quote_in(&text, 1, 2, "only when the key is occupied"));
        assert!(quote_in(&text, 3, 3, "fn   insert()"));
        assert!(!quote_in(&text, 3, 3, "only when"));
        assert!(!quote_in(&text, 3, 4, "fn insert"));
        assert_eq!(strip_comment_marker("    //! doc"), "doc");
        assert_eq!(strip_comment_marker(" * item"), "item");
        assert_eq!(strip_comment_marker("#![no_std]"), "![no_std]");
    }

    #[test]
    fn quotes_are_counted_and_located() {
        let text = lines("a\nfirst half\nsecond half\nb\nfirst half\nsecond half b\n");
        assert_eq!(count_quote(&text, 1, 6, "half second"), 2);
        assert_eq!(count_quote(&text, 1, 4, "half second"), 1);
        assert_eq!(count_quote(&text, 1, 6, "b"), 2);
        assert_eq!(count_quote(&text, 1, 6, "missing"), 0);
        assert_eq!(locate_quote(&text, 1, 6, "half second"), Some((2, 3)));
        assert_eq!(locate_quote(&text, 4, 6, "half second"), Some((5, 6)));
        assert_eq!(locate_quote(&text, 1, 6, "missing"), None);
        assert_eq!(count_quote(&lines("#![no_std]"), 1, 1, "#![no_std]"), 1);
    }
}
