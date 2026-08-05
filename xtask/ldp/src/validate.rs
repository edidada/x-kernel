// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 校验类子命令:check-data / check-bitfields / scan。
//!
//! 这是"约束支柱"的执行层(fail-closed):
//! - check-data:数据完整性(source.path 必须在仓库存在;字段齐全)。
//! - check-bitfields:位段定义自洽 + 揪 false-pass(SCMI VERSION 类 bug)。
//! - scan:在一份 Linux 源码里扫出用到的、且当前是缺口的子系统符号(铁律 4 自动化)。

use std::collections::BTreeMap;
use std::path::Path;

use crate::data::{ApiMapping, BitfieldDef, BitfieldDefs, Gaps};
use crate::repo;

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
    let valid = ["open", "workaround", "partial", "missing", "landed"];
    for g in &gaps.subsystems {
        if !valid.contains(&g.status.as_str()) {
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
                        "WARN  [provider-boundary] `{}` 是 `{}` 但无判据评估记录。填 data/provider-boundary.json 的 per_gap_assessment.{} 后重新 gen。",
                        g.id, g.infra_type, g.id
                    );
                    warnings += 1;
                }
            }
        }
    }

    // bitfields:字段位段合法 + 不重叠(正确布局)。
    for def in &bf.defs {
        if let Err(e) = check_def_layout(def) {
            eprintln!("ERROR [bitfields] `{}`: {}", def.id, e);
            errors += 1;
        }
    }

    println!(
        "check-data: {} 错误, {} 警告 (api={}, gaps={}, bitfields={}; 其中 {} 条 landed 映射无 source—多为 std 等价物,正常;provider 判据缺失警告见上)",
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

/// `ldp scan`:在一份 Linux 源码(文件/目录)里扫出用到的缺口符号。
///
/// 这是铁律 4(缺口必报)的自动化:扫一遍源码,告诉你哪些缺口子系统的符号
/// 出现了,提醒"这些必须在缺口报告里写明"。
pub fn scan_source(gaps: &Gaps, path: &Path) -> Result<(), String> {
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
    println!("扫描 {} 个文件(约 {} 字节)", files.len(), corpus.len());
    println!("────────────────────────────────────────");
    let mut any = false;
    for g in &gaps.subsystems {
        let hits: Vec<&String> = g
            .symbols
            .iter()
            .filter(|s| corpus.contains(s.as_str()))
            .collect();
        if !hits.is_empty() {
            any = true;
            println!(
                "● {} [{}] 命中符号: {}",
                g.name,
                g.status,
                hits.iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            println!("  → status={}, 需在缺口报告写明处置", g.status);
        }
    }
    if !any {
        println!("(未命中任何已知缺口符号 — 这份源码可能只用主干 API)");
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
pub fn check_provider_crate(manifest: &Path) -> usize {
    let raw = match std::fs::read_to_string(manifest) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: 读 {} 失败: {e}", manifest.display());
            return 1;
        }
    };
    let mut errors = 0usize;
    let mut in_deps = false;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_deps = trimmed == "[dependencies]"
                || trimmed == "[dev-dependencies]"
                || (trimmed.starts_with("[target.") && trimmed.ends_with(".dependencies]"));
            continue;
        }
        if !in_deps {
            continue;
        }
        let Some(eq) = trimmed.find('=') else {
            continue;
        };
        let dep = trimmed[..eq].trim();
        let dep = dep.split('.').next().unwrap_or(dep).trim();
        if dep.is_empty()
            || !dep
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            continue;
        }
        let violates = dep == "of"
            || (dep.starts_with('k') && dep.len() > 1 && dep.as_bytes()[1].is_ascii_lowercase());
        if violates {
            eprintln!(
                "ERROR [provider-crate] {} 依赖 `{dep}` — 违反 OS-agnostic 红线(provider crate 不得依赖 k*/of)。",
                manifest.display()
            );
            errors += 1;
        }
    }
    if errors == 0 {
        println!(
            "✓ {} 无 k*/of 依赖(provider crate 的 OS-agnostic 红线满足)。",
            manifest.display()
        );
    } else {
        eprintln!("✗ {errors} 处违反红线(见上)。修法见 provider-boundary.json 的 impl_pitfalls。");
    }
    errors
}

fn collect_source_files(path: &Path, out: &mut Vec<std::path::PathBuf>) -> Result<(), String> {
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
            // 跳过常见非源码大目录,避免误扫。
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
