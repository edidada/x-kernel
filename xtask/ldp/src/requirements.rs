// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 需求 issue 生成(把"发现缺口"升级为"可决策的需求")。
//!
//! 流程:scan/trace 发现缺口 → 查 gaps.json 分类 → 按 infra_type 生成 issue md
//! + 更新 index → 人工 confirm → 实现 → gaps.json 标 landed。
//!
//! 五类 infra_type 对应五条不同的「建议实现路径」,这是本模块的核心价值:
//! 不同缺口的补法天差地别(clk 要多 provider 注册表,i2c 要新 DeviceKind,
//! 单纯驱动缺失直接写代码),需求文档必须给出对应路径才能"等人工确认"。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::data::{GapSubsystem, Gaps};
use crate::repo;
use crate::trace;

/// 有效的 infra_type 取值。
pub const INFRA_TYPES: &[&str] = &[
    "multi-provider",
    "single-provider",
    "framework",
    "subsystem-class",
    "driver-missing",
];

/// 默认分类(scan 发现 gaps.json 未列的新缺口时用)。保守取 framework,
/// 因为它覆盖面最广且需要人工设计判断。
pub const DEFAULT_INFRA_TYPE: &str = "framework";

pub fn requirements_dir() -> PathBuf {
    repo::data_dir().join("requirements")
}

