// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Accepted findings of rule checks, registered in
//! `docs/code-guidelines/exceptions.toml`:
//!
//! ```toml
//! [[exception]]
//! rule = "CG-051-test-source-layout"
//! check = "src-test-file"
//! subject = "drivers/kdemo/src/tests.rs"
//! reason = "CG-051-test-source-layout: TODO(cg): ..."
//! ```
//!
//! An entry suppresses the error findings with the same `(rule, check,
//! subject)`. After the rule id, its reason names the permitting case of the
//! rule's **Exceptions** and why it holds, or `TODO(cg): <planned repair>`.
//! Malformed entries are errors in every run; an entry that matches nothing
//! is stale, which only a run covering its check completely can tell.

use std::{collections::BTreeSet, fs, io, path::Path};

use serde::Deserialize;

use super::{
    registry::find_rule,
    report::{Finding, Severity},
};
use crate::error::{Error, Result};

/// Exception file, relative to the repository root.
pub(crate) const EXCEPTIONS_FILE: &str = "docs/code-guidelines/exceptions.toml";
/// Rule reported for an entry whose own rule is not registered.
const UNKNOWN_RULE: &str = "exceptions";
/// Check name of findings about exception entries themselves.
const EXCEPTION_CHECK: &str = "exception";

/// One registered exception.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Exception {
    pub(crate) rule: String,
    pub(crate) check: String,
    pub(crate) subject: String,
    pub(crate) reason: String,
    /// 1-based line of the entry's `[[exception]]` header.
    pub(crate) line: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExceptionFile {
    #[serde(default)]
    exception: Vec<toml::Spanned<RawException>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawException {
    rule: String,
    check: String,
    subject: String,
    reason: String,
}

/// Reads the exception file under `root`; a missing file has no entries.
pub(crate) fn load(root: &Path) -> Result<Vec<Exception>> {
    let path = root.join(EXCEPTIONS_FILE);
    match fs::read_to_string(&path) {
        Ok(text) => parse(&text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(source) => Err(Error::Io { path, source }),
    }
}

fn parse(text: &str) -> Result<Vec<Exception>> {
    let file: ExceptionFile = toml::from_str(text)
        .map_err(|error| Error::Message(format!("invalid {EXCEPTIONS_FILE}: {error}")))?;
    Ok(file
        .exception
        .into_iter()
        .map(|entry| {
            // The span of an array-of-tables element starts at its header.
            let line = text[..entry.span().start.min(text.len())]
                .matches('\n')
                .count()
                + 1;
            let entry = entry.into_inner();
            Exception {
                rule: entry.rule,
                check: entry.check,
                subject: entry.subject,
                reason: entry.reason,
                line,
            }
        })
        .collect())
}

/// Which exceptions a run can judge.
pub(crate) struct Scope<'a> {
    /// Whether the rule ran; entries of other rules are only format-checked
    /// when their rule is unknown.
    pub(crate) is_rule_selected: &'a dyn Fn(&str) -> bool,
    /// Whether `(rule, check)` ran over all of its inputs, so an unmatched
    /// entry is stale rather than merely out of scope.
    pub(crate) is_complete: &'a dyn Fn(&str, &str) -> bool,
}

/// Outcome of matching findings against exceptions.
#[derive(Debug, Default)]
pub(crate) struct Applied {
    /// Remaining findings plus problems of exception entries, sorted.
    pub(crate) kept: Vec<Finding>,
    pub(crate) suppressed: usize,
}

/// Drops excepted error findings and reports malformed, duplicate or stale
/// entries as error findings located in the exception file.
pub(crate) fn apply(
    findings: Vec<Finding>,
    exceptions: &[Exception],
    scope: &Scope<'_>,
) -> Applied {
    let mut applied = Applied::default();
    let mut matched = vec![false; exceptions.len()];
    for finding in findings {
        let position = (finding.severity == Severity::Error)
            .then(|| {
                exceptions.iter().position(|exception| {
                    exception.rule == finding.rule
                        && exception.check == finding.check
                        && exception.subject == finding.subject
                })
            })
            .flatten();
        match position {
            Some(index) => {
                matched[index] = true;
                applied.suppressed += 1;
            }
            None => applied.kept.push(finding),
        }
    }
    let mut seen = BTreeSet::new();
    for (exception, is_matched) in exceptions.iter().zip(matched) {
        let rule = find_rule(&exception.rule);
        if rule.is_some() && !(scope.is_rule_selected)(&exception.rule) {
            continue;
        }
        let problem = match rule {
            None => format!(
                "exception rule `{}` is not a registered xkmake rule",
                exception.rule
            ),
            Some(rule) if !rule.checks().contains(&exception.check.as_str()) => format!(
                "exception check `{}` is not one of {}",
                exception.check,
                rule.checks().join(", ")
            ),
            Some(_) => match reason_problem(&exception.reason, &exception.rule) {
                Some(problem) => problem,
                None if !seen.insert((&exception.rule, &exception.check, &exception.subject)) => {
                    "duplicate exception: an earlier entry has the same rule, check and subject"
                        .to_string()
                }
                None if !is_matched && (scope.is_complete)(&exception.rule, &exception.check) => {
                    "stale exception: no finding matches it any more; remove it".to_string()
                }
                None => continue,
            },
        };
        applied.kept.push(Finding::new(
            rule.map_or(UNKNOWN_RULE, |rule| rule.rule()),
            EXCEPTION_CHECK,
            EXCEPTIONS_FILE,
            Some(exception.line),
            format!("{}: {}", exception.check, exception.subject),
            problem,
        ));
    }
    applied.kept.sort();
    applied
}

/// Why `reason` is malformed, if it is. After the full rule id and a colon it
/// names the permitting **Exceptions** case or holds `TODO(cg): <repair>`;
/// neither part may be empty.
fn reason_problem(reason: &str, rule: &str) -> Option<String> {
    let Some(rest) = reason
        .strip_prefix(rule)
        .and_then(|rest| rest.strip_prefix(':'))
    else {
        return Some(format!("exception reason must start with `{rule}: `"));
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return Some(format!(
            "exception reason is empty after `{rule}: `; name the permitting Exceptions case and \
             why it holds, or `TODO(cg): <planned repair>`"
        ));
    }
    if rest
        .strip_prefix("TODO(cg):")
        .is_some_and(|repair| repair.trim().is_empty())
    {
        return Some(
            "exception reason is empty after `TODO(cg): `; state the planned repair".into(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{Exception, Finding, Scope, Severity, apply, parse, reason_problem};

    const RULE: &str = "CG-051-test-source-layout";

    fn entry(check: &str, subject: &str, reason: &str) -> Exception {
        Exception {
            rule: RULE.into(),
            check: check.into(),
            subject: subject.into(),
            reason: reason.into(),
            line: 1,
        }
    }

    fn problems(findings: &[Finding]) -> Vec<(&str, String)> {
        findings
            .iter()
            .filter(|finding| finding.check == "exception")
            .map(|finding| (finding.rule, finding.subject.clone()))
            .collect()
    }

    #[test]
    fn parses_entries_with_their_header_lines() {
        let text = "# header\n\n[[exception]]\nrule = \"r\"\ncheck = \"c\"\nsubject = \
                    \"s\"\nreason = \"r: x\"\n\n[[exception]]\nrule = \"r\"\ncheck = \
                    \"c\"\nsubject = \"t\"\nreason = \"r: y\"\n";
        let entries = parse(text).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| (entry.subject.as_str(), entry.line))
                .collect::<Vec<_>>(),
            [("s", 3), ("t", 9)]
        );
        assert!(parse("[[exception]]\nrule = \"r\"\n").is_err());
        assert!(
            parse(
                "[[exception]]\nrule = \"r\"\ncheck = \"c\"\nsubject = \"s\"\nreason = \"r: \
                 x\"\nlint = \"c\"\n"
            )
            .is_err()
        );
    }

    #[test]
    fn reasons_need_a_case_or_a_planned_repair_after_the_rule_id() {
        for reason in [
            format!("{RULE}: generated bindings: the generator owns the layout"),
            format!("{RULE}: TODO(cg): move the cases to ut-tests/"),
            format!("{RULE}: TODO(cg): decide ownership: kfeat or the crate"),
        ] {
            assert_eq!(reason_problem(&reason, RULE), None, "{reason}");
        }
        for reason in [
            String::new(),
            "TODO(cg): repair".to_string(),
            RULE.to_string(),
            format!("{RULE}:"),
            format!("{RULE}:   "),
            format!("{RULE}: TODO(cg):"),
            format!("{RULE}: TODO(cg): "),
        ] {
            assert!(reason_problem(&reason, RULE).is_some(), "{reason:?}");
        }
    }

    #[test]
    fn suppresses_errors_and_reports_bad_or_stale_entries() {
        let reason = format!("{RULE}: TODO(cg): legacy");
        let finding = Finding::new(RULE, "src-test-file", "a.rs", Some(1), "dma", "m");
        let report = Finding {
            severity: Severity::Report,
            ..Finding::new(RULE, "src-test-file", "a.rs", None, "net", "m")
        };
        let exceptions = vec![
            entry("src-test-file", "dma", &reason),
            entry("src-test-file", "gone", &reason),
            entry("src-test-file", "net", &reason),
            entry("unknown-check", "x", &reason),
            entry("test-expect-attr", "y", "no rule id"),
            entry("test-expect-attr", "z", &format!("{RULE}-extra: x")),
            entry("test-expect-attr", "e", &format!("{RULE}: ")),
            entry("test-expect-attr", "t", &format!("{RULE}: TODO(cg):  ")),
            entry("src-test-file", "dma", &reason),
            Exception {
                rule: "CG-999-unknown".into(),
                ..entry("src-test-file", "w", "CG-999-unknown: x")
            },
        ];
        let complete = Scope {
            is_rule_selected: &|_| true,
            is_complete: &|_, _| true,
        };
        let applied = apply(
            vec![finding.clone(), report.clone()],
            &exceptions,
            &complete,
        );
        assert_eq!(applied.suppressed, 1);
        // Report-severity findings are never excepted.
        assert!(applied.kept.contains(&report));
        assert_eq!(
            problems(&applied.kept),
            [
                (RULE, "src-test-file: dma".to_string()),
                (RULE, "src-test-file: gone".to_string()),
                (RULE, "src-test-file: net".to_string()),
                (RULE, "test-expect-attr: e".to_string()),
                (RULE, "test-expect-attr: t".to_string()),
                (RULE, "test-expect-attr: y".to_string()),
                (RULE, "test-expect-attr: z".to_string()),
                (RULE, "unknown-check: x".to_string()),
                ("exceptions", "src-test-file: w".to_string()),
            ]
        );

        // Partial runs judge format but not staleness.
        let partial = Scope {
            is_rule_selected: &|_| true,
            is_complete: &|_, _| false,
        };
        let applied = apply(vec![finding.clone()], &exceptions[..3], &partial);
        assert_eq!(applied.suppressed, 1);
        assert!(problems(&applied.kept).is_empty());

        // Entries of unselected rules are skipped entirely.
        let other_rule = Scope {
            is_rule_selected: &|rule| rule != RULE,
            is_complete: &|_, _| true,
        };
        let applied = apply(Vec::new(), &exceptions[..5], &other_rule);
        assert!(applied.kept.is_empty());
    }
}
