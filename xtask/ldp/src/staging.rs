// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 离线进化循环(Ch8):聚合轨迹 → 候选 → 门槛发布。
//!
//! 三道安全边界的落地:
//! 1. **证据/指令隔离**:`trace` 是观察记录;`promote` 不执行任何外部动作,只标状态。
//! 2. **候选/正式分离**:`staging/` 与 `data/{api-mapping,gaps,bitfield-defs}.json` 物理分离;
//!    `promote` 只标 `promoted` + 打印入库指引,**不自动改正式 `data/`**(入库需人/agent
//!    判断"加进哪个 section / 用什么 xkernel",这是判断不是机械操作)。
//! 3. **验证器不可自我修改**:发布门槛(`PROMOTE_THRESHOLD`)硬编码在二进制里;
//!    `promote` 不碰 `golden/` 与门槛规则。并入正式 data/ 后的 `check-* && golden`
//!    由人在终端跑,门槛脚本本身不会被生成物改写。

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::repo;
use crate::trace::read_all;

/// 发布门槛:候选需被 ≥ 这个数量的不同 port 轨迹支持才可 promote。
/// Ch8 原文:"至少两条非失败轨迹支持才写入正式文档。"
pub const PROMOTE_THRESHOLD: usize = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub id: String,
    pub kind: String,
    pub linux: String,
    pub note: String,
    pub support_count: usize,
    pub sources: Vec<String>,
    /// `new` / `reviewed` / `promoted`。
    pub status: String,
    pub first_ts: u64,
    pub last_ts: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Staging {
    pub candidates: Vec<Candidate>,
}

pub fn staging_dir() -> PathBuf {
    repo::data_dir().join("staging")
}

pub fn staging_path() -> PathBuf {
    staging_dir().join("candidates.json")
}

pub fn load() -> Staging {
    repo::read_json::<Staging>(&staging_path()).unwrap_or_default()
}

fn write(s: &Staging) -> Result<(), String> {
    let path = staging_path();
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(())
}

/// 聚合所有轨迹 → candidates.json。保留已 promoted 候选的 status(不会被覆盖回 new)。
pub fn aggregate() -> Result<Staging, String> {
    let old = load();
    let old_status: BTreeMap<String, String> = old
        .candidates
        .iter()
        .map(|c| (c.id.clone(), c.status.clone()))
        .collect();

    #[derive(Default)]
    struct Acc {
        kind: String,
        notes: Vec<String>,
        sources: BTreeSet<String>,
        first_ts: u64,
        last_ts: u64,
    }
    let mut accs: BTreeMap<(String, String), Acc> = BTreeMap::new();

    for (port, events) in read_all() {
        for ev in events {
            let key = (ev.kind.as_str().to_string(), ev.linux.clone());
            let acc = accs.entry(key.clone()).or_default();
            acc.kind = ev.kind.as_str().to_string();
            if !ev.note.is_empty() {
                acc.notes.push(ev.note.clone());
            }
            acc.sources.insert(port.clone());
            if acc.first_ts == 0 || ev.ts < acc.first_ts {
                acc.first_ts = ev.ts;
            }
            if ev.ts > acc.last_ts {
                acc.last_ts = ev.ts;
            }
        }
    }

    let mut candidates: Vec<Candidate> = accs
        .into_iter()
        .map(|((kind, linux), acc)| {
            let id = format!("{kind}:{linux}");
            let status = old_status.get(&id).cloned().unwrap_or_else(|| "new".into());
            // note:取最长那条(信息最全);无则空。
            let note = acc
                .notes
                .iter()
                .max_by_key(|n| n.len())
                .cloned()
                .unwrap_or_default();
            Candidate {
                id,
                kind,
                linux,
                note,
                support_count: acc.sources.len(),
                sources: acc.sources.into_iter().collect(),
                status,
                first_ts: acc.first_ts,
                last_ts: acc.last_ts,
            }
        })
        .collect();
    // 排序:支持数降序 → linux 字典序(稳定可读)。
    candidates.sort_by(|a, b| {
        b.support_count
            .cmp(&a.support_count)
            .then(a.linux.cmp(&b.linux))
    });

    let staging = Staging { candidates };
    write(&staging)?;
    Ok(staging)
}

/// 标一个候选 promoted + 打印入库指引。不自动改正式 data/(安全边界 2)。
pub fn promote(id: &str) -> Result<Candidate, String> {
    let mut s = load();
    let c = s
        .candidates
        .iter_mut()
        .find(|c| c.id == id)
        .ok_or_else(|| format!("无候选 id=`{id}`。跑 `ldp staging review` 看可用 id"))?;
    if c.status == "promoted" {
        return Err(format!("候选 `{id}` 已 promoted(避免重复发布)"));
    }
    if c.support_count < PROMOTE_THRESHOLD {
        return Err(format!(
            "候选 `{id}` 支持数 {} < 门槛 {}。Ch8:至少两条非失败轨迹支持才可发布。",
            c.support_count, PROMOTE_THRESHOLD
        ));
    }
    c.status = "promoted".to_string();
    let snapshot = c.clone();
    write(&s)?;
    Ok(snapshot)
}

/// 给 promote 成功的候选打印"并入正式 data/"的指引(按 kind 分支)。
pub fn promote_guidance(c: &Candidate) {
    println!(
        "✓ 候选 `{}` 已标 promoted(支持数 {})",
        c.id, c.support_count
    );
    println!("  来源 port: {}", c.sources.join(", "));
    if !c.note.is_empty() {
        println!("  备注: {}", c.note);
    }
    println!("────────────────────────────────────────");
    println!("⚠ 下一步由人/agent 手动并入正式 data/(promote 不自动改 data/,Ch8 安全边界 2):");
    match c.kind.as_str() {
        "lookup-miss" => {
            println!("  → 先判断这是'新映射'还是'新缺口':");
            println!("    · 若 x-kernel 有对应实现 → 加进 data/api-mapping.json 的合适 section,");
            println!(
                "      填全 {{linux, xkernel, source{{path}}, keywords}}(source.path 必须存在,铁律2)。"
            );
            println!(
                "    · 若确认无对应 → 加进 data/gaps.json(新 subsystem 或补已有 subsystem 的 symbols)。"
            );
        }
        "gap" => {
            println!("  → 加进 data/gaps.json:");
            println!(
                "    · 已有同 id subsystem → 补进 symbols,把用法证据补进 workaround/report_template。"
            );
            println!(
                "    · 新缺口 → 加 subsystem 条目(id/name/symbols/status/severity/workaround)。"
            );
        }
        "bitfield-pitfall" => {
            println!("  → 加进 data/bitfield-defs.json 对应 def 的 known_wrong_layouts:");
            println!(
                "    · 附 {{desc, fields:[{{name,lsb,msb}}]}}(从 Linux 源码逐位核对 + GENMASK 依据)。"
            );
            println!(
                "    · 同时补一条 verification_traces,用**非零字段**(避免 VERSION 类 false-pass)。"
            );
        }
        other => println!("  (未知 kind `{other}`)"),
    }
    println!();
    println!("⚠ 并入后必须跑(Ch8 验证器门槛,验证器不可自我修改):");
    println!("    ldp check-data && ldp check-bitfields && ldp golden && ldp gen-doc");
    println!("  任一失败 → 回滚 data/ 改动;该候选保持 promoted 待修。");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_is_two() {
        // Ch8:至少两条非失败轨迹支持。这个常量是安全边界,改它需评审。
        assert_eq!(PROMOTE_THRESHOLD, 2);
    }
}