pub fn index_path() -> PathBuf {
    requirements_dir().join("index.json")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// index.json 里一条需求记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Requirement {
    /// 稳定 id:`req-<gap_id 或 hash>-<seq>`,跨 gen 保持(幂等关键)。
    pub id: String,
    /// 关联 gaps.json 的 subsystem id;为空 = 新发现(gaps.json 未列)。
    #[serde(default)]
    pub gap_id: String,
    pub title: String,
    pub infra_type: String,
    /// proposed / confirmed / in-progress / landed / rejected。
    pub status: String,
    /// 发现来源:`scan:<path>` / `trace:<port>`。
    pub source: String,
    /// 触发符号(如 clk_set_rate)。
    pub symbol: String,
    pub created_ts: u64,
    #[serde(default)]
    pub confirmed_ts: Option<u64>,
    /// issue md 文件名(相对 requirements/)。
    pub file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Index {
    pub requirements: Vec<Requirement>,
}

fn load_index() -> Index {
    repo::read_json::<Index>(&index_path()).unwrap_or_default()
}

fn write_index(idx: &Index) -> Result<(), String> {
    let path = index_path();
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(idx).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(())
}

/// 推断一个 gap 的 infra_type:已知 gap 读 gaps.json;未知用默认。
pub fn classify(gaps: &Gaps, gap_id: &str) -> String {
    gaps.subsystems
        .iter()
        .find(|g| g.id == gap_id)
        .map(|g| {
            if g.infra_type.is_empty() {
                DEFAULT_INFRA_TYPE.to_string()
            } else {
                g.infra_type.clone()
            }
        })
        .unwrap_or_else(|| DEFAULT_INFRA_TYPE.to_string())
}

/// 一个发现项(scan/trace 产出的、要转成 issue 的)。
#[derive(Debug, Clone)]
pub struct Finding {
    pub gap_id: String,
    pub symbol: String,
    pub source: String,
    pub note: String,
}

/// 从 scan 结果构造 findings。返回的每条对应一个要生成 issue 的缺口。
pub fn findings_from_scan(gaps: &Gaps, path: &Path) -> Result<Vec<Finding>, String> {
    // 复用 validate::scan 的扫描逻辑:读源码,找命中的 gap symbols。
    let mut files = Vec::new();
    collect_source_files(path, &mut files)?;
    if files.is_empty() {
        return Err(format!("`{}` 下没有 .c/.h 文件", path.display()));
    }
    let mut corpus = String::new();
    for f in &files {
        if let Ok(s) = std::fs::read_to_string(f) {
            corpus.push_str(&s);
        }
    }
    let source = format!("scan:{}", path.display());
    let mut findings = Vec::new();
    let mut seen: BTreeMap<String, &GapSubsystem> = BTreeMap::new();
    for g in &gaps.subsystems {
        if g.symbols.iter().any(|s| corpus.contains(s.as_str())) {
            // 按 gap 聚合(同一 gap 的多个 symbol 命中只产一个 issue)。
            let hits: Vec<&String> = g
                .symbols
                .iter()
                .filter(|s| corpus.contains(s.as_str()))
                .collect();
            let note = format!(
                "命中符号: {}",
                hits.iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            seen.entry(g.id.clone()).or_insert(g);
            findings.push(Finding {
                gap_id: g.id.clone(),
                symbol: hits[0].clone(),
                source: source.clone(),
                note,
            });
        }
    }
    Ok(findings)
}

/// 从一个 port 的轨迹构造 findings(trace 里 kind=lookup-miss/gap 的)。
pub fn findings_from_trace(port: &str) -> Result<Vec<Finding>, String> {
    let events = trace::read(port)?;
    let source = format!("trace:{port}");
    let mut by_gap: BTreeMap<String, Finding> = BTreeMap::new();
    for ev in events {
        if matches!(
            ev.kind,
            trace::TraceKind::LookupMiss | trace::TraceKind::Gap
        ) {
            let key = ev.linux.clone();
            let entry = by_gap.entry(key.clone()).or_insert_with(|| Finding {
                gap_id: String::new(), // trace 不直接记 gap_id;symbol 作 key
                symbol: ev.linux.clone(),
                source: source.clone(),
                note: ev.note.clone(),
            });
            if !ev.note.is_empty() {
                if entry.note.is_empty() {
                    entry.note.clone_from(&ev.note);
                } else {
                    entry.note = format!("{} / {}", entry.note, ev.note);
                }
            }
        }
    }
    Ok(by_gap.into_values().collect())
}

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

/// 稳定的 issue id(幂等:同 gap_id+symbol 多次 gen 不变)。
fn issue_id(gap_id: &str, symbol: &str) -> String {
    let stem = if !gap_id.is_empty() {
        gap_id.replace('_', "-")
    } else {
        // 新发现:用 symbol 的 sanitized 形式。
        symbol
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect::<String>()
    };
    format!("req-{stem}")
}

/// 生成/更新需求 issue(幂等:已存在的 id 保留 status/confirmed_ts)。
pub fn gen_issues(
    gaps: &Gaps,
    pb: &crate::data::ProviderBoundary,
    findings: &[Finding],
) -> Result<(Vec<Requirement>, usize, usize), String> {
    let mut idx = load_index();
    let old_by_id: BTreeMap<String, Requirement> = idx
        .requirements
        .iter()
        .map(|r| (r.id.clone(), r.clone()))
        .collect();
    let now = now_secs();

    let mut new_count = 0usize;
    let mut updated_count = 0usize;
    let mut produced: Vec<Requirement> = Vec::new();

    for f in findings {
        let infra = if !f.gap_id.is_empty() {
            classify(gaps, &f.gap_id)
        } else {
            DEFAULT_INFRA_TYPE.to_string()
        };
        let id = issue_id(&f.gap_id, &f.symbol);
        let file = format!("{id}.md");

        let (status, confirmed_ts, is_new) = match old_by_id.get(&id) {
            Some(old) => (old.status.clone(), old.confirmed_ts, false),
            None => ("proposed".to_string(), None, true),
        };
        // gap 元信息(标题/影响)从 gaps.json 取;新发现的就地构造。
        let (title, gap_meta) = if !f.gap_id.is_empty() {
            gaps.subsystems
                .iter()
                .find(|g| g.id == f.gap_id)
                .map(|g| (format!("实现 {}({})", g.name, f.symbol), Some(g.clone())))
                .unwrap_or_else(|| (format!("实现 {}", f.symbol), None))
        } else {
            (format!("实现 {}", f.symbol), None)
        };

        let req = Requirement {
            id: id.clone(),
            gap_id: f.gap_id.clone(),
            title: title.clone(),
            infra_type: infra.clone(),
            status: status.clone(),
            source: f.source.clone(),
            symbol: f.symbol.clone(),
            created_ts: old_by_id.get(&id).map(|o| o.created_ts).unwrap_or(now),
            confirmed_ts,
            file,
        };
        write_issue_md(&req, gap_meta.as_ref(), gaps, pb, &f.note, &f.source)?;
        produced.push(req.clone());
        if is_new {
            new_count += 1;
        } else {
            updated_count += 1;
        }

        // 合并进 idx(替换同 id,保留 status/confirmed_ts)。
        if let Some(slot) = idx.requirements.iter_mut().find(|r| r.id == id) {
            slot.title = req.title;
            slot.infra_type = req.infra_type;
            slot.source = req.source;
            slot.symbol = req.symbol;
            slot.file = req.file;
        } else {
            idx.requirements.push(req);
        }
    }
    write_index(&idx)?;
    Ok((produced, new_count, updated_count))
}

/// 按 infra_type 选模板,写一个 issue md。
fn write_issue_md(
    req: &Requirement,
    gap: Option<&GapSubsystem>,
    gaps: &Gaps,
    pb: &crate::data::ProviderBoundary,
    finding_note: &str,
    source: &str,
) -> Result<(), String> {
    let path = requirements_dir().join(&req.file);
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }
    let mut out = String::new();
    out.push_str(
        "<!-- 本文件由 `ldp requirements gen` 生成。手动确认/补充后即可作为正式需求 issue。-->\n\n",
    );
    out.push_str(&format!("# {}\n\n", req.title));
    out.push_str("| 字段 | 值 |\n|------|----|\n");
    out.push_str(&format!("| ID | `{}` |\n", req.id));
    if !req.gap_id.is_empty() {
        out.push_str(&format!("| 关联缺口 | `{}` (gaps.json) |\n", req.gap_id));
    } else {
        out.push_str("| 关联缺口 | 无(新发现,待人工归类) |\n");
    }
    out.push_str(&format!("| 分类 | `{}` |\n", req.infra_type));
    out.push_str(&format!("| 状态 | `{}` |\n", req.status));
    out.push_str(&format!("| 来源 | `{}` |\n", source));
    out.push_str(&format!("| 触发符号 | `{}` |\n", req.symbol));
    if !finding_note.is_empty() {
        out.push_str(&format!("| 发现备注 | {} |\n", finding_note));
    }
    out.push('\n');

    // 背景 + 现状:从 gaps.json 取(已知 gap);新发现给占位。
    out.push_str("## 背景\n\n");
    if let Some(g) = gap {
        out.push_str(&format!("{}\n\n", g.xkernel_status));
        if !g.symbols.is_empty() {
            let syms = g
                .symbols
                .iter()
                .map(|s| format!("`{s}`"))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!("Linux 侧相关符号:{syms}。\n\n"));
        }
    } else {
        out.push_str("扫描/轨迹中发现的潜在缺口,尚未在 `gaps.json` 中归类。需人工确认这是:\n");
        out.push_str("- 已知缺口的新用法(补进现有 gap 的 symbols),还是\n");
        out.push_str("- 全新缺口(新增 gap subsystem 条目)。\n\n");
    }

    // 影响范围:从 cascade_gaps 找关联。
    if let Some(g) = gap {
        let cascades: Vec<&str> = gaps
            .cascade_gaps
            .iter()
            .filter_map(|c| {
                let area = c.get("area").and_then(|v| v.as_str())?;
                let chain = c
                    .get("chain")
                    .and_then(|v| v.as_array())?
                    .iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                chain.contains(&g.id).then_some(area)
            })
            .collect();
        if !cascades.is_empty() {
            out.push_str("## 影响范围\n\n");
            out.push_str("根据 gaps.json 的级联缺口表,该子系统阻塞以下设备类:\n");
            for a in cascades {
                out.push_str(&format!("- {}\n", a));
            }
            out.push('\n');
        }
    }

    // 分类依据(provider 判据):仅 provider 类 issue 显式应用五判据。
    // 这是从「懒省事顺手标 provider」护坡的关键——让人在确认时逐条过判据。
    if req.infra_type == "multi-provider" || req.infra_type == "single-provider" {
        out.push_str(&provider_criteria_section(pb, &req.gap_id));
    }

    // 核心价值:按 infra_type 给「建议实现路径」。
    out.push_str("## 建议实现路径\n\n");
    out.push_str(impl_path_template(&req.infra_type, gap));
    out.push('\n');

    // 验收标准。
    out.push_str("## 验收标准\n\n");
    out.push_str(&acceptance_template(&req.infra_type));
    out.push('\n');

    out.push_str("---\n\n");
    out.push_str("> **人工确认流程**:审查本文件 → `ldp requirements confirm ");
    out.push_str(&req.id);
    out.push_str("` 标 `confirmed` → 进入实现 → 实现后把对应 gaps.json 条目的 `status` 改 `landed` 并跑 `ldp check-data && gen-doc`。\n");
    out.push_str("> 分类若不对,编辑 `data/gaps.json` 对应 subsystem 的 `infra_type` 字段后重新 `ldp requirements gen`。\n");

    // 幂等:内容不变不写。
    if std::fs::read_to_string(&path).unwrap_or_default() == out {
        return Ok(());
    }
    std::fs::write(&path, out).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(())
}

