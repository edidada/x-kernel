// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Port 轨迹(Ch8 在线循环)。append-only,不可变。
//!
//! 每个 port 任务一个 `data/trace/<port>.jsonl`;`trace add` 追加事件,
//! 不提供修改/删除接口(不可变轨迹是 Ch8 进化证据的本意)。
//! 写错的当场观察就当历史保留;真错靠 promote 时的门槛挡住,不靠改写历史。

use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::repo;

/// 一条轨迹事件的种类。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TraceKind {
    /// `ldp lookup` 未命中(Linux API 在 api-mapping 里没有)。
    LookupMiss,
    /// 撞到缺失子系统的用法(已知 gap 的新用法,或疑似新 gap)。
    Gap,
    /// 位段/协议常量易错点(疑似新 known_wrong_layout)。
    BitfieldPitfall,
}

impl TraceKind {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "lookup-miss" => Ok(Self::LookupMiss),
            "gap" => Ok(Self::Gap),
            "bitfield-pitfall" => Ok(Self::BitfieldPitfall),
            other => Err(format!(
                "未知 kind `{other}`,应为 lookup-miss / gap / bitfield-pitfall"
            )),
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::LookupMiss => "lookup-miss",
            Self::Gap => "gap",
            Self::BitfieldPitfall => "bitfield-pitfall",
        }
    }
}

/// 一条不可变轨迹事件(写入 .jsonl,一行一个 JSON)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceEvent {
    /// epoch 秒(单调,用于排序;不做人类可读格式化以免引依赖)。
    pub ts: u64,
    pub kind: TraceKind,
    /// Linux 符号(如 clk_get_rate / SCMI message_id 位段)。
    pub linux: String,
    #[serde(default)]
    pub note: String,
    /// 证据引用(Linux 源码位置 / 规范条款),Ch8 安全边界 1 的"证据"侧。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

pub fn trace_dir() -> PathBuf {
    repo::data_dir().join("trace")
}

pub fn trace_path(port: &str) -> Result<PathBuf, String> {
    sanitize_port(port)?;
    Ok(trace_dir().join(format!("{port}.jsonl")))
}

/// port 名只允许 [a-zA-Z0-9_-](文件名直接拼,防路径遍历)。
fn sanitize_port(port: &str) -> Result<(), String> {
    if port.is_empty() {
        return Err("port 名不能为空".into());
    }
    if !port
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "port 名 `{port}` 含非法字符(只允许字母数字/连字符/下划线)"
        ));
    }
    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 追加一条事件(append-only)。
pub fn add(
    port: &str,
    kind: TraceKind,
    linux: &str,
    note: &str,
    evidence: Option<&str>,
) -> Result<(), String> {
    let path = trace_path(port)?;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }
    let event = TraceEvent {
        ts: now_secs(),
        kind,
        linux: linux.to_string(),
        note: note.to_string(),
        evidence: evidence.map(|s| s.to_string()),
    };
    let line = serde_json::to_string(&event).map_err(|e| e.to_string())?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(format!("{line}\n").as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(())
}

/// 解析一个轨迹文件的内容:逐行反序列化,**坏行跳过并记 warning**(不连坐整文件)。
/// 轨迹是不可变证据(Ch8 在线循环),一行损坏不该让整个 port 的证据被
/// aggregate/staging 静默丢弃——那是错误的容错方向。
fn parse_events(raw: &str) -> (Vec<TraceEvent>, Vec<String>) {
    let mut events = Vec::new();
    let mut warnings = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<TraceEvent>(line) {
            Ok(ev) => events.push(ev),
            Err(e) => warnings.push(format!(":{} 解析失败(已跳过该行): {e}", i + 1)),
        }
    }
    (events, warnings)
}

/// 读一个 port 轨迹的全部事件。坏行跳过 + warn(不 abort 整文件);文件不存在才 Err。
pub fn read(port: &str) -> Result<Vec<TraceEvent>, String> {
    let path = trace_path(port)?;
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let (events, warnings) = parse_events(&raw);
    for w in &warnings {
        eprintln!("WARN [trace] {}{w}", path.display());
    }
    Ok(events)
}

/// 列所有轨迹文件(port 名,排序)。
pub fn list() -> Vec<String> {
    let dir = trace_dir();
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("jsonl")
                && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
            {
                names.push(stem.to_string());
            }
        }
    }
    names.sort();
    names
}

/// 读所有轨迹(用于离线聚合)。
pub fn read_all() -> Vec<(String, Vec<TraceEvent>)> {
    list()
        .into_iter()
        .filter_map(|n| read(&n).ok().map(|evs| (n, evs)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_roundtrip() {
        for k in &["lookup-miss", "gap", "bitfield-pitfall"] {
            let parsed = TraceKind::parse(k).unwrap();
            assert_eq!(parsed.as_str(), *k);
        }
        assert!(TraceKind::parse("bogus").is_err());
    }

    #[test]
    fn port_name_rejects_traversal() {
        assert!(sanitize_port("dw_wdt-rk3588").is_ok());
        assert!(sanitize_port("").is_err());
        assert!(sanitize_port("../etc").is_err());
        assert!(sanitize_port("a/b").is_err());
        assert!(sanitize_port("a b").is_err());
    }

    #[test]
    fn parse_events_skips_bad_lines_not_whole_file() {
        // 一份混入坏行的轨迹:坏行应被跳过 + 记 warning,合法行照样返回。
        // 旧行为是第一行坏数据就让整个文件 Err → aggregate 静默丢整个 port 的证据。
        let raw = "{\"ts\":1,\"kind\":\"gap\",\"linux\":\"clk_get_rate\",\"note\":\"x\"}\n\
                   not json at all\n\
                   {\"ts\":2,\"kind\":\"lookup-miss\",\"linux\":\"foo\"}\n\
                   {\"bad\": \"missing required fields\"}";
        let (events, warnings) = parse_events(raw);
        assert_eq!(events.len(), 2, "2 条合法事件必须保留");
        assert_eq!(events[0].linux, "clk_get_rate");
        assert_eq!(events[1].linux, "foo");
        assert_eq!(warnings.len(), 2, "2 行坏数据各一条 warning");
    }
}
