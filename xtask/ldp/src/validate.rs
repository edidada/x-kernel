// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 校验类子命令:check-data / check-bitfields / scan。
//!
//! 这是"约束支柱"的执行层(fail-closed):
//! - check-data:数据完整性(source.path 必须在仓库存在;字段齐全)。
//! - check-bitfields:位段定义自洽 + 揪 false-pass(SCMI VERSION 类 bug)。
//! - scan:在一份 Linux 源码里扫出用到的、且当前是缺口的子系统符号(铁律 4 自动化)。

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use crate::{
    data::{ApiMapping, BitfieldDef, BitfieldDefs, Gaps},
    repo,
};

/// gap status 状态机合法取值。同时用于 `check-data`(gaps.status 校验)与
/// `ldp gaps --status <s>` 过滤值校验,避免拼写错误(如 `missng`)静默返回空。
pub const GAP_STATUSES: &[&str] = &["open", "workaround", "partial", "missing", "landed"];

/// 某个 status 字符串是否在 gap 状态机内。
pub fn is_valid_gap_status(s: &str) -> bool {
    GAP_STATUSES.contains(&s)
}

/// 找出出现 >1 次的字符串(排序去重),用于重复 id 检测(R6):
/// 同名/同 id 会让 lookup 与 golden 的 `find()` 行为依赖顺序,隐蔽且危险。
fn duplicate_values(values: &[String]) -> Vec<String> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for v in values {
        *counts.entry(v.as_str()).or_default() += 1;
    }
    counts
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(k, _)| k.to_string())
        .collect()
}

