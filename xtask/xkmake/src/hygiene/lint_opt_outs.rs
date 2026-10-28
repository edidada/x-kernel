// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CG-024 lint exceptions: inner lint opt-outs are registered.
//!
//! An inner `#![allow(..)]` or `#![expect(..)]`, also inside `cfg_attr`,
//! switches a lint off for a whole crate or module. When a rule file lists the
//! lint under a `rustc`, `clippy` or `dylint` check, the opt-out needs an entry
//! in `exceptions.toml`. Each file and opted-out rule lint is one finding with
//! the subject `<path>:<lint as written>`; lints no rule owns are ignored.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
};

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use syn::{Expr, ExprLit, Lit, Meta, Token, punctuated::Punctuated};

use super::{
    Context, Finding, RuleCheck,
    exceptions::EXCEPTIONS_FILE,
    rule_files::{self, RuleFile},
};
use crate::error::{IoResultExt, Result};

pub(crate) const RULE_ID: &str = "CG-024-lint-exceptions";

const INNER_OPT_OUT: &str = "inner-opt-out";

const CHECKS: [&str; 1] = [INNER_OPT_OUT];

/// Rule file backends whose lints attributes name, with the attribute prefix.
const LINT_BACKENDS: [(&str, &str); 3] = [("rustc", ""), ("clippy", "clippy::"), ("dylint", "")];

/// The CG-024 rule check.
pub(crate) struct LintOptOuts;

impl RuleCheck for LintOptOuts {
    fn rule(&self) -> &'static str {
        RULE_ID
    }

    fn checks(&self) -> &'static [&'static str] {
        &CHECKS
    }

    fn run(&self, context: &Context) -> Result<Vec<Finding>> {
        let owners = rule_lints(&rule_files::load(context.root())?);
        let mut findings = Vec::new();
        for file in context.member_rust_files()? {
            let path = context.root().join(file);
            let content = fs::read_to_string(&path).with_path(&path)?;
            findings.extend(collect_opt_outs(file, &content, &owners));
        }
        findings.sort();
        Ok(findings)
    }
}

/// Rule ids owning each lint, keyed by the name an attribute uses.
fn rule_lints(rules: &[(String, RuleFile)]) -> BTreeMap<String, Vec<String>> {
    let mut owners: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (_, rule) in rules {
        for (backend, prefix) in LINT_BACKENDS {
            for lint in rule.lints(backend) {
                owners
                    .entry(format!("{prefix}{lint}"))
                    .or_default()
                    .push(rule.id.clone());
            }
        }
    }
    owners
}

/// Findings for the inner opt-outs of rule lints in one source `file`
/// (repository-relative): one per lint, at its first opt-out.
fn collect_opt_outs(
    file: &str,
    content: &str,
    owners: &BTreeMap<String, Vec<String>>,
) -> Vec<Finding> {
    let mut seen = BTreeSet::new();
    let mut findings = Vec::new();
    for opt_out in inner_opt_outs(content) {
        let Some(rules) = owners.get(&opt_out.lint) else {
            continue;
        };
        if !seen.insert(opt_out.lint.clone()) {
            continue;
        }
        let reason = opt_out.reason.map_or_else(
            || "no reason".to_string(),
            |reason| format!("reason: {reason}"),
        );
        findings.push(Finding::new(
            RULE_ID,
            INNER_OPT_OUT,
            file,
            Some(opt_out.line),
            format!("{file}:{}", opt_out.lint),
            format!(
                "inner `{}({})` opts out of {} for the whole crate or module; register it in \
                 {EXCEPTIONS_FILE} ({reason})",
                opt_out.level,
                opt_out.lint,
                rules.join(", ")
            ),
        ));
    }
    findings
}

/// One lint named by an inner `allow` or `expect` attribute.
#[derive(Debug, Eq, PartialEq)]
struct OptOut {
    /// 1-based line of the attribute.
    line: usize,
    level: &'static str,
    /// Lint path as written, e.g. `clippy::unwrap_used`.
    lint: String,
    reason: Option<String>,
}

/// Inner `allow`/`expect` lints of `content` in source order, including those
/// under `cfg_attr` and in nested modules or functions. Source that does not
/// tokenize has none: rustc rejects it, so it cannot opt out of a lint.
fn inner_opt_outs(content: &str) -> Vec<OptOut> {
    // A level identifier must occur literally; skip files without one.
    if !(content.contains("allow") || content.contains("expect")) {
        return Vec::new();
    }
    let Ok(tokens) = content.parse::<TokenStream>() else {
        return Vec::new();
    };
    let mut opt_outs = Vec::new();
    collect_inner_attributes(tokens, &mut opt_outs);
    opt_outs
}

fn collect_inner_attributes(tokens: TokenStream, opt_outs: &mut Vec<OptOut>) {
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    for (index, token) in tokens.iter().enumerate() {
        if is_punct(token, '#')
            && tokens
                .get(index + 1)
                .is_some_and(|next| is_punct(next, '!'))
            && let Some(TokenTree::Group(group)) = tokens.get(index + 2)
            && group.delimiter() == Delimiter::Bracket
            && let Ok(meta) = syn::parse2::<Meta>(group.stream())
        {
            collect_lint_levels(&meta, group.span().start().line, opt_outs);
        }
        if let TokenTree::Group(group) = token {
            collect_inner_attributes(group.stream(), opt_outs);
        }
    }
}

