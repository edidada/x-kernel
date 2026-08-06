// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 查询类子命令:summary / lookup / gaps。
//!
//! 设计遵循 Ch4 工具规约:
//! - 返回**结构化候选**,不是全文倾倒(rule 2);
//! - **显式截断**:超过上限时打印 "showing N of M" 并提示用更窄查询(rule 3);
//! - 匹配用 keywords + linux + xkernel(Ch3 上下文前缀),不靠精确字面。

use crate::data::{ApiMapping, GapSubsystem, Gaps, MappingRow};

/// 最多展示的候选数(显式截断,rule 3)。
const LOOKUP_MAX: usize = 12;

/// 概览卡片(Ch3 双层记忆的"常驻概览"层)。
pub fn summary(api: &ApiMapping, gaps: &Gaps, bf_defs: usize, golden: usize) {
    let total_rows: usize = api.sections.iter().map(|s| s.rows.len()).sum();
    let status_count = |st: &str| -> usize {
        api.sections
            .iter()
            .flat_map(|s| s.rows.iter())
            .filter(|r| r.status == st)
            .count()
    };
    let gap_status = |st: &str| gaps.subsystems.iter().filter(|g| g.status == st).count();

    println!("linux-driver-port 知识库概览");
    println!("────────────────────────────────────────");
    println!("API 映射 : {} 条 / {} 区", total_rows, api.sections.len());
    println!(
        "  状态    : landed={}  partial/missing/workaround={}",
        status_count("landed"),
        total_rows - status_count("landed")
    );
    println!("缺失子系统: {} 项", gaps.subsystems.len());
    println!(
        "  状态    : missing={}  workaround={}  partial={}  landed={}",
        gap_status("missing"),
        gap_status("workaround"),
        gap_status("partial"),
        gap_status("landed")
    );
    println!("位段定义  : {} 个", bf_defs);
    println!("Golden 基线: {} 份 (ldp golden 列举)", golden);
    println!();
    println!("下一步:");
    println!("  ldp lookup <api>     查某条 Linux API 的 x-kernel 对应");
    println!("  ldp gaps             列缺失子系统");
    println!("  ldp check-data       校验数据完整性(source.path / 位段锚 / 重复 id)");
    println!("  ldp check-layout     位段声明 ↔ 参考真值(从钉死参考机械导出,diff claim)");
    println!("  ldp golden           跑 golden 回归");
    println!("  ldp help             全部子命令与边界");
}

/// 一条查询的匹配结果。
struct Hit<'a> {
    section: &'a str,
    row: &'a MappingRow,
    score: usize,
    /// 强命中:有 keyword 精确等于查询词,或 linux 名精确等于查询词。
    /// 全部命中都不是强命中时,lookup 会补一条缺口提示——避免弱子串命中
    /// 把一个真实但未映射的 API 误判成"已映射"(铁律 4:缺口必报)。
    exact: bool,
}

/// `ldp lookup`:结构化候选查询。
pub fn lookup(api: &ApiMapping, query: &str) -> Result<(), String> {
    if query.trim().is_empty() {
        return Err("lookup 需要一个查询词,例如 `ldp lookup readl`".into());
    }
    let q = query.trim().to_ascii_lowercase();
    let mut hits: Vec<Hit> = Vec::new();
    for sec in &api.sections {
        for row in &sec.rows {
            let (score, exact) = score_row(row, &q);
            if score > 0 {
                hits.push(Hit {
                    section: &sec.title,
                    row,
                    score,
                    exact,
                });
            }
        }
    }
    hits.sort_by_key(|h| std::cmp::Reverse(h.score));

    let total = hits.len();
    if total == 0 {
        println!("未命中:`{}` 在 api-mapping.json 中无对应。", q);
        println!("→ 按铁律 2,这可能是缺口;查 `ldp gaps` 或按判断框架定级后补一条。");
        return Ok(());
    }
    let shown = total.min(LOOKUP_MAX);
    println!("命中 {} 条(展示 {}/{})", total, shown, total);
    if total > LOOKUP_MAX {
        println!(
            "(显式截断:用更窄的查询词看剩余,例如 `ldp lookup {}` 加限定)",
            q
        );
    }
    println!("────────────────────────────────────────");
    for h in hits.iter().take(shown) {
        let s = &h.row.source;
        let src = match s {
            Some(src) => {
                let line = src.line.map(|l| format!(":{l}")).unwrap_or_default();
                format!("  [{}{line}]", src.path)
            }
            None => "  [无 source — 注意核对]".to_string(),
        };
        let st = if h.row.status != "landed" {
            format!("  ({})", h.row.status)
        } else {
            String::new()
        };
        println!("[{}] {}{}", h.section, h.row.linux, st);
        println!("  → {}", h.row.xkernel);
        if !h.row.note.is_empty() {
            println!("  · {}", h.row.note);
        }
        println!("{src}");
    }
    // 全部命中都是弱子串匹配时补缺口提示:一个含短词的长查询(如
    // `totally_made_up_api_xyz` 含 "up")会命中 semaphore,但那不是"已映射"——
    // 若查询的是真实 API,这种假命中会让缺口被静默漏报(铁律 4)。
    if !hits.iter().any(|h| h.exact) {
        println!("────────────────────────────────────────");
        println!("⚠ 以上均为模糊子串匹配(无 keyword / linux 名精确命中)。");
        println!("  若查询的是真实 Linux API,按铁律 4 先当潜在缺口处理:");
        println!("  查 `ldp gaps`,或逐条核对后再决定补映射还是报缺口——别因有命中就认定\"已映射\"。");
    }
    Ok(())
}

