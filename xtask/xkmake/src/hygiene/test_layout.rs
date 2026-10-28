// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CG-051 test source layout: tests stay out of production sources.
//!
//! Subjects are stable across edits so they can be registered as exceptions:
//! file-level checks use the repository-relative path; item-level checks use
//! `<path>:<item>` (the test function or module name), or the path when the
//! item has no name (an inner attribute, unparsable source).

use std::{fs, path::Path};

use proc_macro2::{Delimiter, Group, TokenStream, TokenTree};

use super::{Context, Finding, RuleCheck};
use crate::error::{IoResultExt, Result};

pub(crate) const RULE_ID: &str = "CG-051-test-source-layout";

const SRC_TEST_FILE: &str = "src-test-file";
const INLINE_TEST_MODULE: &str = "inline-test-module";
const SRC_TEST_FUNCTION: &str = "src-test-function";
const LEGACY_HOST_TESTS_DIR: &str = "legacy-host-tests-dir";
const UT_TESTS_HOST_ATTR: &str = "ut-tests-host-attr";
const TESTS_GUEST_MARKER: &str = "tests-guest-marker";
const TEST_EXPECT_ATTR: &str = "test-expect-attr";

const CHECKS: [&str; 7] = [
    SRC_TEST_FILE,
    INLINE_TEST_MODULE,
    SRC_TEST_FUNCTION,
    LEGACY_HOST_TESTS_DIR,
    UT_TESTS_HOST_ATTR,
    TESTS_GUEST_MARKER,
    TEST_EXPECT_ATTR,
];

/// The CG-051 rule check.
pub(crate) struct TestSourceLayout;

impl RuleCheck for TestSourceLayout {
    fn rule(&self) -> &'static str {
        RULE_ID
    }

    fn checks(&self) -> &'static [&'static str] {
        &CHECKS
    }

    fn run(&self, context: &Context) -> Result<Vec<Finding>> {
        let workspace = context.workspace()?;
        let rust_files: Vec<&String> = context
            .files()?
            .iter()
            .filter(|file| file.ends_with(".rs"))
            .collect();
        let mut findings = Vec::new();
        for package in workspace.packages.values() {
            let crate_dir = workspace.relative(package.directory());
            for file in &rust_files {
                let relative = if crate_dir.is_empty() {
                    file.as_str()
                } else {
                    match file.strip_prefix(&format!("{crate_dir}/")) {
                        Some(relative) => relative,
                        None => continue,
                    }
                };
                let path = context.root().join(file.as_str());
                let content = fs::read_to_string(&path).with_path(&path)?;
                findings.extend(collect_violations(file, relative, &content));
            }
        }
        // A file under nested member crates is visited once per enclosing crate.
        findings.sort();
        findings.dedup();
        Ok(findings)
    }
}

/// A violating item: 1-based line and item name, when known.
#[derive(Debug, Default, Eq, PartialEq)]
struct Site {
    line: Option<usize>,
    name: Option<String>,
}

/// Findings for one source `file` (repository-relative) whose path inside its
/// crate is `relative`.
fn collect_violations(file: &str, relative: &str, content: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut push = |check: &'static str, site: Site, message: &str| {
        let subject = match &site.name {
            Some(name) => format!("{file}:{name}"),
            None => file.to_string(),
        };
        findings.push(Finding::new(
            RULE_ID, check, file, site.line, subject, message,
        ));
    };
    let is_src = relative.starts_with("src/");
    let is_src_test = is_src && is_src_test_file(Path::new(relative));
    if relative.starts_with("host-tests/") {
        push(
            LEGACY_HOST_TESTS_DIR,
            Site::default(),
            "host tests must live in <crate>/tests/",
        );
    }
    if is_src_test {
        push(
            SRC_TEST_FILE,
            Site::default(),
            "guest unit tests must live in <crate>/ut-tests/",
        );
    }
    if is_src {
        for site in inline_test_modules(content) {
            push(
                INLINE_TEST_MODULE,
                site,
                "use unittest::path_mod_test! with files under ut-tests/",
            );
        }
    }
    if is_src && !is_src_test {
        for site in test_attribute_sites(content, &["test", "def_test"]) {
            push(
                SRC_TEST_FUNCTION,
                site,
                "test functions must live in tests/ (host) or ut-tests/ (guest)",
            );
        }
    }
    if relative.starts_with("ut-tests/") {
        for site in test_attribute_sites(content, &["test"]) {
            push(
                UT_TESTS_HOST_ATTR,
                site,
                "ut-tests must use #[def_test], not #[test]",
            );
        }
    }
    if relative.starts_with("tests/") {
        for site in test_attribute_sites(content, &["def_test"]) {
            push(
                TESTS_GUEST_MARKER,
                site,
                "Cargo tests must use #[test], not unittest #[def_test]",
            );
        }
    }
    if relative.starts_with("ut-tests/") || relative.starts_with("tests/") {
        for site in handwritten_panic_expect_sites(content) {
            push(
                TEST_EXPECT_ATTR,
                site,
                "test code must not hand-write unwrap/expect/panic #[expect]; rely on the test \
                 harness lint policy",
            );
        }
    }
    findings
}