/// `ldp check-data`:数据完整性校验。返回错误数(0 = 通过)。
pub fn check_data(api: &ApiMapping, gaps: &Gaps, bf: &BitfieldDefs) -> usize {
    let mut errors = 0usize;
    let mut no_source = 0usize;
    let mut warnings = 0usize;

    // api-mapping:每条带 source 的,path 必须存在(铁律 2 数据化)。
    // 无 source 的(std 等价物如 Mutex/atomic/log/Box)不警告——它们是显然映射。
    for sec in &api.sections {
        for row in &sec.rows {
            if let Some(src) = &row.source {
                // source.path 必须是仓库内相对路径(防绝对路径/..逃逸)。
                if src.path.starts_with('/') || src.path.split('/').any(|c| c == "..") {
                    eprintln!(
                        "ERROR [api-mapping] `{}` 的 source.path 必须是仓库相对路径: {}",
                        row.linux, src.path
                    );
                    errors += 1;
                } else if !repo::repo_path_exists(&src.path) {
                    eprintln!(
                        "ERROR [api-mapping] `{}` 引用了不存在的 source.path: {}",
                        row.linux, src.path
                    );
                    errors += 1;
                }
            } else if row.status == "landed" {
                no_source += 1;
            }
            if row.xkernel.trim().is_empty() {
                eprintln!("ERROR [api-mapping] `{}` 的 xkernel 为空", row.linux);
                errors += 1;
            }
        }
    }

    // gaps:status 必须在状态机内。
    let valid = GAP_STATUSES;
    for g in &gaps.subsystems {
        if !is_valid_gap_status(&g.status) {
            eprintln!(
                "ERROR [gaps] `{}` 的 status=`{}` 不在状态机 {:?} 内",
                g.id, g.status, valid
            );
            errors += 1;
        }
        if g.name.trim().is_empty() {
            eprintln!("ERROR [gaps] id=`{}` 的 name 为空", g.id);
            errors += 1;
        }
        // infra_type 必须是合法分类(requirements gen 据此选实现路径模板)。
        if !g.infra_type.is_empty()
            && !crate::requirements::INFRA_TYPES.contains(&g.infra_type.as_str())
        {
            eprintln!(
                "ERROR [gaps] `{}` 的 infra_type=`{}` 无效,应为 {:?}",
                g.id,
                g.infra_type,
                crate::requirements::INFRA_TYPES
            );
            errors += 1;
        }
    }

    // provider 边界 nudge(可选,不阻塞):provider 类 gap 在 provider-boundary.json
    // 里无判据记录 → WARN。判据是判断,留空≠错,只=没论证(不 fail-closed)。
    let pb = crate::repo::load_provider_boundary();
    if let Ok(pb) = &pb {
        for g in &gaps.subsystems {
            if g.infra_type == "multi-provider" || g.infra_type == "single-provider" {
                let has = pb.per_gap_assessment.get(&g.id).is_some();
                if !has {
                    eprintln!(
                        "WARN  [provider-boundary] `{}` 是 `{}` 但无判据评估记录。填 \
                         data/provider-boundary.json 的 per_gap_assessment.{} 后重新 gen。",
                        g.id, g.infra_type, g.id
                    );
                    warnings += 1;
                }
            }
        }
    }

    // bitfields:字段位段合法 + 不重叠(正确布局)+ 锚存在性(R4)。
    // references.json 是参考树注册表;ref.tree 必须是已注册 id。
    // (ref.file 是否真实存在要 vendor 参考树,留给 check-layout fail-closed。)
    let references = crate::repo::load_references();
    let (ref_ids, have_refs): (BTreeSet<&str>, bool) = match &references {
        Ok(r) => (
            r.trees.iter().map(|t| t.id.as_str()).collect(),
            references.is_ok(),
        ),
        Err(_) => (BTreeSet::new(), false),
    };
    for def in &bf.defs {
        if let Err(e) = check_def_layout(def) {
            eprintln!("ERROR [bitfields] `{}`: {}", def.id, e);
            errors += 1;
        }
        // ref.tree 必须在 references.json 注册(加载失败时不重复报)。
        if have_refs && !ref_ids.contains(def.r#ref.tree.as_str()) {
            eprintln!(
                "ERROR [bitfields] `{}` 的 ref.tree `{}` 未在 references.json 注册",
                def.id, def.r#ref.tree
            );
            errors += 1;
        }
        // driver_src.path(若声明)必须是仓库内存在的相对路径(同 api-mapping source.path 口径)。
        // scope=port 的 def 在不带 --port 时会被 check-layout 跳过,这里补上这条轻量校验。
        if let Some(drv) = &def.driver_src {
            if drv.path.starts_with('/') || drv.path.split('/').any(|c| c == "..") {
                eprintln!(
                    "ERROR [bitfields] `{}` 的 driver_src.path 必须是仓库相对路径: {}",
                    def.id, drv.path
                );
                errors += 1;
            } else if !repo::repo_path_exists(&drv.path) {
                eprintln!(
                    "ERROR [bitfields] `{}` 引用了不存在的 driver_src.path: {}",
                    def.id, drv.path
                );
                errors += 1;
            }
        }
    }

    // 重复 id 检测(R6):同名/同 id 会让 lookup 与 golden 的 find() 行为依赖顺序。
    let api_linux: Vec<String> = api
        .sections
        .iter()
        .flat_map(|s| s.rows.iter().map(|r| r.linux.clone()))
        .collect();
    for d in duplicate_values(&api_linux) {
        eprintln!(
            "ERROR [api-mapping] linux 名 `{d}` 出现多次(应唯一,否则 lookup/golden 命中不确定)"
        );
        errors += 1;
    }
    let gap_ids: Vec<String> = gaps.subsystems.iter().map(|g| g.id.clone()).collect();
    for d in duplicate_values(&gap_ids) {
        eprintln!("ERROR [gaps] id `{d}` 出现多次(应唯一)");
        errors += 1;
    }
    let bf_ids: Vec<String> = bf.defs.iter().map(|d| d.id.clone()).collect();
    for d in duplicate_values(&bf_ids) {
        eprintln!("ERROR [bitfields] def id `{d}` 出现多次(应唯一)");
        errors += 1;
    }

    println!(
        "check-data: {} 错误, {} 警告 (api={}, gaps={}, bitfields={}; 其中 {} 条 landed 映射无 \
         source—多为 std 等价物,正常;provider 判据缺失警告见上)",
        errors,
        warnings,
        api.sections.iter().map(|s| s.rows.len()).sum::<usize>(),
        gaps.subsystems.len(),
        bf.defs.len(),
        no_source
    );
    errors
}

