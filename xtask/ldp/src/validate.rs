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

use crate::bitfield::{Verdict, classify_trace};
use crate::data::{ApiMapping, BitfieldDef, BitfieldDefs, Gaps};
use crate::repo;

/// `ldp check-data`:数据完整性校验。返回错误数(0 = 通过)。
pub fn check_data(api: &ApiMapping, gaps: &Gaps, bf: &BitfieldDefs) -> usize {
    let mut errors = 0usize;
    let mut no_source = 0usize;

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

    // bitfields:字段位段合法 + 不重叠(正确布局)。
    for def in &bf.defs {
        if let Err(e) = check_def_layout(def) {
            eprintln!("ERROR [bitfields] `{}`: {}", def.id, e);
            errors += 1;
        }
    }

    println!(
        "check-data: {} 错误 (api={}, gaps={}, bitfields={}; 其中 {} 条 landed 映射无 source—多为 std 等价物,正常)",
        errors,
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
        if f.msb < f.lsb {
            return Err(format!("字段 `{}` msb({}) < lsb({})", f.name, f.msb, f.lsb));
        }
        if f.msb >= def.width {
            return Err(format!(
                "字段 `{}` msb({}) >= 总宽({})",
                f.name, f.msb, def.width
            ));
        }
        for b in f.lsb..=f.msb {
            if let Some(prev) = occupied.insert(b, f.name.clone()) {
                return Err(format!("字段 `{}` 与 `{}` 在 bit {} 重叠", f.name, prev, b));
            }
        }
    }
    Ok(())
}

/// `ldp check-bitfields`:位段 false-pass 检测。返回错误数。
///
/// 对每条 verification_trace,在正确布局与每个 known_wrong_layout 下分别打包;
/// 打包值相同却自评"能区分" → 矛盾(fail-closed)。
pub fn check_bitfields(bf: &BitfieldDefs) -> usize {
    let mut errors = 0usize;
    let mut notes = 0usize;
    let mut confirms = 0usize;

    for def in &bf.defs {
        println!("══ {} ({}) ══", def.id, def.name);
        if def.known_wrong_layouts.is_empty() {
            println!("  (无 known_wrong_layouts,跳过 false-pass 检测)");
            continue;
        }
        for trace in &def.verification_traces {
            print!("  · {} :", trace.name);
            let mut trace_bad = false;
            for wrong in &def.known_wrong_layouts {
                match classify_trace(def, &wrong.fields, trace) {
                    Ok(Verdict::DocumentedFalsePass { packed }) => {
                        notes += 1;
                        println!();
                        println!(
                            "      ⚠ 文档化 false-pass(打包=0x{:x}) — 这条 trace 无法区分正确布局与错误布局「{}」",
                            packed, wrong.desc
                        );
                    }
                    Ok(Verdict::Contradiction { correct, wrong: w }) => {
                        errors += 1;
                        trace_bad = true;
                        println!();
                        println!(
                            "      ✗ 矛盾!trace 自评能区分,但打包值相同(correct=0x{:x} == wrong=0x{:x})「{}」",
                            correct, w, wrong.desc
                        );
                        println!(
                            "        → 数据在撒谎。这种 trace 不能用来验证位段(VERSION 骗人那一类)。"
                        );
                    }
                    Ok(Verdict::Confirmed { correct, wrong: w }) => {
                        confirms += 1;
                        print!(" 区分(correct=0x{:x} ≠ wrong=0x{:x})", correct, w);
                    }
                    Ok(Verdict::OverPessimistic { correct, wrong: w }) => {
                        print!(" 能区分但自评悲观(0x{:x} ≠ 0x{:x})", correct, w);
                    }
                    Err(e) => {
                        errors += 1;
                        trace_bad = true;
                        println!();
                        println!("      ✗ 解析失败:{}", e);
                    }
                }
            }
            if !trace_bad {
                println!();
            }
            if !trace.self_note.is_empty() {
                println!("      note: {}", trace.self_note);
            }
        }
    }

    println!("────────────────────────────────────────");
    println!(
        "check-bitfields: {} 确认, {} 文档化 false-pass, {} 矛盾/错误",
        confirms, notes, errors
    );
    if errors > 0 {
        eprintln!("✗ FAIL: 位段验证用例存在矛盾,数据不可信。");
    } else if notes > 0 {
        println!("ℹ 有 {} 条文档化 false-pass(正常,如 VERSION)。", notes);
    }
    errors
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
