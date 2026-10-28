// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Findings of repository rule checks and their text/JSON output.

use serde::Serialize;

use crate::{
    cli::ReportFormat,
    error::{Error, Result},
};

/// How a finding affects the exit status.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Severity {
    /// A rule violation: exit status 1 unless an exception matches it.
    Error,
    /// Informational, printed only with `--report`.
    Report,
}

impl Severity {
    const fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Report => "report",
        }
    }
}

/// One machine-readable finding.
///
/// `(rule, check, subject)` identifies the finding for exceptions; `subject`
/// therefore never contains a line number.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct Finding {
    /// Full rule id, e.g. `CG-051-test-source-layout`.
    pub(crate) rule: &'static str,
    /// Check name from the rule file's `backend: xkmake` lint list.
    pub(crate) check: &'static str,
    pub(crate) severity: Severity,
    /// Path relative to the repository root.
    pub(crate) file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) line: Option<usize>,
    pub(crate) subject: String,
    pub(crate) message: String,
}

impl Finding {
    /// An error-severity finding.
    pub(crate) fn new(
        rule: &'static str,
        check: &'static str,
        file: impl Into<String>,
        line: Option<usize>,
        subject: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            rule,
            check,
            severity: Severity::Error,
            file: file.into(),
            line,
            subject: subject.into(),
            message: message.into(),
        }
    }
}

/// Number of `findings` with `severity`.
pub(crate) fn count(findings: &[Finding], severity: Severity) -> usize {
    findings
        .iter()
        .filter(|finding| finding.severity == severity)
        .count()
}

/// Prints findings to stdout; report-severity findings only when `report`.
pub(crate) fn print_findings(
    findings: &[Finding],
    format: ReportFormat,
    report: bool,
) -> Result<()> {
    let shown: Vec<&Finding> = findings
        .iter()
        .filter(|finding| report || finding.severity != Severity::Report)
        .collect();
    match format {
        ReportFormat::Json => {
            let json = serde_json::to_string_pretty(&shown).map_err(|error| {
                Error::Environment(Box::new(Error::Message(format!(
                    "failed to serialize findings: {error}"
                ))))
            })?;
            println!("{json}");
        }
        ReportFormat::Text => print_text(&shown),
    }
    Ok(())
}

/// Prints findings in a rustc-like layout.
fn print_text(findings: &[&Finding]) {
    for finding in findings {
        println!(
            "{}[{}/{}]: {}",
            finding.severity.label(),
            finding.rule,
            finding.check,
            finding.subject
        );
        match finding.line {
            Some(line) => println!("  --> {}:{line}", finding.file),
            None => println!("  --> {}", finding.file),
        }
        println!("  = {}", finding.message);
    }
}