fn check_def_layout(def: &BitfieldDef) -> Result<(), String> {
    let mut occupied: BTreeMap<u32, String> = BTreeMap::new();
    for f in &def.fields {
        if f.claim.msb < f.claim.lsb {
            return Err(format!(
                "字段 `{}` msb({}) < lsb({})",
                f.name, f.claim.msb, f.claim.lsb
            ));
        }
        if f.claim.msb >= def.width {
            return Err(format!(
                "字段 `{}` msb({}) >= 总宽({})",
                f.name, f.claim.msb, def.width
            ));
        }
        for b in f.claim.lsb..=f.claim.msb {
            if let Some(prev) = occupied.insert(b, f.name.clone()) {
                return Err(format!("字段 `{}` 与 `{}` 在 bit {} 重叠", f.name, prev, b));
            }
        }
    }
    Ok(())
}

/// 一次 scan 命中的缺口及其命中符号。
pub struct ScanHit {
    pub gap: crate::data::GapSubsystem,
    pub hits: Vec<String>,
}

/// scan 的一次结果:命中列表 + 语料统计。
pub struct ScanResult {
    pub hits: Vec<ScanHit>,
    pub files: usize,
    pub bytes: usize,
}

/// 收集一份 Linux 源码里命中 gap symbols 的子系统(铁律 4 数据化)。
///
/// 返回完整 gap 元信息(报告草稿/决策门用);`requirements::findings_from_scan`
/// 聚合同一份命中出需求 finding。
pub fn scan_hits(gaps: &Gaps, path: &Path) -> Result<ScanResult, String> {
    let mut files = Vec::new();
    collect_source_files(path, &mut files)?;
    if files.is_empty() {
        return Err(format!("`{}` 下没有 .c/.h 文件", path.display()));
    }
    // 逐文件读全文,统计每个 gap 子系统 symbols 的命中。
    let mut corpus = String::new();
    for f in &files {
        match std::fs::read_to_string(f) {
            Ok(s) => corpus.push_str(&s),
            Err(_) => { /* 二进制/读不动,跳过 */ }
        }
    }
    let mut hits = Vec::new();
    for g in &gaps.subsystems {
        let syms: Vec<String> = g
            .symbols
            .iter()
            .filter(|s| corpus.contains(s.as_str()))
            .cloned()
            .collect();
        if !syms.is_empty() {
            hits.push(ScanHit {
                gap: g.clone(),
                hits: syms,
            });
        }
    }
    Ok(ScanResult {
        hits,
        files: files.len(),
        bytes: corpus.len(),
    })
}

/// 打印 scan 命中结果(命令行的信息输出部分)。
pub fn print_scan_result(res: &ScanResult) {
    println!("扫描 {} 个文件(约 {} 字节)", res.files, res.bytes);
    println!("────────────────────────────────────────");
    for hit in &res.hits {
        // 过短符号(如 RCU/up)在拼接语料上做字面子串匹配易误命中,标注提醒:
        // scan 是字面匹配不解析 C,短词命中≠真用了该缺口符号。
        let shown: Vec<String> = hit
            .hits
            .iter()
            .map(|s| {
                if s.chars().count() <= 3 {
                    format!("{s} ⚠短词,可能误匹配")
                } else {
                    s.clone()
                }
            })
            .collect();
        println!(
            "● {} [{}] 命中符号: {}",
            hit.gap.name,
            hit.gap.status,
            shown.join(", ")
        );
        println!("  → 严重度模型: {}", hit.gap.severity_summary());
    }
    if res.hits.is_empty() {
        println!("(未命中任何已知缺口符号 — 这份源码可能只用主干 API)");
    }
}