fn is_src_test_file(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    file_name == "tests.rs" || file_name.starts_with("test_") || file_name.ends_with("_tests.rs")
}

/// Inline test modules: `mod` items under a test cfg or `mod_test` marker.
///
/// Matching runs on the source with whitespace removed, so each kept byte
/// records its original line.
fn inline_test_modules(content: &str) -> Vec<Site> {
    let mut compact = String::with_capacity(content.len());
    let mut lines = Vec::with_capacity(content.len());
    let mut line = 1;
    for ch in content.chars() {
        if ch == '\n' {
            line += 1;
        }
        if !ch.is_whitespace() {
            compact.push(ch);
            lines.extend(std::iter::repeat_n(line, ch.len_utf8()));
        }
    }
    let mut sites = Vec::new();
    for marker in [
        "#[cfg(test)]",
        "#[cfg(unittest)]",
        "#[unittest::mod_test]",
        "#[mod_test]",
    ] {
        for (offset, _) in compact.match_indices(marker) {
            let item = &compact[offset + marker.len()..];
            let has_visibility = item.starts_with("pub");
            let item = if let Some(visible) = item.strip_prefix("pub(") {
                visible.split_once(')').map_or("", |(_, rest)| rest)
            } else {
                item.strip_prefix("pub").unwrap_or(item)
            };
            let is_test_module = item.starts_with("mod")
                && (!has_visibility
                    || marker.contains("mod_test")
                    || item.find('{').is_some_and(|body| {
                        item.find(';').is_none_or(|declaration| body < declaration)
                    }));
            if !is_test_module {
                continue;
            }
            let name: String = item["mod".len()..]
                .chars()
                .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
                .collect();
            sites.push(Site {
                line: lines.get(offset).copied(),
                name: (!name.is_empty()).then_some(name),
            });
        }
    }
    sites.sort_by_key(|site| site.line);
    sites
}

/// Items carrying a `#[<name>]` attribute whose last path segment is one of
/// `names`, also when nested in `cfg_attr`.
fn test_attribute_sites(content: &str, names: &[&str]) -> Vec<Site> {
    // An attribute identifier must occur literally even inside cfg_attr. This
    // only rejects impossible matches; tokens still decide all candidates.
    if !names.iter().any(|name| content.contains(name)) {
        return Vec::new();
    }
    // Tokenization excludes comment/string contents without trying to evaluate
    // platform cfgs. Keep malformed source conservative until rustc diagnoses it.
    match content.parse::<TokenStream>() {
        Ok(tokens) => {
            let mut sites = Vec::new();
            collect_attribute_sites(tokens, names, &mut sites);
            sites
        }
        Err(_) => content
            .lines()
            .enumerate()
            .filter(|(_, line)| names.iter().any(|name| line.contains(&format!("#[{name}"))))
            .map(|(index, _)| Site {
                line: Some(index + 1),
                name: None,
            })
            .collect(),
    }
}

fn collect_attribute_sites(tokens: TokenStream, names: &[&str], sites: &mut Vec<Site>) {
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    for (index, token) in tokens.iter().enumerate() {
        if is_punct(token, '#') {
            let is_inner = tokens
                .get(index + 1)
                .is_some_and(|next| is_punct(next, '!'));
            let group_index = index + 1 + usize::from(is_inner);
            if let Some(TokenTree::Group(group)) = tokens.get(group_index)
                && matches_attribute(group, names)
            {
                sites.push(Site {
                    line: Some(group.span().start().line),
                    name: if is_inner {
                        None
                    } else {
                        item_name(&tokens[group_index + 1..])
                    },
                });
            }
        }
        if let TokenTree::Group(group) = token {
            collect_attribute_sites(group.stream(), names, sites);
        }
    }
}

