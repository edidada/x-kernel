// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `ldp` — linux-driver-port skill 的 harness。
//!
//! 本二进制是 skill 的「工具 + 约束 + 验证」执行层(数据层在
//! `docs/ai/skills/linux-driver-port/data/*.json`)。把 SKILL.md 里的散文铁律
//! 变成会失败的检查:
//! - 位段 false-pass(SCMI VERSION 骗人那类)→ `check-bitfields`
//! - 编造的 API(source.path 不存在)→ `check-data`
//! - walkthrough 与数据漂移 → `golden`
//!
//! 用法见 `ldp help`。设计依据见 `docs/ai/skills/linux-driver-port/docs/design.md`。

use std::{path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};

mod bitfield;
mod cmd;
mod data;
mod extract;
mod gendoc;
mod golden;
mod layout;
mod query;
mod references;
mod repo;
mod requirements;
mod sequence;
mod staging;
mod trace;
mod validate;

#[derive(Parser)]
#[command(
    name = "ldp",
    version,
    about = "linux-driver-port skill harness:数据为源的查询 + 可执行约束 + 验证。",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// 打印全部子命令的用途、边界(反例)与示例。
    ///
    /// 反例:help 不会替你执行任何检查;要校验请用 check-data / check-bitfields / golden。
    Help,

    /// 知识库概览卡片(常驻级,Ch3 双层记忆)。
    ///
    /// 反例:summary 不列细节;查具体映射用 lookup,查缺口用 gaps。
    Summary,

    /// 查某条 Linux API 的 x-kernel 对应(结构化候选 + 显式截断)。
    ///
    /// 反例:lookup 只查 api-mapping.json;查缺口子系统的现状用 gaps。
    Lookup {
        /// 查询词,如 readl / clk_get_rate / of_match_table。
        query: String,
    },

    /// 列出/筛选缺失子系统。
    ///
    /// 反例:gaps 不扫你的源码;要扫一份 Linux 源码里用了哪些缺口符号,用 scan。
    Gaps {
        /// 按 status 过滤:open / workaround / partial / missing / landed。
        #[arg(long)]
        status: Option<String>,
        /// 按 id 精确取一条。
        #[arg(long)]
        id: Option<String>,
    },

    /// 校验数据完整性(JSON 可解析、source.path 存在、字段齐全)。fail-closed。
    ///
    /// 反例:check-data 不校验位段布局;布局用 check-layout。
    CheckData,

    /// 校验布局声明 ↔ 参考真值(以及可选的驱动代码)。fail-closed。
    ///
    /// 从钉死的本地参考树机械导出位段真值,diff claim。取代旧
    /// check-bitfields 的自指范式(检查 JSON 跟自己的关系)。
    ///
    /// 反例:check-layout 不验 source.path;数据完整性用 check-data。
    CheckLayout {
        /// 只查某个 port 的布局(scope=port);省略则只查 protocol 级。
        #[arg(long)]
        port: Option<String>,
    },

    /// [兼容别名] 等价于 `check-layout`。旧 check-bitfields 的自指逻辑已删除。
    CheckBitfields {
        /// 传给 check-layout 的 --port。
        #[arg(long, alias = "def")]
        port: Option<String>,
    },

    /// 机械查 provider crate 的 OS-agnostic 红线(Cargo.toml 不得依赖 k*/of)。fail-closed。
    ///
    /// 反例:只查依赖红线,不查 trait 完整性/注册表可注入(那是设计判断,走需求 issue)。
    CheckProviderCrate {
        /// provider crate 的 Cargo.toml 路径。
        path: PathBuf,
    },

    /// 在一份 Linux 源码(文件/目录)里扫出用到的、且当前是缺口的子系统符号。
    ///
    /// 铁律 4(缺口必报)的数据化:命中即提示。加 `--report <out.md>` 直接产出
    /// 缺口报告草稿(「先报告、后生成」的第一步);加 `--gate` 检查 blocker 决策门
    /// —— 命中的可阻塞缺口若无已决策需求 issue,退出码 1,禁止进入代码生成。
    ///
    /// 反例:scan 不判断某缺口在本驱动里是否真是 blocker(conditional 需人工
    /// 按实际用法选分支,记进需求 issue/报告);它只保证「缺口必须先有决策」。
    Scan {
        /// Linux 源码路径(文件或目录)。
        path: PathBuf,
        /// 把命中结果渲染成缺口报告草稿并写到这个 markdown 路径。
        #[arg(long)]
        report: Option<PathBuf>,
        /// 检查 blocker 决策门(存在未决策的可阻塞缺口即退出 1)。
        #[arg(long)]
        gate: bool,
    },

    /// 从 data/*.json 重新生成 knowledge/*.md(数据为源)。
    ///
    /// 反例:gen-doc 不改 data/*.json;真相源在 JSON,改 JSON 再重生成。
    GenDoc,

    /// 跑 golden 回归(把 walkthrough 的断言对照当前数据)。
    ///
    /// 反例:golden 不执行编译;它只查「walkthrough 声明的映射/缺口在数据里是否还在」。
    Golden {
        /// 基线名(如 dwcmshc)。省略则列全部基线。
        name: Option<String>,
    },

    /// 参考树注册表:把"以 Linux/U-Boot 源码为参考"变成机器可验
    /// (必须是本地 submodule/vendored 目录,拒绝 web search)。fail-closed。
    ///
    /// 反例:ref check 只验本地就位 + pin 标记;不联网核对远程。
    Ref {
        #[command(subcommand)]
        action: RefAction,
    },

    /// 记录/查看 port 轨迹(Ch8 在线循环,append-only 不可变)。
    ///
    /// 反例:trace 不会自动并入正式 data/;要发布用 `staging promote`。
    Trace {
        #[command(subcommand)]
        action: TraceAction,
    },

    /// 聚合轨迹 → 候选 → 门槛发布(Ch8 离线进化循环)。
    ///
    /// 反例:staging promote 不自动改 data/(Ch8 安全边界),只标 promoted + 打印指引。
    Staging {
        #[command(subcommand)]
        action: StagingAction,
    },

    /// 发现缺口 → 生成/确认需求 issue(可决策的需求文档)。
    ///
    /// 反例:gen 只生成 issue + 更新 index;confirm 才改状态;不自动改 gaps.json。
    Requirements {
        #[command(subcommand)]
        action: RequirementsAction,
    },

    /// 序列门禁：参考实现的有序副作用序列 vs v2 移植（覆盖/可达/顺序）。
    ///
    /// 反例：check-layout 只看位段形状，看不到"少调了一个初始化函数"。
    Sequence {
        #[command(subcommand)]
        action: SequenceAction,
    },
}

