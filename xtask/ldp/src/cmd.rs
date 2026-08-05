// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 子命令分发与自描述 help(主动工具发现,Ch4 rule 6)。

use std::process::ExitCode;

use crate::Cmd;
use crate::data::BitfieldDefs;
use crate::{RefAction, RequirementsAction, StagingAction, TraceAction};
use crate::{data, gendoc, golden, layout, query, references, repo, requirements, staging, trace, validate};

/// 主动工具发现:打印每个子命令的用途 + 边界(反例)+ 示例。
pub fn help() {
    println!("ldp — linux-driver-port skill harness");
    println!("数据层(真相源): docs/ai/skills/linux-driver-port/data/*.json");
    println!();
    println!("子命令(每条都标注了边界/反例):");
    println!();
    println!("  summary                  知识库概览(常驻级)。不列细节。");
    println!("  lookup <api>             查 Linux API 的 x-kernel 对应(结构化候选+截断)。");
    println!("                           不查缺口;缺口用 gaps。");
    println!("  gaps [--status S] [--id] 列/筛缺失子系统。不扫源码;源码用 scan。");
    println!("  check-data               数据完整性(JSON/source.path/字段)。fail-closed。");
    println!("                           不验位段布局;布局用 check-layout。");
    println!("  ref list/check           参考树注册表(本地 Linux/U-Boot 源码)。fail-closed。");
    println!("                           必须本地目录(拒绝 web search);vendor 方法见 data/references/README.md。");
    println!("  check-layout [--port]    布局声明 ↔ 参考真值(以及可选驱动代码)。fail-closed。");
    println!("                           从钉死参考机械导出位段,diff claim;取代旧 check-bitfields 自指范式。");
    println!("                           旧 check-bitfields 为兼容别名(转发)。",);
    println!(
        "  check-provider-crate <f> 机械查 provider crate 的 OS-agnostic 红线(不得依赖 k*/of)。"
    );
    println!(
        "                           只查依赖红线;trait 完整性/注册表可注入是设计判断,走需求 issue。"
    );
    println!("  scan <path>              扫一份 Linux 源码里用到的缺口符号。只提示不改文件。");
    println!("  gen-doc                  从 data/*.json 生成 knowledge/*.md(数据为源)。");
    println!("                           不改 JSON;真相源在 JSON。");
    println!("  golden [name]            跑 golden 回归(walkthrough 断言对照数据)。");
    println!("                           不编译;只查断言一致性。");
    println!("  trace add/list/show      记 port 轨迹(Ch8 在线循环,append-only)。");
    println!("                           不并入正式 data/;发布用 staging promote。");
    println!("  staging aggregate/review/promote  轨迹→候选→门槛发布(Ch8 离线循环)。");
    println!("                           promote 不自动改 data/;门槛≥2 支持。");
    println!("  requirements gen/list/show/confirm  发现缺口→需求 issue→人工确认。");
    println!("                           gen 不改 gaps.json;confirm 才改状态。");
    println!();
    println!("示例:");
    println!("  ldp lookup readl");
    println!("  ldp gaps --status missing");
    println!("  ldp check-bitfields");
    println!("  ldp scan drivers/mmc/host/sdhci-of-dwcmshc.c");
    println!("  ldp golden dwcmshc");
    println!("  ldp gen-doc");
    println!(
        "  ldp trace add dw_wdt-rk3588 --kind gap --linux clk_set_rate --note 'probe needs rate'"
    );
    println!("  ldp staging aggregate && ldp staging review");
    println!("  ldp requirements gen --from-scan drivers/net/ethernet/foo.c");
    println!("  ldp requirements list");
    println!("  ldp requirements confirm req-clk-framework --status confirmed");
    println!();
    println!("五条铁律里有四条已被 ldp 变成会失败的检查:");
    println!("  铁律2 不编造 API   → check-data(source.path 必须存在)");
    println!("  铁律1/2 位段忠实   → check-layout(从钉死参考机械导出位段,diff claim/驱动)");
    println!("  铁律4 缺口必报     → scan(源码里用到的缺口符号强制提示)");
    println!("  数据/文档一致性    → golden + gen-doc(walkthrough 不漂移,md 不手改)");
    println!("  经验沉淀           → trace + staging(≥2 条轨迹支持才发布,Ch8 进化闭环)");
}