fn is_punct(token: &TokenTree, ch: char) -> bool {
    matches!(token, TokenTree::Punct(punct) if punct.as_char() == ch)
}

fn matches_meta(meta: &syn::Meta, names: &[&str]) -> bool {
    if meta
        .path()
        .segments
        .last()
        .is_some_and(|part| names.iter().any(|name| part.ident == *name))
    {
        return true;
    }
    let syn::Meta::List(list) = meta else {
        return false;
    };
    if !list.path.is_ident("cfg_attr") {
        return false;
    }
    list.parse_args_with(syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)
        .is_ok_and(|args| args.iter().skip(1).any(|attr| matches_meta(attr, names)))
}

fn matches_attribute(group: &Group, names: &[&str]) -> bool {
    group.delimiter() == Delimiter::Bracket
        && syn::parse2::<syn::Meta>(group.stream()).is_ok_and(|meta| matches_meta(&meta, names))
}

/// Name of the `fn` or `mod` item that follows an outer attribute, skipping
/// further attributes and qualifiers; `None` when the item has no such name.
fn item_name(tokens: &[TokenTree]) -> Option<String> {
    let mut index = 0;
    while let Some(token) = tokens.get(index) {
        match token {
            TokenTree::Punct(punct) if punct.as_char() == '#' => index += 2,
            TokenTree::Punct(punct) if punct.as_char() == ';' => return None,
            TokenTree::Group(group) if group.delimiter() == Delimiter::Brace => return None,
            TokenTree::Ident(ident) if ident == "fn" || ident == "mod" => {
                return match tokens.get(index + 1) {
                    Some(TokenTree::Ident(name)) => {
                        let name = name.to_string();
                        Some(name.strip_prefix("r#").unwrap_or(&name).to_string())
                    }
                    _ => None,
                };
            }
            _ => index += 1,
        }
    }
    None
}

/// Lines with a hand-written `#[expect]` for the panic lints that the test
/// harness lint policy already allows.
fn handwritten_panic_expect_sites(content: &str) -> Vec<Site> {
    let lines: Vec<&str> = content.lines().collect();
    let code = |line: &str| {
        line.split_once("//")
            .map_or(line, |(code, _comment)| code)
            .trim()
            .to_string()
    };
    let mut sites = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let attribute = code(line);
        let is_outer = attribute.starts_with("#[expect(");
        if !(is_outer || attribute.starts_with("#![expect("))
            || !["unwrap_used", "expect_used", "panic"]
                .iter()
                .any(|lint| attribute.contains(&format!("clippy::{lint}")))
        {
            continue;
        }
        let name = if is_outer {
            lines[index + 1..]
                .iter()
                .map(|line| code(line))
                .find(|line| !line.is_empty() && !line.starts_with('#'))
                .and_then(|line| line_item_name(&line))
        } else {
            None
        };
        sites.push(Site {
            line: Some(index + 1),
            name,
        });
    }
    sites
}

/// Name after the first `fn` or `mod` keyword of one source line.
fn line_item_name(line: &str) -> Option<String> {
    let words: Vec<&str> = line
        .split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
        .filter(|word| !word.is_empty())
        .collect();
    let keyword = words
        .iter()
        .position(|word| *word == "fn" || *word == "mod")?;
    words.get(keyword + 1).map(|name| (*name).to_string())
}

#[cfg(test)]
mod test_support {
    //! Boolean views of the detectors and the pre-framework string form,
    //! so the detector tests read as plain predicates.

    use std::path::Path;

    use super::{
        collect_violations, handwritten_panic_expect_sites, inline_test_modules,
        test_attribute_sites,
    };

    pub(super) fn has_test_attribute(content: &str, names: &[&str]) -> bool {
        !test_attribute_sites(content, names).is_empty()
    }

    pub(super) fn has_host_test_attr(content: &str) -> bool {
        has_test_attribute(content, &["test"])
    }

    pub(super) fn has_guest_test_marker(content: &str) -> bool {
        has_test_attribute(content, &["def_test"])
    }