/// 生成「分类依据(provider 判据)」节。provider 类 issue 专用。
/// 列五判据 + 该 gap 的已记录评估(✓/✗/?)。未记录的留 unknown
/// (b 选项:渐进填充)。把「该不该抽 provider」从默认值变成必须逐条过的清单。
fn provider_criteria_section(pb: &crate::data::ProviderBoundary, gap_id: &str) -> String {
    let mut s = String::new();
    s.push_str("## 分类依据(provider 判据)\n\n");
    s.push_str("该缺口被归为 provider 类。在确认前,请逐条核对判据——");
    s.push_str(
        "**五条全 pass 才值得抽 provider;任一 fail → 改归类(改 gaps.json 的 infra_type)。**\n\n",
    );
    s.push_str("| 判据 | 问题 | 本 gap 评估 |\n");
    s.push_str("|------|------|------------|\n");
    let assessment = if !gap_id.is_empty() {
        pb.per_gap_assessment.get(gap_id)
    } else {
        None
    };
    for c in &pb.criteria {
        let verdict_cell = assessment
            .and_then(|a| a.get(&c.id))
            .and_then(|v| v.as_str())
            .map(render_verdict)
            .unwrap_or_else(|| "? 待评估".to_string());
        s.push_str(&format!(
            "| **{}** | {} | {} |\n",
            c.name, c.question, verdict_cell
        ));
    }
    s.push('\n');
    // 已记录的 note。
    if let Some(note) = assessment
        .and_then(|a| a.get("note"))
        .and_then(|v| v.as_str())
    {
        s.push_str(&format!("> 已记录论证:{}\n\n", note));
    } else {
        s.push_str("> ⚠ 未逐条论证(默认归类)。请据实填写 `data/provider-boundary.json` 的 ");
        s.push_str(&format!(
            "`per_gap_assessment.{gap_id}` 后重新 `ldp requirements gen`。\n\n"
        ));
    }
    s.push_str("判据参考(完整边界表 + A/B/concrete 三分见 `data/provider-boundary.json`):\n");
    s.push_str(
        "- A 类(provider 能力):横向「提供什么能力」,多 provider × 多消费者,OS-agnostic 目标。\n",
    );
    s.push_str(
        "- B 类(device class 契约):纵向「是什么设备」,每内核重写数据面,**不是 provider**。\n",
    );
    s.push_str("- concrete(具体驱动):寄存器层复用 + 数据面重写,**不是 provider**。\n\n");
    s.push_str(&impl_pitfalls_section(pb));
    s
}

