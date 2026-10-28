// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kconfig implication facts extracted from the xconfig parser's AST.
//!
//! "A implies B" means every configuration with A enabled also enables B:
//! B is a positive conjunct of A's effective dependency (its own `depends on`
//! plus the conditions inherited from enclosing `menu`, `if` and `choice`
//! blocks), or A unconditionally `select`s B, applied transitively. Anything
//! weaker (disjunctions, negations, conditional selects, `imply`) does not
//! count, so the relation is conservative.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use xconfig::{
    build_config::cargo_feature_for_symbol,
    kconfig::{Entry, Expr, Parser, Property, SymbolType},
};

use crate::error::Result;

/// Implication-relevant facts of one Kconfig symbol.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SymbolFacts {
    /// Bool or tristate symbol.
    pub(crate) is_bool: bool,
    /// Symbols every definition's effective dependency requires.
    pub(crate) requires: BTreeSet<String>,
    /// Symbols any definition selects unconditionally.
    pub(crate) selects: BTreeSet<String>,
    definitions: usize,
}

/// Where a symbol is defined, for diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Location {
    pub(crate) file: String,
    pub(crate) line: usize,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct KconfigModel {
    pub(crate) symbols: BTreeMap<String, SymbolFacts>,
    pub(crate) locations: BTreeMap<String, Location>,
}

impl KconfigModel {
    /// Parses `<root>/Kconfig` with xconfig and extracts implication facts.
    pub(crate) fn load(root: &Path) -> Result<Self> {
        let file = Parser::new(root.join("Kconfig"), root)?.parse()?;
        let mut model = Self::default();
        let mut sources = vec!["Kconfig".to_string()];
        model.walk(&file.entries, &mut Vec::new(), &mut sources);
        sources.sort();
        sources.dedup();
        for source in sources {
            model.index_locations(root, &source);
        }
        Ok(model)
    }

    fn walk<'a>(
        &mut self,
        entries: &'a [Entry],
        inherited: &mut Vec<&'a Expr>,
        sources: &mut Vec<String>,
    ) {
        for entry in entries {
            match entry {
                Entry::Config(config) => {
                    self.define(
                        &config.name,
                        &config.symbol_type,
                        &config.properties,
                        inherited,
                    );
                }
                Entry::MenuConfig(config) => {
                    self.define(
                        &config.name,
                        &config.symbol_type,
                        &config.properties,
                        inherited,
                    );
                }
                Entry::Choice(choice) => {
                    let pushed = push_condition(inherited, choice.depends.as_ref());
                    for option in &choice.options {
                        self.define(
                            &option.name,
                            &option.symbol_type,
                            &option.properties,
                            inherited,
                        );
                    }
                    inherited.truncate(inherited.len() - usize::from(pushed));
                }
                Entry::Menu(menu) => {
                    let pushed = push_condition(inherited, menu.depends.as_ref());
                    self.walk(&menu.entries, inherited, sources);
                    inherited.truncate(inherited.len() - usize::from(pushed));
                }
                Entry::If(block) => {
                    inherited.push(&block.condition);
                    self.walk(&block.entries, inherited, sources);
                    inherited.pop();
                }
                Entry::Source(source) => {
                    sources.push(source.path.to_string_lossy().replace('\\', "/"));
                }
                Entry::Comment(_) | Entry::MainMenu(_) => {}
            }
        }
    }

    fn define(
        &mut self,
        name: &str,
        symbol_type: &SymbolType,
        properties: &Property,
        inherited: &[&Expr],
    ) {
        let mut requires = BTreeSet::new();
        for condition in inherited.iter().copied().chain(properties.depends.as_ref()) {
            positive_conjuncts(condition, &mut requires);
        }
        let facts = self.symbols.entry(name.to_string()).or_default();
        facts.is_bool |= matches!(symbol_type, SymbolType::Bool | SymbolType::Tristate);
        // Repeated definitions are alternatives: only shared requirements hold.
        facts.requires = if facts.definitions == 0 {
            requires
        } else {
            facts.requires.intersection(&requires).cloned().collect()
        };
        facts.definitions += 1;
        facts.selects.extend(
            properties
                .select
                .iter()
                .filter(|(_, condition)| condition.is_none())
                .map(|(target, _)| target.clone()),
        );
    }

    fn index_locations(&mut self, root: &Path, relative: &str) {
        let Ok(text) = fs::read_to_string(root.join(relative)) else {
            return;
        };
        for (index, line) in text.lines().enumerate() {
            let mut words = line.split_whitespace();
            if matches!(words.next(), Some("config" | "menuconfig"))
                && let Some(name) = words.next()
            {
                self.locations
                    .entry(name.to_string())
                    .or_insert_with(|| Location {
                        file: relative.to_string(),
                        line: index + 1,
                    });
            }
        }
    }
}

impl KconfigModel {
    /// Symbols enabled whenever `symbol` is enabled (excluding itself).
    pub(crate) fn implied_by(&self, symbol: &str) -> BTreeSet<String> {
        let mut implied = BTreeSet::new();
        let mut pending: Vec<&str> = vec![symbol];
        while let Some(name) = pending.pop() {
            let Some(facts) = self.symbols.get(name) else {
                continue;
            };
            for next in facts.requires.iter().chain(&facts.selects) {
                if next != symbol && implied.insert(next.clone()) {
                    pending.push(next);
                }
            }
        }
        implied
    }

    /// Bool/tristate `KFEAT_*` symbols that map to a kfeat feature.
    pub(crate) fn feature_symbols(&self) -> impl Iterator<Item = (&String, String)> {
        self.symbols
            .iter()
            .filter(|(name, facts)| facts.is_bool && name.starts_with("KFEAT_"))
            .filter_map(|(name, _)| cargo_feature_for_symbol(name).map(|feature| (name, feature)))
    }

    /// kfeat feature name -> bool symbols that select it through the same
    /// mapping `xkmake build` uses (`KFEAT_*`, `PLATFORM_*`, `MACHINE_<ARCH>_*`).
    pub(crate) fn feature_map(&self) -> BTreeMap<String, BTreeSet<String>> {
        let mut map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (name, facts) in &self.symbols {
            if facts.is_bool
                && let Some(feature) = cargo_feature_for_symbol(name)
            {
                map.entry(feature).or_default().insert(name.clone());
            }
        }
        map
    }
}

fn push_condition<'a>(inherited: &mut Vec<&'a Expr>, condition: Option<&'a Expr>) -> bool {
    condition
        .map(|condition| inherited.push(condition))
        .is_some()
}

/// Collects symbols that must be enabled for `expr` to hold.
fn positive_conjuncts(expr: &Expr, out: &mut BTreeSet<String>) {
    match expr {
        Expr::Symbol(name) if !matches!(name.as_str(), "y" | "m" | "n") => {
            out.insert(name.clone());
        }
        Expr::And(left, right) => {
            positive_conjuncts(left, out);
            positive_conjuncts(right, out);
        }
        Expr::Equal(left, right) => match (left.as_ref(), right.as_ref()) {
            (Expr::Symbol(name), value) | (value, Expr::Symbol(name))
                if is_yes(value) && name != "y" =>
            {
                out.insert(name.clone());
            }
            _ => {}
        },
        _ => {}
    }
}

fn is_yes(expr: &Expr) -> bool {
    matches!(expr, Expr::Symbol(value) | Expr::Const(value) if value == "y")
}