    pub(super) fn has_inline_test_module(content: &str) -> bool {
        !inline_test_modules(content).is_empty()
    }

    pub(super) fn has_handwritten_panic_expect_attr(content: &str) -> bool {
        !handwritten_panic_expect_sites(content).is_empty()
    }

    /// `<check>: <message>` for every finding of `relative` in crate `crate_dir`.
    pub(super) fn collect_test_layout_violations(
        crate_dir: &str,
        relative: &Path,
        content: &str,
        violations: &mut Vec<String>,
    ) {
        let relative = relative.to_str().unwrap_or_default();
        let file = format!("{crate_dir}/{relative}");
        violations.extend(
            collect_violations(&file, relative, content)
                .into_iter()
                .map(|finding| format!("{}: {}", finding.check, finding.message)),
        );
    }
}

#[cfg(test)]
mod test_attribute_tokens {
    use super::test_support::{
        collect_test_layout_violations, has_guest_test_marker, has_host_test_attr,
        has_test_attribute,
    };

    #[test]
    fn combined_scan_matches_individual_attribute_queries() {
        for source in [
            "fn ordinary() {}",
            "fn latest_value() {} // test is not an attribute",
            "#[test] fn host() {}",
            "#[unittest::def_test] fn guest() {}",
            "#[cfg_attr(any(), cfg_attr(all(), unittest::def_test))] fn guest() {}",
            "#[test] fn host() {} #[unittest::def_test] fn guest() {}",
            "#[cfg_attr(test, inline)] fn ordinary() {}",
            "const DOC: &str = r#\"#[test] #[def_test]\"#;",
            "/* #[test] */ fn ordinary() {}",
            "#[test] fn unfinished( {",
        ] {
            assert_eq!(
                has_test_attribute(source, &["test", "def_test"]),
                has_host_test_attr(source) || has_guest_test_marker(source),
                "{source}",
            );
        }
    }

    #[test]
    fn top_level_test_functions_cannot_hide_in_production_sources() {
        for source in [
            "#[unittest::def_test(user)] fn example() {}",
            "#[cfg(any())] #[test] fn example() {}",
        ] {
            let mut violations = Vec::new();
            collect_test_layout_violations(
                "demo",
                "src/ordinary.rs".as_ref(),
                source,
                &mut violations,
            );
            assert_eq!(violations.len(), 1, "{source}");
        }
        let mut violations = Vec::new();
        collect_test_layout_violations(
            "demo",
            "src/ordinary.rs".as_ref(),
            "//! #[def_test] example\nfn ordinary() {}",
            &mut violations,
        );
        assert!(violations.is_empty());
    }

    #[test]
    fn comments_and_strings_do_not_register_tests() {
        for source in [
            "//! In-crate `#[def_test]` cases exercise transitions.",
            "/* outer /* #[test] */ #[unittest::def_test] */",
            "const DOC: &str = r###\"#[test] #[def_test]\"###;",
            "#[doc = \"#[test] unittest::def_test\"] fn ordinary() {}",
        ] {
            assert!(!has_guest_test_marker(source), "{source}");
            assert!(!has_host_test_attr(source), "{source}");
        }
    }