/// 生成「实现质量清单(cross-kernel 坑)」节。provider 类 issue 在分类判据之后渲染。
/// 这是 Stage 2:provider 已确认该抽之后,怎么避免「半成品 OS-agnostic」。
/// 三坑从 clk 实证归纳(firmware-specific-key/incomplete-trait/global-static-registry)。
fn impl_pitfalls_section(pb: &crate::data::ProviderBoundary) -> String {
    let mut s = String::new();
    let Some(pits) = pb.impl_pitfalls.get("pits").and_then(|v| v.as_array()) else {
        return s; // 无 pits 定义则不渲染
    };
    s.push_str("## 实现质量清单(cross-kernel 坑,Stage 2)\n\n");
    s.push_str("provider 已确认该抽。**实现时逐条对照,避免「名义 OS-agnostic 实际被绑死」**:\n\n");
    s.push_str("| 坑 | 错误做法 | 正确做法 | 机械查? |\n");
    s.push_str("|----|---------|---------|--------|\n");
    for p in pits {
        let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let wrong = p.get("wrong").and_then(|v| v.as_str()).unwrap_or("");
        let right = p.get("right").and_then(|v| v.as_str()).unwrap_or("");
        let check = p.get("check").and_then(|v| v.as_str()).unwrap_or("人工");
        s.push_str(&format!("| **{name}** | {wrong} | {right} | {check} |\n"));
    }
    s.push('\n');
    if let Some(redline) = pb.impl_pitfalls.get("redline") {
        let rule = redline.get("rule").and_then(|v| v.as_str()).unwrap_or("");
        let cmd = redline
            .get("cmd")
            .and_then(|v| v.as_str())
            .unwrap_or("ldp check-provider-crate <Cargo.toml>");
        s.push_str(&format!("> 🔴 红线(机械可查):{rule}\n>   `{cmd}`\n\n"));
    }
    s.push_str("三坑从 clk 实证归纳(详见 `data/provider-boundary.json` 的 impl_pitfalls)。\n\n");
    s
}