/// `ldp scan --report <out>`:把 scan 结果渲染成缺口报告草稿并落盘。
///
/// 这是「先报告、后生成」的第一步:草稿在生成代码之前就存在,P3/P4 是往
/// 草稿里填证据(逐条处置、参考序列对照表、集成结论),不是事后补报告。
/// 可阻塞缺口(严重度模型含 blocker/conditional)会在「阻塞项汇总」里标出
/// 决策状态,与 `ldp scan --gate` 的判定口径一致。
pub fn render_report_draft(
    res: &ScanResult,
    linux_path: &Path,
    out_path: &Path,
) -> Result<(), String> {
    let mut s = String::new();
    s.push_str(
        "<!-- 本文件由 `ldp scan --report` 自动生成(草稿)。\n人工补全:compatible / DeviceKind / \
         落点 / 逐条处置 / 可编译性结论 / 集成状态。\n模板见 templates/gap-report.md。 -->\n\n",
    );
    s.push_str(&format!(
        "# 移植缺口报告(草稿)— {}\n\n",
        linux_path.display()
    ));
    s.push_str("| 字段 | 值 |\n|------|----|\n");
    s.push_str(&format!("| Linux 源 | `{}` |\n", linux_path.display()));
    s.push_str("| 目标 compatible | 待填 |\n");
    s.push_str("| 目标 DeviceKind / bus | 待填 |\n");
    s.push_str("| x-kernel 落点 | 待填(硬件逻辑层 + 胶水层) |\n");
    s.push_str("| 移植结论 | **待定**(可完整工作 / 受限可用 / 阻塞-需先补子系统) |\n\n");

    s.push_str("## 依赖的 Linux 子系统清单(scan 自动命中)\n\n");
    for hit in &res.hits {
        let g = &hit.gap;
        let mark = if g.status == "landed" { "✅" } else { "❌" };
        s.push_str(&format!(
            "- [ ] {} `{}` [{}] 命中: {}\n",
            mark,
            g.name,
            g.status,
            hit.hits.join(", ")
        ));
    }
    s.push('\n');

    s.push_str("## 逐条缺口(scan 自动命中;处置待填)\n\n");
    s.push_str("| 子系统 | 严重度模型 | 现状 | 处置建议 | 需求 issue |\n");
    s.push_str("|--------|-----------|------|---------|-----------|\n");
    for hit in &res.hits {
        let g = &hit.gap;
        let w = if g.workaround.is_empty() {
            "—".to_string()
        } else {
            g.workaround.replace('\n', " ")
        };
        s.push_str(&format!(
            "| **{}** | {} | {} | {} | {} |\n",
            g.name,
            g.severity_summary(),
            g.xkernel_status.replace('\n', " "),
            w,
            crate::requirements::requirement_summary(&g.id)
        ));
    }
    s.push('\n');

    s.push_str("## 阻塞项汇总(决策门,生成代码之前必须过)\n\n");
    let mut any_block = false;
    for hit in &res.hits {
        if !hit.gap.may_block_port() {
            continue;
        }
        any_block = true;
        let req = crate::requirements::requirement_summary(&hit.gap.id);
        if crate::requirements::gap_has_decided_requirement(&hit.gap.id) {
            s.push_str(&format!(
                "- ✅ `{}`: 严重度模型含 conditional/blocker,已决策({})。\n",
                hit.gap.id, req
            ));
        } else {
            s.push_str(&format!(
                "- ⚠ `{}`: 严重度模型含 conditional/blocker,尚无已决策需求 issue({})。\n",
                hit.gap.id, req
            ));
            s.push_str(
                "  - 下一步:`ldp requirements gen --from-scan <路径>` 生成 issue,人工 `confirm`\n",
            );
            s.push_str("    后 `ldp scan <路径> --gate` 通过,才能进入代码生成。\n");
        }
    }
    if !any_block {
        s.push_str(
            "- 无:命中的缺口均不含 blocker/conditional,可直接进入代码生成(处置照 gaps.json 的 \
             workaround)。\n",
        );
    }
    s.push('\n');

    s.push_str(
        "## 可编译性结论\n\n- `make build` / `cargo clippy`(rk3588 target):待填(Phase 5)。\n\n",
    );
    s.push_str("## 集成状态\n\n- 注册三连 / 真机验证(Phase 5/6):待填。\n");
    s.push_str("> 在草稿上继续补全(处置、编号参考序列对照表、集成记录),完成后即正式缺口报告。\n");

    if let Some(p) = out_path.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    std::fs::write(out_path, s).map_err(|e| format!("{}: {e}", out_path.display()))?;
    Ok(())
}

/// scan 的源码收集(requirements::findings_from_scan 经 scan_hits 复用本函数)。
fn collect_source_files(path: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    if path.is_file() {
        if let Some(e) = path.extension()
            && (e == "c" || e == "h")
        {
            out.push(path.to_path_buf());
        }
        return Ok(());
    }
    if !path.is_dir() {
        return Err(format!("`{}` 不是文件也不是目录", path.display()));
    }
    for entry in std::fs::read_dir(path).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let p = entry.path();
        if p.is_dir() {
            if let Some(name) = p.file_name().and_then(|n| n.to_str())
                && matches!(name, ".git" | "Documentation" | "tools" | "samples")
            {
                continue;
            }
            collect_source_files(&p, out)?;
        } else if let Some(e) = p.extension()
            && (e == "c" || e == "h")
        {
            out.push(p);
        }
    }
    Ok(())
}