    #[test]
    fn attributes_survive_spacing_comments_and_disabled_cfgs() {
        for source in [
            "#[def_test] fn example() {}",
            "# [ unittest /* separator */ :: def_test (user, serial) ] fn example() {}",
            "#[cfg(any())] mod hidden { #[unittest::def_test] fn example() {} }",
            "macro_rules! cases { () => { #[def_test] fn example() {} }; }",
            "#[cfg_attr(any(), unittest::def_test(user))] fn example() {}",
            "#[cfg_attr(any(), cfg_attr(all(), unittest::def_test))] fn example() {}",
        ] {
            assert!(has_guest_test_marker(source), "{source}");
        }
        assert!(has_host_test_attr(
            "# /* comment */ [ test ] fn example() {}"
        ));
        assert!(!has_host_test_attr("#[test_case] fn example() {}"));
        assert!(!has_guest_test_marker("#[def_test_helper] fn example() {}"));
        assert!(has_host_test_attr(
            "#[cfg_attr(test, test)] fn example() {}"
        ));
        assert!(!has_guest_test_marker(
            "#[cfg_attr(any(), doc = \"#[def_test]\")] fn ordinary() {}"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        collect_violations, is_src_test_file,
        test_support::{
            collect_test_layout_violations, has_handwritten_panic_expect_attr,
            has_inline_test_module,
        },
    };

    /// `(check, subject, line)` of every finding.
    fn located(
        file: &str,
        relative: &str,
        content: &str,
    ) -> Vec<(&'static str, String, Option<usize>)> {
        collect_violations(file, relative, content)
            .into_iter()
            .map(|finding| (finding.check, finding.subject, finding.line))
            .collect()
    }

    #[test]
    fn findings_name_their_check_and_a_line_free_subject() {
        assert_eq!(
            located("k/host-tests/a.rs", "host-tests/a.rs", "fn a() {}"),
            [(
                "legacy-host-tests-dir",
                "k/host-tests/a.rs".to_string(),
                None
            )]
        );
        assert_eq!(
            located(
                "k/src/copy_tests.rs",
                "src/copy_tests.rs",
                "#[def_test]\nfn copies() {}"
            ),
            [("src-test-file", "k/src/copy_tests.rs".to_string(), None)]
        );
        assert_eq!(
            located(
                "k/src/lib.rs",
                "src/lib.rs",
                "fn a() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn works() {}\n}\n",
            ),
            [
                (
                    "inline-test-module",
                    "k/src/lib.rs:tests".to_string(),
                    Some(2)
                ),
                (
                    "src-test-function",
                    "k/src/lib.rs:works".to_string(),
                    Some(4)
                ),
            ]
        );
        assert_eq!(
            located(
                "k/ut-tests/a.rs",
                "ut-tests/a.rs",
                "#[test]\n#[cfg(any())]\npub(crate) async fn first() {}\n#[test] fn r#second() {}",
            ),
            [
                (
                    "ut-tests-host-attr",
                    "k/ut-tests/a.rs:first".to_string(),
                    Some(1)
                ),
                (
                    "ut-tests-host-attr",
                    "k/ut-tests/a.rs:second".to_string(),
                    Some(4)
                ),
            ]
        );
        assert_eq!(
            located(
                "k/tests/a.rs",
                "tests/a.rs",
                "\n#[unittest::def_test(user)]\nfn guest() {}"
            ),
            [(
                "tests-guest-marker",
                "k/tests/a.rs:guest".to_string(),
                Some(2)
            )]
        );
        assert_eq!(
            located(
                "k/tests/a.rs",
                "tests/a.rs",
                "#![expect(clippy::panic, reason = \"x\")]\n#[expect(clippy::unwrap_used, reason \
                 = \"x\")] // why\n#[inline]\npub fn helper() {}\n",
            ),
            [
                ("test-expect-attr", "k/tests/a.rs".to_string(), Some(1)),
                (
                    "test-expect-attr",
                    "k/tests/a.rs:helper".to_string(),
                    Some(2)
                ),
            ]
        );
    }

    #[test]
    fn unparsable_sources_fall_back_to_the_file_subject() {
        assert_eq!(
            located("k/src/a.rs", "src/a.rs", "#[test] fn unfinished( {"),
            [("src-test-function", "k/src/a.rs".to_string(), Some(1))]
        );
    }

    #[test]
    fn recognizes_src_test_file_names() {
        assert!(is_src_test_file("src/copy_tests.rs".as_ref()));
        assert!(is_src_test_file("src/tests.rs".as_ref()));
        assert!(is_src_test_file("src/foo/test_copy.rs".as_ref()));
        assert!(!is_src_test_file("src/contest.rs".as_ref()));
    }

    #[test]
    fn recognizes_inline_test_modules_without_layout_sensitive_paths() {
        assert!(has_inline_test_module("#[cfg(unittest)]\nmod copy_tests;"));
        assert!(has_inline_test_module("#[cfg(test)] mod tests { }"));
        assert!(has_inline_test_module("#[mod_test]\nmod tests { }"));
        assert!(has_inline_test_module(
            "#[mod_test] mod tests { include!(\"../ut-tests/lib.rs\"); }"
        ));
        assert!(!has_inline_test_module(
            "unittest::path_mod_test!(tests, \"../ut-tests/lib.rs\");"
        ));
    }

    #[test]
    fn production_includes_are_not_test_layout_evidence() {
        for source in [
            include_str!("../../../../util/kbuild_config/src/lib.rs"),
            r#"include!("generated_constants.rs");"#,
            r#"include!(concat!(env!("OUT_DIR"), "/bindings.rs"));"#,
            r#"// include!("example.rs") includes ordinary Rust code."#,
            r##"const EXAMPLE: &str = r#"include!("example.rs");"#;"##,
            "#[cfg(unittest)] pub mod ahci;",
        ] {
            let mut violations = Vec::new();
            collect_test_layout_violations("demo", "src/lib.rs".as_ref(), source, &mut violations);
            assert!(violations.is_empty(), "{source}: {violations:?}");
        }
    }

    #[test]
    fn test_module_includes_remain_layout_violations() {
        for attribute in [
            "#[cfg(test)]",
            "#[cfg(unittest)]",
            "#[mod_test]",
            "#[unittest::mod_test]",
        ] {
            for visibility in [
                "",
                "pub ",
                "pub(crate) ",
                "pub(super) ",
                "pub(in crate::task) ",
            ] {
                for include in [
                    r#"include!("../ut-tests/cases.rs");"#,
                    r#"include!(env!("TEST_CASES"));"#,
                ] {
                    let source = format!("{attribute}\n{visibility}mod tests {{\n{include}\n}}");
                    let mut violations = Vec::new();
                    collect_test_layout_violations(
                        "demo",
                        "src/lib.rs".as_ref(),
                        &source,
                        &mut violations,
                    );
                    assert_eq!(violations.len(), 1, "{source}: {violations:?}");
                }
            }
        }
    }

    #[test]
    fn reports_guest_tests_left_under_src() {
        let mut violations = Vec::new();
        collect_test_layout_violations(
            "core/kuaccess",
            "src/copy_tests.rs".as_ref(),
            "use unittest::def_test;\n#[def_test]\nfn copies() {}",
            &mut violations,
        );

        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("guest unit tests must live in <crate>/ut-tests/"));
    }

    #[test]
    fn reports_wrong_macro_for_each_test_directory() {
        let mut violations = Vec::new();
        collect_test_layout_violations(
            "core/demo",
            "ut-tests/host_style.rs".as_ref(),
            "#[test]\nfn host_style() {}",
            &mut violations,
        );
        collect_test_layout_violations(
            "core/demo",
            "tests/guest_style.rs".as_ref(),
            "use unittest::def_test;\n#[def_test]\nfn guest_style() {}",
            &mut violations,
        );

        assert_eq!(violations.len(), 2);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("ut-tests must use #[def_test]"))
        );
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("Cargo tests must use #[test]"))
        );
    }

    #[test]
    fn reports_handwritten_panic_expect_attrs_in_test_sources() {
        assert!(has_handwritten_panic_expect_attr(
            "#[expect(clippy::expect_used, reason = \"test scenario: setup\")]\nfn helper() {}"
        ));
        assert!(has_handwritten_panic_expect_attr(
            "#![expect(clippy::panic, reason = \"test scenario: setup\")]\nfn helper() {}"
        ));
        assert!(!has_handwritten_panic_expect_attr(
            "#[allow(clippy::unwrap_used, reason = \"test scenario: setup\")]\nfn helper() {}"
        ));
        assert!(!has_handwritten_panic_expect_attr(
            "// #[expect(clippy::expect_used, reason = \"old text\")]\nfn helper() {}"
        ));
        assert!(!has_handwritten_panic_expect_attr(
            "#[expect(\n    clippy::expect_used,\n    reason = \"test scenario: setup\"\n)]\nfn \
             helper() {}"
        ));

        let mut violations = Vec::new();
        collect_test_layout_violations(
            "drivers/integration/kdriver",
            "ut-tests/block_irq_unittest.rs".as_ref(),
            "#[expect(clippy::expect_used, reason = \"test scenario: setup\")]\nfn helper() {}",
            &mut violations,
        );
        collect_test_layout_violations(
            "util/kerrno",
            "tests/kerrno_host.rs".as_ref(),
            "#![expect(clippy::panic, reason = \"test scenario: setup\")]\nfn helper() {}",
            &mut violations,
        );

        assert_eq!(violations.len(), 2);
        assert!(violations.iter().all(|violation| {
            violation.contains("test code must not hand-write unwrap/expect/panic #[expect]")
        }));
    }
}
