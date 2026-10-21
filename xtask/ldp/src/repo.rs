// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 仓库根定位与数据文件加载。
//!
//! `ldp` 既可以在仓库内被 `make`/`cargo run` 调用,也可以被任意 AI agent
//! 直接执行。repo 根通过编译期 `CARGO_MANIFEST_DIR` 解析(`xtask/ldp` 上溯两级),
//! 数据文件固定在 `<repo>/docs/ai/skills/linux-driver-port/data/`。

use std::path::{Path, PathBuf};

use crate::data::{ApiMapping, BitfieldDefs, Gaps};

/// 定位仓库根目录。
pub fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = .../xtask/ldp
    let manifest = env!("CARGO_MANIFEST_DIR");
    let p = Path::new(manifest);
    p.ancestors()
        .nth(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// skill 目录(`docs/ai/skills/linux-driver-port`)。
pub fn skill_dir() -> PathBuf {
    repo_root().join("docs/ai/skills/linux-driver-port")
}

/// 数据目录。
pub fn data_dir() -> PathBuf {
    skill_dir().join("data")
}

pub fn api_mapping_path() -> PathBuf {
    data_dir().join("api-mapping.json")
}
pub fn gaps_path() -> PathBuf {
    data_dir().join("gaps.json")
}
pub fn bitfield_path() -> PathBuf {
    data_dir().join("bitfield-defs.json")
}
pub fn golden_dir() -> PathBuf {
    data_dir().join("golden")
}
pub fn references_path() -> PathBuf {
    data_dir().join("references.json")
}

/// 读取并解析一个 JSON 文件,失败时给出带路径的可读错误。
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    serde_json::from_str(&raw).map_err(|e| format!("{}: {}", path.display(), e))
}

pub fn load_api_mapping() -> Result<ApiMapping, String> {
    read_json(&api_mapping_path())
}
pub fn load_gaps() -> Result<Gaps, String> {
    read_json(&gaps_path())
}
pub fn load_bitfields() -> Result<BitfieldDefs, String> {
    read_json(&bitfield_path())
}
pub fn load_references() -> Result<crate::data::References, String> {
    read_json(&references_path())
}
pub fn provider_boundary_path() -> PathBuf {
    data_dir().join("provider-boundary.json")
}
pub fn load_provider_boundary() -> Result<crate::data::ProviderBoundary, String> {
    read_json(&provider_boundary_path())
}

/// 仓库根下某个相对路径是否存在(目录或文件)。
pub fn repo_path_exists(rel: &str) -> bool {
    !rel.is_empty() && repo_root().join(rel).exists()
}