#[derive(Subcommand)]
enum RefAction {
    /// 列已注册参考树 + 本地状态(就位/缺失/pin)。
    List,
    /// 校验所有 required 参考本地就位且可确认 pin。fail-closed。
    Check,
}

#[derive(Subcommand)]
enum TraceAction {
    /// 追加一条事件到 <port>.jsonl(不可变,append-only)。
    Add {
        /// port 任务名(只允许字母数字/连字符/下划线),如 dw_wdt-rk3588。
        port: String,
        /// 事件种类:lookup-miss / gap / bitfield-pitfall。
        #[arg(long)]
        kind: String,
        /// Linux 符号,如 clk_get_rate 或 "SCMI message_id 位段"。
        #[arg(long)]
        linux: String,
        /// 上下文描述(可选)。
        #[arg(long)]
        note: Option<String>,
        /// 证据引用(Linux 源码位置/规范条款,可选)。
        #[arg(long)]
        evidence: Option<String>,
    },
    /// 列所有轨迹文件 + 事件计数。
    List,
    /// 看某 port 的全部事件。
    Show { port: String },
}

#[derive(Subcommand)]
enum StagingAction {
    /// 聚合所有轨迹 → candidates.json(保留已 promoted 状态)。
    Aggregate,
    /// 列候选,标出 ≥ 门槛可发布的。
    Review,
    /// 标一个候选 promoted + 打印入库指引(不自动改 data/)。
    Promote {
        /// 候选 id,如 `gap:clk_set_rate`。
        #[arg(long)]
        id: String,
    },
}

#[derive(Subcommand)]
enum SequenceAction {
    /// 从参考 C 函数抽取有序步骤，打印 TOML 草稿。
    Extract {
        /// 参考源码文件。
        file: PathBuf,
        /// 函数名，如 rtl_hw_init_8125。
        #[arg(long)]
        function: String,
        /// 归属的命名路径（init / start / stop / link / irq）。
        #[arg(long, default_value = "init")]
        path: String,
    },
    /// 校验 data/sequences/*.toml 的覆盖/可达/顺序。
    Check {
        /// 只查某个 port。
        #[arg(long)]
        port: Option<String>,
        /// unported 也判为失败（默认只告警）。
        #[arg(long)]
        strict: bool,
        /// 列出未移植的步骤。
        #[arg(long)]
        list_unported: bool,
    },
}

#[derive(Subcommand)]
enum RequirementsAction {
    /// 扫一份 Linux 源码/从轨迹发现缺口 → 生成需求 issue(每个缺口一个 md)。
    Gen {
        /// 从 scan 生成:Linux 源码路径。
        #[arg(long)]
        from_scan: Option<PathBuf>,
        /// 从 trace 生成:port 任务名。
        #[arg(long)]
        from_trace: Option<String>,
    },
    /// 列需求 issue(可按状态过滤)。
    List {
        /// 按 status 过滤:proposed/confirmed/in-progress/landed/rejected。
        #[arg(long)]
        status: Option<String>,
    },
    /// 看一个需求的详情。
    Show {
        /// 需求 id,如 `req-clk-framework`。
        id: String,
    },
    /// 标需求状态(人工确认)。
    Confirm {
        /// 需求 id。
        id: String,
        /// 新状态:confirmed/in-progress/landed/rejected。
        #[arg(long)]
        status: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        None => {
            cmd::help();
            ExitCode::SUCCESS
        }
        Some(Cmd::Help) => {
            cmd::help();
            ExitCode::SUCCESS
        }
        Some(c) => cmd::run(c),
    }
}
