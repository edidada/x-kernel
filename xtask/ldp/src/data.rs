// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 数据层(`data/*.json`)的反序列化模型。
//!
//! 这些结构体是为了**忠实建模 JSON schema**而存在的;部分字段当前未被命令消费,
//! 但保留它们以保证真相源完整、向前兼容。字段语义见
//! `docs/ai/skills/linux-driver-port/data/schema.md`。

#![allow(dead_code)]

use serde::Deserialize;

/// 单条 Linux → x-kernel API 映射。
#[derive(Debug, Clone, Deserialize)]
pub struct MappingRow {
    pub linux: String,
    pub xkernel: String,
    #[serde(default)]
    pub note: String,
    pub source: Option<Source>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default = "default_status_landed")]
    pub status: String,
}

fn default_status_landed() -> String {
    "landed".to_string()
}

/// 源码证据(铁律 2 的数据化)。
#[derive(Debug, Clone, Deserialize)]
pub struct Source {
    pub path: String,
    pub line: Option<u32>,
}

/// api-mapping.json 的一个分区。
#[derive(Debug, Clone, Deserialize)]
pub struct MappingSection {
    pub id: String,
    pub title: String,
    pub order: u32,
    pub rows: Vec<MappingRow>,
}

/// api-mapping.json 顶层。
#[derive(Debug, Clone, Deserialize)]
pub struct ApiMapping {
    #[serde(default)]
    pub conventions: serde_json::Value,
    pub sections: Vec<MappingSection>,
}

/// gaps.json 里一个缺失子系统。
#[derive(Debug, Clone, Deserialize)]
pub struct GapSubsystem {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub symbols: Vec<String>,
    pub status: String,
    #[serde(default)]
    pub xkernel_status: String,
    #[serde(default)]
    pub severity: String,
    #[serde(default)]
    pub workaround: String,
    #[serde(default)]
    pub report_template: String,
    #[serde(default)]
    pub context: Vec<String>,
    #[serde(default)]
    pub dimensions: Vec<GapDimension>,
    /// clk 用 dimensions;gpio/reset/regulator/iommu/phy/device-class 用 severity_branches
    /// (一个 map:分支名 → {severity, when?, label?})。两者表达同一件事的不同形状。
    #[serde(default)]
    pub severity_branches: serde_json::Value,
    /// 基础设施类型:multi-provider / single-provider / framework / subsystem-class / driver-missing。
    /// 决定需求 issue 的「建议实现路径」(requirements gen 据此选模板)。推断规则见
    /// data/schema.md。
    #[serde(default)]
    pub infra_type: String,
}

/// clk 这类按维度分级的子系统用。
#[derive(Debug, Clone, Deserialize)]
pub struct GapDimension {
    pub name: String,
    pub linux: Vec<String>,
    pub severity: String,
    #[serde(default)]
    pub workaround: String,
    #[serde(default)]
    pub report_template: String,
}

/// gaps.json 顶层。
#[derive(Debug, Clone, Deserialize)]
pub struct Gaps {
    pub subsystems: Vec<GapSubsystem>,
    #[serde(default)]
    pub cascade_gaps: Vec<serde_json::Value>,
}

/// 布局字段声明:移植方声称该字段占 [lsb, msb],并锚到参考符号。
///
/// `ref_symbol` 指向参考树里一个机械可导出的符号(Linux 的 `#define`
/// `MSG_ID_MASK GENMASK(7,0)` 等);`claim` 是移植方写进 JSON 的声明,
/// 由 `ldp check-layout` 与参考导出真值比对(以及可选地与驱动代码实际
/// 写法比对)。
#[derive(Debug, Clone, Deserialize)]
pub struct FieldClaim {
    pub name: String,
    /// 参考树内的符号名(如 `MSG_ID_MASK`)。提取器据参考文件机械导出
    /// 它的 [lsb, msb] 真值,与 `claim` 比对。
    pub ref_symbol: String,
    /// 移植方声明。`lsb`/`msb` 是被验证的断言(抄对了吗),非自由字段。
    pub claim: FieldSpan,
}

/// 一个字段占的位段 [lsb, msb](闭区间,与 GENMASK 一致)。
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub struct FieldSpan {
    pub lsb: u32,
    pub msb: u32,
}

impl FieldSpan {
    /// 该位段的掩码(若 msb>=63 则返回全 1)。
    pub fn mask(&self) -> u64 {
        let width = self.msb.saturating_sub(self.lsb) + 1;
        if width >= 64 {
            u64::MAX
        } else {
            (1u64 << width) - 1
        }
    }
    pub fn width(&self) -> u32 {
        self.msb.saturating_sub(self.lsb) + 1
    }
}