/// `ldp check-provider-crate <Cargo.toml>`:机械查 provider crate 的 OS-agnostic 红线。
///
/// provider-boundary.json 的 redline:声称 OS-agnostic 的 provider crate,Cargo.toml
/// 不得依赖任何 `k*` crate(kspin/khal/...)或 `of`(DT)crate。这是 provider 三坑里
/// 唯一机械可查的一条(其余两条是设计判断,走需求 issue 的实现质量清单)。
///
/// 判据:dep 名 == "of",或 dep 名以 `k` 开头且第二字符是小写字母(=k 前缀内核 crate)。
/// 不误伤 smallvec/log/device_res/unittest 等。
///
/// 用 `toml` crate 解析(而非逐行字符串匹配),正确处理 inline table、sub-table
/// (`[dependencies.foo]`)、`[target.'cfg(...)'.dependencies]` 等 Cargo.toml 形态。
pub fn check_provider_crate(manifest: &Path) -> usize {
    let raw = match std::fs::read_to_string(manifest) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: 读 {} 失败: {e}", manifest.display());
            return 1;
        }
    };
    let doc: toml::Value = match toml::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "error: 解析 {} 失败(不是合法 TOML): {e}",
                manifest.display()
            );
            return 1;
        }
    };
    let deps = collect_dep_names(&doc);
    let mut errors = 0usize;
    for (section, dep) in &deps {
        let violates = dep == "of"
            || (dep.starts_with('k') && dep.len() > 1 && dep.as_bytes()[1].is_ascii_lowercase());
        if violates {
            eprintln!(
                "ERROR [provider-crate] {} 的 [{section}] 依赖 `{dep}` — 违反 OS-agnostic \
                 红线(provider crate 不得依赖 k*/of)。",
                manifest.display()
            );
            errors += 1;
        }
    }
    if errors == 0 {
        println!(
            "✓ {} 无 k*/of 依赖(扫描 {} 个依赖项,provider crate 的 OS-agnostic 红线满足)。",
            manifest.display(),
            deps.len()
        );
    } else {
        eprintln!("✗ {errors} 处违反红线(见上)。修法见 provider-boundary.json 的 impl_pitfalls。");
    }
    errors
}