fn is_punct(token: &TokenTree, ch: char) -> bool {
    matches!(token, TokenTree::Punct(punct) if punct.as_char() == ch)
}

/// Lints of an `allow` or `expect` meta, looking through `cfg_attr`.
fn collect_lint_levels(meta: &Meta, line: usize, opt_outs: &mut Vec<OptOut>) {
    let Meta::List(list) = meta else {
        return;
    };
    let Ok(arguments) = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
    else {
        return;
    };
    if list.path.is_ident("cfg_attr") {
        for attribute in arguments.iter().skip(1) {
            collect_lint_levels(attribute, line, opt_outs);
        }
        return;
    }
    let level = if list.path.is_ident("allow") {
        "allow"
    } else if list.path.is_ident("expect") {
        "expect"
    } else {
        return;
    };
    let reason = arguments.iter().find_map(|argument| match argument {
        Meta::NameValue(pair) if pair.path.is_ident("reason") => match &pair.value {
            Expr::Lit(ExprLit {
                lit: Lit::Str(reason),
                ..
            }) => Some(reason.value()),
            _ => None,
        },
        _ => None,
    });
    for argument in &arguments {
        if let Meta::Path(path) = argument {
            let lint = path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>()
                .join("::");
            opt_outs.push(OptOut {
                line,
                level,
                lint,
                reason: reason.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{OptOut, collect_opt_outs, inner_opt_outs, rule_lints};
    use crate::hygiene::rule_files::parse;

    fn owners() -> BTreeMap<String, Vec<String>> {
        let rules = [
            "---\nid: CG-021-x\nchecks:\n  - backend: clippy\n    lint: [panic, unwrap_used]\n  - \
             backend: ai\n---\n",
            "---\nid: CG-025-y\nchecks:\n  - backend: rustc\n    lint: \
             [unsafe_op_in_unsafe_fn]\n---\n",
            "---\nid: CG-001-z\nchecks:\n  - backend: dylint\n    lint: [static_spin]\n  - \
             backend: xkmake\n    lint: [layout]\n---\n",
        ];
        rule_lints(
            &rules
                .iter()
                .map(|text| (String::new(), parse(text).unwrap()))
                .collect::<Vec<_>>(),
        )
    }

    fn lints(content: &str) -> Vec<(usize, &'static str, String, Option<String>)> {
        inner_opt_outs(content)
            .into_iter()
            .map(
                |OptOut {
                     line,
                     level,
                     lint,
                     reason,
                 }| (line, level, lint, reason),
            )
            .collect()
    }

    #[test]
    fn rule_lints_use_attribute_names_of_lint_backends_only() {
        let owners = owners();
        assert_eq!(
            owners.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "clippy::panic",
                "clippy::unwrap_used",
                "static_spin",
                "unsafe_op_in_unsafe_fn"
            ]
        );
        assert_eq!(owners["clippy::panic"], ["CG-021-x"]);
    }

    #[test]
    fn finds_inner_levels_with_their_reasons() {
        let source =
            "//! Crate docs mention #![allow(clippy::panic)].\n#![no_std]\n#![allow(\n    \
             clippy::unwrap_used,\n    reason = \"CG-021-x: case (b): \\\n              \
             legacy\"\n)]\n#![cfg_attr(not(test), expect(unsafe_op_in_unsafe_fn, \
             dead_code))]\n#[allow(clippy::panic)]\nfn outer() {}\nmod inner {\n    \
             #![allow(missing_docs)]\n}\nconst S: &str = \"#![allow(clippy::panic)]\";\n";
        assert_eq!(
            lints(source),
            [
                (
                    3,
                    "allow",
                    "clippy::unwrap_used".to_string(),
                    Some("CG-021-x: case (b): legacy".to_string())
                ),
                (8, "expect", "unsafe_op_in_unsafe_fn".to_string(), None),
                (8, "expect", "dead_code".to_string(), None),
                (12, "allow", "missing_docs".to_string(), None),
            ]
        );
        assert!(lints("fn f() {}\n").is_empty());
        assert!(lints("#![allow(clippy::panic)]\nfn unfinished( {\n").is_empty());
    }

    #[test]
    fn reports_each_rule_lint_once_per_file() {
        let source = "#![allow(clippy::panic, missing_docs)]\nmod a {\n    #![cfg_attr(test, \
                      allow(clippy::panic))]\n    #![expect(clippy::identity_op, \
                      static_spin)]\n}\n";
        let findings = collect_opt_outs("k/src/lib.rs", source, &owners());
        assert_eq!(
            findings
                .iter()
                .map(|finding| (finding.check, finding.subject.as_str(), finding.line))
                .collect::<Vec<_>>(),
            [
                ("inner-opt-out", "k/src/lib.rs:clippy::panic", Some(1)),
                ("inner-opt-out", "k/src/lib.rs:static_spin", Some(4)),
            ]
        );
        assert!(findings[0].message.contains("CG-021-x"));
        assert!(findings[0].message.contains("no reason"));
    }
}
