// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Golden 回归(验证支柱 + Ch8 持续进化)。
//!
//! 把 walkthrough 声明的"用了哪些映射、报了哪些缺口"变成机器可查的断言。
//! 改了数据,walkthrough 依赖的映射若被删/改,`ldp golden` 立刻告诉你下游漂移了
//! (Ch6 PASS_TO_PASS + Ch8 文档/数据一致性)。

use serde::Deserialize;

use crate::data::{ApiMapping, GapSubsystem, Gaps};

/// 收集一个 gap 的所有 severity 取值(主值 + dimensions + severity_branches)。
fn gap_severities(g: &GapSubsystem) -> Vec<&str> {
    let mut v: Vec<&str> = vec![g.severity.as_str()];
    for d in &g.dimensions {
        v.push(d.severity.as_str());
    }
    if let Some(branches) = g.severity_branches.as_object() {
        for (_k, val) in branches {
            if let Some(sev) = val.get("severity").and_then(|x| x.as_str()) {
                v.push(sev);
            }
        }
    }
    v
}

#[derive(Debug, Deserialize)]
struct Golden {
    id: String,
    title: String,
    #[serde(default)]
    source_doc: String,
    #[serde(default)]
    expected_api_mappings: Vec<ExpectedMapping>,
    #[serde(default)]
    expected_gaps: Vec<ExpectedGap>,
    #[serde(default)]
    expected_verdict: String,
}

#[derive(Debug, Deserialize)]
struct ExpectedMapping {
    linux: String,
    xkernel_contains: String,
    #[serde(default)]
    source_substring: String,
}

#[derive(Debug, Deserialize)]
struct ExpectedGap {
    #[serde(default)]
    #[allow(dead_code)]
    id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    severity: String,
    #[serde(default)]
    #[allow(dead_code)]
    handling: String,
}

/// 跑单个 golden 基线。返回错误数。
pub fn run_golden(name: &str, api: &ApiMapping, gaps: &Gaps) -> usize {
    let path = crate::repo::golden_dir().join(format!("{name}.json"));
    let golden: Golden = match crate::repo::read_json(&path) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("✗ 读 golden `{name}` 失败: {e}");
            return 1;
        }
    };
    println!("══ golden: {} ({}) ══", golden.id, golden.title);
    println!("源文档: {}", golden.source_doc);
    println!("预期结论: {}", golden.expected_verdict);
    println!("────────────────────────────────────────");

    let mut errors = 0usize;

    // 1) 每条期望映射都必须能在 api-mapping.json 找到对应(FAIL_TO_PASS)。
    println!("[映射断言]");
    for em in &golden.expected_api_mappings {
        let found = api.sections.iter().any(|sec| {
            sec.rows.iter().any(|row| {
                let xk_hit = row.xkernel.contains(&em.xkernel_contains);
                let linux_hit = row.linux.contains(&em.linux)
                    || row
                        .keywords
                        .iter()
                        .any(|k| k.contains(&em.linux) || em.linux.contains(k));
                let src_hit = em.source_substring.is_empty()
                    || row
                        .source
                        .as_ref()
                        .map(|s| s.path.contains(&em.source_substring))
                        .unwrap_or(false);
                xk_hit && linux_hit && src_hit
            })
        });
        if found {
            println!("  ✓ {} → 含「{}」", em.linux, em.xkernel_contains);
        } else {
            errors += 1;
            eprintln!(
                "  ✗ {} → 找不到含「{}」的映射(walkthrough 可能引用了编造/已删的 API)",
                em.linux, em.xkernel_contains
            );
        }
    }

    // 2) 每条带标准 id 的期望缺口必须命中 gaps.json 且 severity 一致。
    println!("[缺口断言]");
    for eg in &golden.expected_gaps {
        if eg.id.is_empty() {
            println!("  ℹ 「{}」(本项目独有概念,无标准 id — 仅提示)", eg.label);
            continue;
        }
        match gaps.subsystems.iter().find(|g| g.id == eg.id) {
            Some(g) => {
                let all = gap_severities(g);
                if eg.severity.is_empty() || all.iter().any(|s| *s == eg.severity) {
                    println!("  ✓ {} [{}] severity={}", eg.label, g.id, eg.severity);
                } else {
                    errors += 1;
                    eprintln!(
                        "  ✗ {} [{}] severity 漂移: walkthrough 期望「{}」,gaps.json 实际 {}",
                        eg.label,
                        g.id,
                        eg.severity,
                        all.join("/")
                    );
                }
            }
            None => {
                errors += 1;
                eprintln!(
                    "  ✗ 「{}」(id={}) 在 gaps.json 中不存在(walkdown 与缺口库脱节)",
                    eg.label, eg.id
                );
            }
        }
    }

    println!("────────────────────────────────────────");
    if errors == 0 {
        println!("✓ golden `{name}` PASS — walkthrough 与当前数据一致。");
    } else {
        eprintln!("✗ golden `{name}` FAIL — {errors} 处不一致(见上)。");
    }
    errors
}

/// 列出所有 golden 基线名。
pub fn list_goldens() -> Vec<String> {
    let dir = crate::repo::golden_dir();
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            if e.path().extension().and_then(|x| x.to_str()) == Some("json")
                && let Some(stem) = e.path().file_stem().and_then(|s| s.to_str())
            {
                names.push(stem.to_string());
            }
        }
    }
    names.sort();
    names
}