fn render_verdict(v: &str) -> String {
    match v {
        "pass" => "✓ pass".to_string(),
        "fail" => "✗ fail(改归类!)".to_string(),
        _ => "? 待评估".to_string(),
    }
}

/// 按 infra_type 给实现路径模板。
fn impl_path_template(infra: &str, _gap: Option<&GapSubsystem>) -> &'static str {
    match infra {
        "multi-provider" => {
            "按 clk 模式(provider 模式):\n\
             1. 定义 `Provider` trait(核心操作,如 `get_rate`/`enable`/`set_rate`)。\n\
             2. 建 **phandle 键的多 provider 注册表**(`SpinNoIrq<Vec<(u32, Arc<dyn Provider>)>>`),\n\
             \t因为这类子系统有多个生产者(多个 controller/PHY/regulator)。\n\
             3. consumer API 走 device-res 形式:`devm_get(device, name)`,总线把 DT\n\
             \t(`clocks`/`resets`/`phys`/`*-supply`)预解析成 `ResourceDesc` 变体,驱动不碰 DT。\n\
             4. DT 解析放总线层(`khal::firmware::devices`),平台 provider 在 `early_driver_init` 注册。\n\
             5. 参考:`drivers/clk/`(Tier-0 实现)。\n\n\
             > 这是工作量最大的一类(约 1-2k 行),因为它要建一整套 provider/consumer 框架。\n"
        }
        "single-provider" => {
            "按 device-res/dma 模式(单全局 provider):\n\
             1. 定义 `Provider` trait。\n\
             2. 用单全局 provider(`Once<&dyn Provider>`),因为这类子系统整个系统只有一个后端\n\
             \t(如 DMA、MMIO 映射方式)。\n\
             3. consumer API:`devm_*` 形式,生命周期挂设备。\n\
             4. 参考:`drivers/device-res/`。\n"
        }
        "framework" => {
            "关键子框架(非 provider 模式,需具体框架设计):\n\
             1. 识别这个框架的核心抽象(如 regmap=寄存器 RMW,workqueue=延后执行,timer=定时回调)。\n\
             2. 设计 trait + 实现,通常基于现有原语(`Io`/`ktask`/`WaitQueue`)。\n\
             3. 不需要 provider 注册表或新 DeviceKind。\n\
             4. 工作量因框架而异,先评估是否值得建框架 vs. 驱动侧就地替代(见 gaps.json 的 workaround)。\n"
        }
        "subsystem-class" => {
            "新设备类(需扩展 DeviceKind):\n\
             1. 在 `DeviceKind` 加新变体(如 `I2c`/`Spi`/`Watchdog`/`Rtc`)。\n\
             2. 定义该类的 trait(如 `I2cController`),含总线操作。\n\
             3. 加 `kclass::publish_<class>(device, Box::new(dev))` 宏。\n\
             4. 子设备(总线上的从设备)如何挂载是设计重点(如 i2c controller 要能注册子 i2c 设备)。\n\
             5. 参考:`publish_block`/`publish_net`/`publish_char` 现有模式。\n\
             \t**这是 RK3588 i2c/spi 移植最常见的 blocker。**\n"
        }
        "driver-missing" => {
            "单纯驱动缺失(基础设施都在):\n\
             1. 底层 DeviceKind + publish_* + 所需子系统都已有,只是这个具体驱动没移植。\n\
             2. 直接按 `templates/driver-skeleton.md` 走 Phase 0-6,不动基础设施。\n\
             3. 不需要新建框架;本 issue 的价值在于记录「哪些驱动待移植」的 backlog。\n"
        }
        _ => "分类未知(待人工在 gaps.json 设 `infra_type`)。参考其它 issue 的实现路径模板。\n",
    }
}