/// 运行一个子命令,返回退出码。
pub fn run(cmd: Cmd) -> ExitCode {
    match cmd {
        Cmd::Summary => with_data(|api, gaps, bf| {
            let ngolden = golden::list_goldens().len();
            query::summary(&api, &gaps, bf.defs.len(), ngolden);
            0
        }),
        Cmd::Lookup { query: q } => with_data(|api, _gaps, _bf| match query::lookup(&api, &q) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {e}");
                1
            }
        }),
        Cmd::Gaps { status, id } => with_data(|_api, gaps, _bf| {
            query::list_gaps(&gaps, status.as_deref(), id.as_deref());
            0
        }),
        Cmd::CheckData => with_data(|api, gaps, bf| validate::check_data(&api, &gaps, &bf)),
        Cmd::CheckLayout { port } => {
            with_data(move |_api, _gaps, bf| layout::check_layout(&bf, port.as_deref()))
        }
        Cmd::CheckBitfields { port } => {
            // 兼容别名:旧 check-bitfields 的自指逻辑已删除,转发到 check-layout。
            with_data(move |_api, _gaps, bf| layout::check_layout(&bf, port.as_deref()))
        }
        Cmd::Scan { path } => {
            with_data(
                |_api, gaps, _bf| match validate::scan_source(&gaps, &path) {
                    Ok(()) => 0,
                    Err(e) => {
                        eprintln!("error: {e}");
                        1
                    }
                },
            )
        }
        Cmd::CheckProviderCrate { path } => {
            let n = validate::check_provider_crate(&path);
            if n == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Cmd::GenDoc => with_data(|api, gaps, _bf| {
            gendoc::gen_api_mapping(&api);
            gendoc::gen_gaps(&gaps);
            println!("完成。knowledge/api-mapping.md 与 knowledge/gaps.md 已与 data/*.json 同步。");
            0
        }),
        Cmd::Golden { name } => {
            let names = golden::list_goldens();
            if names.is_empty() {
                eprintln!(
                    "error: 没有找到 golden 基线({})",
                    repo::golden_dir().display()
                );
                return ExitCode::from(1);
            }
            match name {
                Some(n) => {
                    if !names.contains(&n) {
                        eprintln!("error: 没有 golden 基线 `{n}`。可用: {}", names.join(", "));
                        return ExitCode::from(1);
                    }
                    with_data(|api, gaps, _bf| golden::run_golden(&n, &api, &gaps))
                }
                None => {
                    let api = match repo::load_api_mapping() {
                        Ok(a) => a,
                        Err(e) => {
                            eprintln!("error: {e}");
                            return ExitCode::from(1);
                        }
                    };
                    let gaps = match repo::load_gaps() {
                        Ok(g) => g,
                        Err(e) => {
                            eprintln!("error: {e}");
                            return ExitCode::from(1);
                        }
                    };
                    let mut total = 0usize;
                    for n in &names {
                        total += golden::run_golden(n, &api, &gaps);
                    }
                    if total == 0 {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::from(1)
                    }
                }
            }
        }
        Cmd::Help => {
            help();
            ExitCode::SUCCESS
        }
        Cmd::Ref { action } => run_ref(action),
        Cmd::Trace { action } => run_trace(action),
        Cmd::Staging { action } => run_staging(action),
        Cmd::Requirements { action } => run_requirements(action),
    }
}

