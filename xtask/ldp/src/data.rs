// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Data model for the linux-driver-port skill knowledge base.
//!
//! `data/*.json` 是唯一真相源;本模块负责反序列化并提供查询。
//! 字段语义见 `docs/ai/skills/linux-driver-port/data/schema.md`。

//! 数据层(`data/*.json`)的反序列化模型。
//!
//! 这些结构体是为了**忠实建模 JSON schema**而存在的;部分字段当前未被命令消费,
//! 但保留它们以保证真相源完整、向前兼容。

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

/// 位段定义里的一个字段。
#[derive(Debug, Clone, Deserialize)]
pub struct BitField {
    pub name: String,
    pub lsb: u32,
    pub msb: u32,
    #[serde(default)]
    pub genmask: String,
}

/// 一条验证用例。
#[derive(Debug, Clone, Deserialize)]
pub struct VerificationTrace {
    pub name: String,
    /// 字段名 → 值。值可能不是整数(serde_json::Number),用 u64 解析。
    pub fields: serde_json::Value,
    /// 数据自评:这条 trace 能否区分正确布局与已知错误布局。
    /// true/false 由 check-bitfields 与实际打包结果交叉验证。
    /// 字符串形如 "partial" 也允许,按"非 true"处理。
    #[serde(default)]
    pub distinguishes_layout: serde_json::Value,
    #[serde(default)]
    pub self_note: String,
}

/// 一个已知错误布局。
#[derive(Debug, Clone, Deserialize)]
pub struct WrongLayout {
    pub desc: String,
    pub fields: Vec<BitField>,
}

/// 位段定义。
#[derive(Debug, Clone, Deserialize)]
pub struct BitfieldDef {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub spec_ref: String,
    pub width: u32,
    pub fields: Vec<BitField>,
    #[serde(default)]
    pub known_wrong_layouts: Vec<WrongLayout>,
    #[serde(default)]
    pub verification_traces: Vec<VerificationTrace>,
}

/// bitfield-defs.json 顶层。
#[derive(Debug, Clone, Deserialize)]
pub struct BitfieldDefs {
    pub defs: Vec<BitfieldDef>,
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

impl BitField {
    pub fn width(&self) -> u32 {
        self.msb.saturating_sub(self.lsb) + 1
    }
    pub fn field_mask(&self) -> u64 {
        if self.width() >= 64 {
            u64::MAX
        } else {
            (1u64 << self.width()) - 1
        }
    }
}