/// 某个寄存器/协议 wire 布局的参考锚定声明。
///
/// 旧的 `known_wrong_layouts` / `verification_traces` 自指范式已删除:
/// 布局真值由 `ref` 钉死的参考树机械导出,`fields[].claim` 是被验证的
/// 移植方声明,不再靠人手写"我猜会怎么记错"。
#[derive(Debug, Clone, Deserialize)]
pub struct BitfieldDef {
    pub id: String,
    pub name: String,
    /// `protocol`(全局共享,如 SCMI header)或 `port`(某次移植专属)。
    #[serde(default = "default_scope_protocol")]
    pub scope: String,
    /// `scope=port` 时必填;该布局属于哪个移植任务。
    #[serde(default)]
    pub port: String,
    /// 参考锚:`{tree, file}`。`tree` 是 references.json 里注册的参考树 id;
    /// `file` 是该树内的相对路径。check-layout 据此提取真值。
    pub r#ref: LayoutRef,
    /// 可选:驱动代码锚。填了则 check-layout 额外提取驱动代码里的位声明,
    /// 与 claim 比对(D2:驱动实现 ↔ 声明)。
    #[serde(default)]
    pub driver_src: Option<DriverSrc>,
    pub width: u32,
    #[serde(default)]
    pub spec_ref: String,
    pub fields: Vec<FieldClaim>,
    /// 人类文档(不承重):如状态码映射、诊断备注。
    #[serde(default)]
    pub status_codes: serde_json::Value,
    #[serde(default)]
    pub diagnostic_note: String,
}

fn default_scope_protocol() -> String {
    "protocol".to_string()
}

/// 参考锚:钉到某个参考树的某个文件。
#[derive(Debug, Clone, Deserialize)]
pub struct LayoutRef {
    /// references.json 里某 `trees[].id`。
    pub tree: String,
    /// 参考树内相对路径(如 `drivers/firmware/arm_scmi/common.h`)。
    pub file: String,
}

/// 驱动代码锚:D2 声明侧来源。
/// `anchor` 指向驱动里的一个 `const fn`(位段打包函数)或 `const`(单字段),
/// 提取器从其位表达式抽出实际位声明。
#[derive(Debug, Clone, Deserialize)]
pub struct DriverSrc {
    pub path: String,
    pub anchor: String,
}

/// bitfield-defs.json 顶层。
#[derive(Debug, Clone, Deserialize)]
pub struct BitfieldDefs {
    pub defs: Vec<BitfieldDef>,
}

/// references.json:参考树注册表。
///
/// 把"以 Linux/U-Boot 源码为参考"变成机器可验:每个参考必须是本地目录
/// (submodule 或 vendored 快照),带 git pin。拒绝 web search —— 缺参考
/// 即 fail-closed,这正是"太容易忘了"的结构性解法:必须在场才能过。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct References {
    #[serde(default)]
    pub trees: Vec<Reference>,
}

/// 一棵参考树。
#[derive(Debug, Clone, Deserialize)]
pub struct Reference {
    pub id: String,
    /// 仓库内路径(submodule 或 vendored)。必须真实存在。
    pub path: String,
    /// `linux` / `u-boot` / `other`。决定提取器期望的文件形态。
    #[serde(default = "default_kind_linux")]
    pub kind: String,
    /// git pin(tag/commit)。submodule 则由 .gitmodules 钉,vendored 则手动记。
    #[serde(default)]
    pub pin: String,
    /// `true` = 缺它或 pin 不匹配即 fail-closed;`false` = 仅提示。
    #[serde(default = "default_true")]
    pub required: bool,
}

fn default_kind_linux() -> String {
    "linux".to_string()
}
fn default_true() -> bool {
    true
}

impl MappingRow {
    /// 该条映射所有可检索的词面(Ch3 上下文前缀)。
    pub fn terms(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.keywords.iter().map(|s| s.as_str()).collect();
        v.push(self.linux.as_str());
        v.push(self.xkernel.as_str());
        v
    }
}

impl FieldClaim {
    /// 该字段在布局里的位段(取自 claim)。
    pub fn span(&self) -> FieldSpan {
        self.claim
    }
}

/// provider-boundary.json 里一条判据。
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderCriterion {
    pub id: String,
    pub name: String,
    pub question: String,
    #[serde(default)]
    pub pass_means: String,
    #[serde(default)]
    pub fail_means: String,
}

/// provider-boundary.json 顶层。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ProviderBoundary {
    #[serde(default)]
    pub criteria: Vec<ProviderCriterion>,
    /// per_gap_assessment 是 object,键为 gap_id,值为 {criterion_id: verdict}。
    /// verdict 为 "pass"/"fail"/"unknown"。保留为 Value 做轻量查询。
    #[serde(default)]
    pub per_gap_assessment: serde_json::Value,
    /// Stage 2 实现质量坑(firmware-specific-key/incomplete-trait/global-static-registry)
    /// + redline。保留为 Value 做 light 渲染。与 criteria(Stage 1)分开。
    #[serde(default)]
    pub impl_pitfalls: serde_json::Value,
}
