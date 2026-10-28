// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Frontmatter of the rule files `docs/code-guidelines/CG-*.md`.
//!
//! Only the fields the checks need are read: `id` and the `lint` list of each
//! `checks` item. The parser accepts the subset of YAML the rule files use;
//! `lint` lists are flow sequences, possibly spread over several lines.

use std::{fs, path::Path};

use crate::error::{Error, IoResultExt, Result};

/// Rule file directory, relative to the repository root.
pub(crate) const RULES_DIR: &str = "docs/code-guidelines";

/// The frontmatter fields of one rule file.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct RuleFile {
    /// Full rule id, e.g. `CG-021-no-panic-on-reachable-failure`.
    pub(crate) id: String,
    /// `(backend, lint names)` of each `checks` item, in file order.
    pub(crate) checks: Vec<(String, Vec<String>)>,
}

impl RuleFile {
    /// Lint names listed under `backend`.
    pub(crate) fn lints(&self, backend: &str) -> Vec<String> {
        self.checks
            .iter()
            .filter(|(name, _)| name == backend)
            .flat_map(|(_, lints)| lints.iter().cloned())
            .collect()
    }
}

/// Parses the frontmatter of one rule file; `None` without an `id`.
pub(crate) fn parse(text: &str) -> Option<RuleFile> {
    let frontmatter = text.strip_prefix("---\n")?.split("\n---").next()?;
    let mut id = None;
    let mut checks: Vec<(String, Vec<String>)> = Vec::new();
    let mut pending: Option<String> = None;
    for line in frontmatter.lines() {
        let trimmed = line.trim();
        if let Some(list) = pending.as_mut() {
            list.push_str(trimmed);
        } else if let Some(value) = trimmed.strip_prefix("id:") {
            id = Some(value.trim().to_string());
        } else if let Some(backend) = trimmed.strip_prefix("- backend:") {
            checks.push((backend.trim().to_string(), Vec::new()));
        } else if let Some(list) = trimmed.strip_prefix("lint:")
            && !checks.is_empty()
        {
            pending = Some(list.trim().to_string());
        }
        if let Some(list) = pending.take_if(|list| list.ends_with(']'))
            && let Some((_, lints)) = checks.last_mut()
        {
            let inner = list.trim_start_matches('[').trim_end_matches(']');
            lints.extend(
                inner
                    .split(',')
                    .map(|name| name.trim().trim_matches(['"', '\'']).to_string())
                    .filter(|name| !name.is_empty()),
            );
        }
    }
    Some(RuleFile { id: id?, checks })
}

/// Every `CG-*.md` rule file under `root`, as `(file name, frontmatter)` in
/// file name order. A rule file without an `id` is an error.
pub(crate) fn load(root: &Path) -> Result<Vec<(String, RuleFile)>> {
    let directory = root.join(RULES_DIR);
    let mut files = Vec::new();
    for entry in fs::read_dir(&directory).with_path(&directory)? {
        let path = entry.with_path(&directory)?.path();
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        if !(name.starts_with("CG-") && name.ends_with(".md")) {
            continue;
        }
        let text = fs::read_to_string(&path).with_path(&path)?;
        let rule = parse(&text).ok_or_else(|| {
            Error::Message(format!("{RULES_DIR}/{name}: no `id` in the frontmatter"))
        })?;
        files.push((name, rule));
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::{RuleFile, parse};

    #[test]
    fn reads_the_id_and_each_backend_lint_list() {
        let text = "---\nid: CG-1-x\nchecks:\n  - backend: clippy\n    lint: [panic]\n  - \
                    backend: xkmake\n    lint: [a, \"b\",\n      c]\n  - backend: ai\n    \
                    triggers:\n      - 'lint: [d]'\n---\n# body\nlint: [e]\n";
        let rule = parse(text).unwrap();
        assert_eq!(
            rule,
            RuleFile {
                id: "CG-1-x".into(),
                checks: vec![
                    ("clippy".into(), vec!["panic".into()]),
                    ("xkmake".into(), vec!["a".into(), "b".into(), "c".into()]),
                    ("ai".into(), Vec::new()),
                ],
            }
        );
        assert_eq!(rule.lints("xkmake"), ["a", "b", "c"]);
        assert!(rule.lints("rustc").is_empty());
        assert_eq!(parse("# no frontmatter\n"), None);
    }
}