fn score_row(row: &MappingRow, q: &str) -> (usize, bool) {
    let mut score = 0;
    let mut exact = false;
    for kw in &row.keywords {
        let kwl = kw.to_ascii_lowercase();
        if kwl == q {
            score += 10;
            exact = true;
        } else if kwl.contains(q) || q.contains(&kwl) {
            score += 4;
        }
    }
    let linux = row.linux.to_ascii_lowercase();
    let xk = row.xkernel.to_ascii_lowercase();
    if linux == q {
        exact = true;
    }
    if linux.contains(q) {
        score += 3;
    }
    if xk.contains(q) {
        score += 2;
    }
    (score, exact)
}

/// `ldp gaps`:列出/筛选缺失子系统。
pub fn list_gaps(gaps: &Gaps, status_filter: Option<&str>, id_filter: Option<&str>) {
    let mut shown = 0;
    for g in &gaps.subsystems {
        if let Some(st) = status_filter
            && g.status != st
        {
            continue;
        }
        if let Some(id) = id_filter
            && g.id != id
        {
            continue;
        }
        print_gap(g);
        shown += 1;
    }
    if shown == 0 {
        println!("(无匹配的缺口条目)");
    }
}

fn print_gap(g: &GapSubsystem) {
    let dims = if g.dimensions.is_empty() {
        String::new()
    } else {
        format!(
            "  [多维度: {}]",
            g.dimensions
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    println!("● {} ({}){}", g.name, g.status, dims);
    println!("  id={}  severity={}", g.id, g.severity);
    if !g.symbols.is_empty() {
        println!("  symbols: {}", g.symbols.join(", "));
    }
    if !g.xkernel_status.is_empty() {
        println!("  现状: {}", g.xkernel_status);
    }
    if !g.workaround.is_empty() {
        println!("  替代: {}", g.workaround);
    }
    if !g.dimensions.is_empty() {
        for d in &g.dimensions {
            println!("  · {}: {} — {}", d.name, d.severity, d.workaround);
        }
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(keywords: &[&str], linux: &str) -> MappingRow {
        MappingRow {
            linux: linux.into(),
            xkernel: "x".into(),
            note: String::new(),
            source: None,
            keywords: keywords.iter().map(|s| s.to_string()).collect(),
            status: "landed".into(),
        }
    }

    #[test]
    fn exact_keyword_is_strong() {
        // 真实查询命中精确 keyword → 强命中。
        let (score, exact) = score_row(&row(&["readl", "writel"], "readl/writel"), "readl");
        assert!(exact, "keyword 精确命中应判为强");
        assert!(score >= 10);
    }

    #[test]
    fn long_fake_query_containing_short_keyword_is_weak() {
        // 长假 API 含短 keyword "up" → 仅子串反向命中 → 弱。
        // 这正是会把一个真实但未映射的 API 误判成"已映射"的危险情形,
        // lookup 据此补缺口提示(R1)。
        let (score, exact) = score_row(
            &row(&["up", "down", "semaphore"], "struct semaphore / down/up"),
            "totally_made_up_api_xyz",
        );
        assert!(!exact, "仅子串反向命中应判为弱");
        assert!(score > 0 && score < 10);
    }

    #[test]
    fn linux_name_exact_is_strong_without_keyword() {
        // 无 keyword 时,linux 名精确等于查询也算强。
        let (_, exact) = score_row(&row(&[], "clk_get_rate"), "clk_get_rate");
        assert!(exact);
    }
}