/// 参考树注册表。
fn run_ref(action: RefAction) -> ExitCode {
    match action {
        RefAction::List => {
            references::list();
            ExitCode::SUCCESS
        }
        RefAction::Check => {
            let n = references::check();
            if n == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
    }
}

/// 在线循环:轨迹记录/查看。
fn run_trace(action: TraceAction) -> ExitCode {
    match action {
        TraceAction::Add {
            port,
            kind,
            linux,
            note,
            evidence,
        } => {
            let kind = match trace::TraceKind::parse(&kind) {
                Ok(k) => k,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::from(1);
                }
            };
            match trace::add(
                &port,
                kind,
                &linux,
                note.as_deref().unwrap_or(""),
                evidence.as_deref(),
            ) {
                Ok(()) => {
                    let p = trace::trace_path(&port)
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    println!("已追加 → {p}");
                    println!(
                        "  (轨迹不可变;发布进正式 data/ 需 `ldp staging aggregate && ldp staging promote`)"
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::from(1)
                }
            }
        }
        TraceAction::List => {
            let names = trace::list();
            if names.is_empty() {
                println!("(无轨迹文件:{})", trace::trace_dir().display());
                return ExitCode::SUCCESS;
            }
            println!("轨迹文件({}):", names.len());
            for n in &names {
                let cnt = trace::read(n).map(|v| v.len()).unwrap_or(0);
                println!("  {n}: {cnt} 条事件");
            }
            ExitCode::SUCCESS
        }
        TraceAction::Show { port } => match trace::read(&port) {
            Ok(events) => {
                if events.is_empty() {
                    println!("(轨迹 {port} 为空)");
                    return ExitCode::SUCCESS;
                }
                println!("轨迹 {port}({} 条事件):", events.len());
                for ev in &events {
                    let ev_txt = if ev.note.is_empty() {
                        String::new()
                    } else {
                        format!(" — {}", ev.note)
                    };
                    let ev_src = ev
                        .evidence
                        .as_deref()
                        .map(|s| format!(" [evidence: {s}]"))
                        .unwrap_or_default();
                    println!(
                        "  [{}] {}:{}{}{}",
                        ev.ts,
                        ev.kind.as_str(),
                        ev.linux,
                        ev_txt,
                        ev_src
                    );
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(1)
            }
        },
    }
}

/// 离线循环:聚合 → 候选 → 门槛发布。
fn run_staging(action: StagingAction) -> ExitCode {
    match action {
        StagingAction::Aggregate => match staging::aggregate() {
            Ok(s) => {
                let promotable = s
                    .candidates
                    .iter()
                    .filter(|c| {
                        c.support_count >= staging::PROMOTE_THRESHOLD && c.status != "promoted"
                    })
                    .count();
                println!(
                    "聚合完成:{} 个候选({} 个可发布:支持数≥{},未 promoted)。文件:{}",
                    s.candidates.len(),
                    promotable,
                    staging::PROMOTE_THRESHOLD,
                    staging::staging_path().display()
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(1)
            }
        },
        StagingAction::Review => {
            let s = staging::load();
            if s.candidates.is_empty() {
                println!("(无候选;先跑 `ldp staging aggregate`)");
                return ExitCode::SUCCESS;
            }
            println!(
                "候选({} 个,发布门槛 = 支持数≥{} 条不同 port 轨迹):",
                s.candidates.len(),
                staging::PROMOTE_THRESHOLD
            );
            println!("────────────────────────────────────────");
            for c in &s.candidates {
                let ready = c.support_count >= staging::PROMOTE_THRESHOLD && c.status != "promoted";
                let mark = if c.status == "promoted" {
                    "✓ promoted"
                } else if ready {
                    "★ 可发布"
                } else {
                    "· 候选"
                };
                let sources = c.sources.join(", ");
                println!(
                    "{mark} [{}] {}  (支持 {}: {})",
                    c.kind, c.linux, c.support_count, sources
                );
                if !c.note.is_empty() {
                    println!("        note: {}", c.note);
                }
            }
            ExitCode::SUCCESS
        }
        StagingAction::Promote { id } => match staging::promote(&id) {
            Ok(c) => {
                staging::promote_guidance(&c);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(1)
            }
        },
    }
}

/// 需求 issue 生成/确认。
fn run_requirements(action: RequirementsAction) -> ExitCode {
    match action {
        RequirementsAction::Gen {
            from_scan,
            from_trace,
        } => {
            let gaps = match repo::load_gaps() {
                Ok(g) => g,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::from(1);
                }
            };
            let pb = match repo::load_provider_boundary() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::from(1);
                }
            };
            let findings = match (from_scan, from_trace) {
                (Some(path), None) => match requirements::findings_from_scan(&gaps, &path) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("error: {e}");
                        return ExitCode::from(1);
                    }
                },
                (None, Some(port)) => match requirements::findings_from_trace(&port) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("error: {e}");
                        return ExitCode::from(1);
                    }
                },
                (None, None) => {
                    eprintln!("error: 需要 --from-scan <path> 或 --from-trace <port>");
                    return ExitCode::from(1);
                }
                (Some(_), Some(_)) => {
                    eprintln!("error: --from-scan 与 --from-trace 互斥,只用一个");
                    return ExitCode::from(1);
                }
            };
            if findings.is_empty() {
                println!("未发现缺口(没有命中任何已知 gap 符号)。这份源码可能只用主干 API。");
                return ExitCode::SUCCESS;
            }
            match requirements::gen_issues(&gaps, &pb, &findings) {
                Ok((_reqs, new, updated)) => {
                    println!(
                        "生成需求 issue:{} 个(新建 {}, 更新 {})。目录:{}",
                        new + updated,
                        new,
                        updated,
                        requirements::requirements_dir().display()
                    );
                    println!(
                        "下一步:`ldp requirements list` 看全部,`ldp requirements confirm <id> --status confirmed` 人工确认。"
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::from(1)
                }
            }
        }
        RequirementsAction::List { status } => {
            let reqs = requirements::list(status.as_deref());
            if reqs.is_empty() {
                println!(
                    "(无需求 issue。跑 `ldp requirements gen --from-scan <linux源码>` 生成。)"
                );
                return ExitCode::SUCCESS;
            }
            let no_filter = status.is_none();
            let filt = status.map(|s| format!(" (状态={s})")).unwrap_or_default();
            println!("需求 issue({} 个{}):", reqs.len(), filt);
            println!("────────────────────────────────────────");
            for r in &reqs {
                println!(
                    "{} [{}] {} — {}",
                    r.status_emoji(),
                    r.infra_type,
                    r.title,
                    r.id
                );
                println!("        来源: {} | 触发: {}", r.source, r.symbol);
            }
            let proposed = reqs.iter().filter(|r| r.status == "proposed").count();
            if proposed > 0 && no_filter {
                println!("\n⚠ {proposed} 个待人工确认(proposed)。确认后才能进入实现。 ");
            }
            ExitCode::SUCCESS
        }
        RequirementsAction::Show { id } => {
            let idx = requirements::list(None);
            match idx.iter().find(|r| r.id == id) {
                Some(r) => {
                    let path = requirements::requirements_dir().join(&r.file);
                    println!("需求 {} → {}", r.id, path.display());
                    match std::fs::read_to_string(&path) {
                        Ok(content) => print!("{content}"),
                        Err(e) => eprintln!("读文件失败: {e}"),
                    }
                    ExitCode::SUCCESS
                }
                None => {
                    eprintln!("error: 无需求 id=`{id}`");
                    ExitCode::from(1)
                }
            }
        }
        RequirementsAction::Confirm { id, status } => {
            match requirements::set_status(&id, &status) {
                Ok(r) => {
                    println!("✓ 需求 `{}` 状态 → `{}`", r.id, r.status);
                    if r.status == "confirmed" {
                        println!("  已确认。进入实现时记得:");
                        println!("  - 实现后把 gaps.json 对应 subsystem 的 status 改 landed");
                        println!("  - 跑 `ldp check-data && ldp gen-doc` 同步");
                        println!(
                            "  - `ldp requirements confirm {} --status landed` 收尾",
                            r.id
                        );
                    } else if r.status == "landed" {
                        println!("  🎉 该需求已落地。");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::from(1)
                }
            }
        }
    }
}

/// 加载全部三类数据后执行 f;任一加载失败 → 打印并返回退出码 1。
fn with_data<F>(f: F) -> ExitCode
where
    F: FnOnce(data::ApiMapping, data::Gaps, BitfieldDefs) -> usize,
{
    let api = match repo::load_api_mapping() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let gaps = match repo::load_gaps() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let bf = match repo::load_bitfields() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let errs = f(api, gaps, bf);
    if errs == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