/// 从 Cargo.toml 的 TOML 文档收集所有依赖名(扫描 dependencies / dev-dependencies /
/// target.<cfg>.dependencies)。返回 (section 描述, dep 名),供报错定位。
/// sub-table 形态(`[dependencies.foo]`)与 inline table(`foo = { ... }`)都取到 key "foo"。
fn collect_dep_names(doc: &toml::Value) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for section in ["dependencies", "dev-dependencies"] {
        if let Some(t) = doc.get(section).and_then(toml::Value::as_table) {
            for k in t.keys() {
                out.push((section, k.clone()));
            }
        }
    }
    if let Some(targets) = doc.get("target").and_then(toml::Value::as_table) {
        for (_cfg, tv) in targets {
            let Some(ttab) = tv.as_table() else {
                continue;
            };
            for section in ["dependencies", "dev-dependencies"] {
                if let Some(dep_tab) = ttab.get(section).and_then(toml::Value::as_table) {
                    for k in dep_tab.keys() {
                        out.push(("target.* dependencies", k.clone()));
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gap_status_validation() {
        for ok in &["open", "workaround", "partial", "missing", "landed"] {
            assert!(is_valid_gap_status(ok), "{ok} 应合法");
        }
        assert!(!is_valid_gap_status("missng"), "拼写错误应被判出");
        assert!(!is_valid_gap_status(""));
    }

    #[test]
    fn duplicate_values_finds_repeats() {
        assert_eq!(
            duplicate_values(&["a".into(), "b".into(), "a".into(), "c".into(), "b".into()]),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(duplicate_values(&["x".into(), "y".into()]).is_empty());
        assert!(duplicate_values(&[]).is_empty());
    }

    #[test]
    fn collect_dep_names_handles_inline_subtable_and_target() {
        // 覆盖 Cargo.toml 的三种依赖形态 + target.* + dev-deps(toml 解析路径)。
        let toml_src = r#"
[dependencies]
smallvec = "1"
khal = { workspace = true }
of = "1"

[dev-dependencies]
unittest = { workspace = true }

[target.'cfg(unix)'.dependencies]
log = "0.4"

[dependencies.nested]
version = "1"
"#;
        let doc: toml::Value = toml::from_str(toml_src).unwrap();
        let names: Vec<String> = collect_dep_names(&doc)
            .into_iter()
            .map(|(_, n)| n)
            .collect();
        assert!(names.contains(&"smallvec".into()));
        assert!(names.contains(&"khal".into()));
        assert!(names.contains(&"of".into()));
        assert!(
            names.contains(&"nested".into()),
            "sub-table 形态 `[dependencies.nested]` 也要取到 key"
        );
        assert!(
            names.contains(&"unittest".into()),
            "dev-dependencies 要扫到"
        );
        assert!(
            names.contains(&"log".into()),
            "target.'cfg(...)'.dependencies 要扫到"
        );
    }

    #[test]
    fn gap_severity_model_may_block_port() {
        let gap = |severity: &str, dims: &str, branches: &str| -> crate::data::GapSubsystem {
            serde_json::from_str(&format!(
                r#"{{
                    "id": "t",
                    "name": "test",
                    "status": "missing",
                    "severity": "{severity}",
                    "dimensions": {dims},
                    "severity_branches": {branches}
                }}"#
            ))
            .unwrap()
        };

        // 纯 works-around:不触发门。
        let g = gap("works-around", "[]", "{}");
        assert!(!g.may_block_port());
        // 顶层 conditional:触发。
        let g = gap("conditional", "[]", "{}");
        assert!(g.may_block_port());
        // 顶层空(未声明):按 triage「拿不准按 blocker」触发。
        let g = gap("", "[]", "{}");
        assert!(g.may_block_port());
        // 维度级 conditional(clk-rate 形态):触发。
        let dims = r#"[{"name":"clk-rate","linux":["clk_get_rate"],"severity":"conditional"}]"#;
        let g = gap("", dims, "{}");
        assert!(g.may_block_port());
        // 分支级 blocker(gpio functional_pin 形态):触发。
        let branches = r#"{"functional_pin":{"severity":"blocker","when":"作为功能引脚"}}"#;
        let g = gap("conditional", "[]", branches);
        assert!(g.may_block_port());
        // 分支全 works-around + 顶层 works-around:不触发。
        let branches =
            r#"{"power_reset_pin":{"severity":"works-around","when":"firmware 已拉好"}}"#;
        let g = gap("works-around", "[]", branches);
        assert!(!g.may_block_port());
    }

    #[test]
    fn scan_report_draft_writes_expected_sections() {
        let gaps: Gaps = serde_json::from_str(
            r#"{
                "subsystems": [
                    {
                        "id": "clk-framework",
                        "name": "clk framework",
                        "symbols": ["clk_get_rate"],
                        "status": "partial",
                        "severity": "",
                        "dimensions": [
                            {"name":"clk-enable","linux":[],"severity":"works-around"},
                            {"name":"clk-rate","linux":["clk_get_rate"],"severity":"conditional"}
                        ],
                        "xkernel_status": "Tier-0 已落地"
                    }
                ]
            }"#,
        )
        .unwrap();
        let res = ScanResult {
            hits: vec![ScanHit {
                gap: gaps.subsystems[0].clone(),
                hits: vec!["clk_get_rate".to_string()],
            }],
            files: 1,
            bytes: 42,
        };
        let out =
            std::env::temp_dir().join(format!("ldp-scan-report-test-{}.md", std::process::id()));
        render_report_draft(&res, Path::new("drivers/foo.c"), &out).unwrap();
        let txt = std::fs::read_to_string(&out).unwrap();
        for expected in [
            "移植缺口报告(草稿)",
            "## 依赖的 Linux 子系统清单",
            "## 逐条缺口",
            "## 阻塞项汇总",
            "clk-framework",
        ] {
            assert!(txt.contains(expected), "草稿应含 `{expected}`");
        }
        let _ = std::fs::remove_file(&out);
    }
}