/// 验收标准模板(精简,按类给可检查项)。
fn acceptance_template(infra: &str) -> String {
    let common = "- [ ] `ldp check-data && ldp check-bitfields && ldp golden` 全过\n\
                  - [ ] gaps.json 对应条目 `status` 改 `landed`\n";
    let specific = match infra {
        "multi-provider" | "single-provider" => {
            "- [ ] Provider trait + 注册表实现\n\
             - [ ] consumer API(`devm_*`)可用,至少一个驱动验证\n\
             - [ ] DT 解析在总线层生效(驱动不碰 DT)\n"
        }
        "framework" => "- [ ] 框架核心抽象可用\n- [ ] 至少一个驱动用新框架替代 workaround\n",
        "subsystem-class" => {
            "- [ ] DeviceKind 新变体已加\n\
             - [ ] publish_* 宏可用\n\
             - [ ] 至少一个 controller 驱动 + 子设备挂载验证\n"
        }
        "driver-missing" => "- [ ] 驱动按 skeleton 实现,build + clippy 过\n- [ ] P6 带机验证\n",
        _ => "",
    };
    format!("{common}{specific}")
}

impl Requirement {
    pub fn status_emoji(&self) -> &str {
        match self.status.as_str() {
            "proposed" => "📝",
            "confirmed" => "✅",
            "in-progress" => "🔧",
            "landed" => "🎉",
            "rejected" => "❌",
            _ => "❓",
        }
    }
}

/// 列所有需求(按状态分组可过滤)。
pub fn list(status_filter: Option<&str>) -> Vec<Requirement> {
    let idx = load_index();
    let mut reqs = idx.requirements;
    if let Some(st) = status_filter {
        reqs.retain(|r| r.status == st);
    }
    // 排序:状态优先级(proposed 先) → id。
    reqs.sort_by(|a, b| {
        status_rank(&a.status)
            .cmp(&status_rank(&b.status))
            .then(a.id.cmp(&b.id))
    });
    reqs
}

fn status_rank(s: &str) -> u8 {
    match s {
        "proposed" => 0,
        "confirmed" => 1,
        "in-progress" => 2,
        "landed" => 3,
        "rejected" => 4,
        _ => 5,
    }
}

/// 标一个需求的状态(人工确认)。返回更新后的要求。
pub fn set_status(id: &str, status: &str) -> Result<Requirement, String> {
    let valid = ["proposed", "confirmed", "in-progress", "landed", "rejected"];
    if !valid.contains(&status) {
        return Err(format!("状态 `{status}` 无效,应为 {:?}", valid));
    }
    let mut idx = load_index();
    let r = idx
        .requirements
        .iter_mut()
        .find(|r| r.id == id)
        .ok_or_else(|| format!("无需求 id=`{id}`。跑 `ldp requirements list` 看可用 id"))?;
    if r.status == "landed" || r.status == "rejected" {
        return Err(format!("需求 `{id}` 已 `{}`(终态,避免重复变更)", r.status));
    }
    r.status = status.to_string();
    if status == "confirmed" && r.confirmed_ts.is_none() {
        r.confirmed_ts = Some(now_secs());
    }
    let snapshot = r.clone();
    write_index(&idx)?;
    Ok(snapshot)
}
