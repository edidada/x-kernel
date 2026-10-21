// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 参考树注册表:`ldp ref list` / `ldp ref check`。
//!
//! 把"以 Linux/U-Boot 源码为参考"从散文升级为机器可验:每个参考必须是本地目录
//! (submodule 或 vendored),带 git pin。拒绝 web search —— 缺参考即 fail-closed,
//! 这是"太容易忘了"的结构性解法(必须在场才能过 check-layout)。
//!
//! 不校验 pin 是否与远程一致(那要联网,且 vendored 子集无 .git);只校验:
//! - `path` 在仓库内真实存在;
//! - `path` 下有 `.git`(submodule)或 `VENDORED.txt`(vendored 标记)。

use std::path::Path;

use crate::{data::Reference, repo};

/// 参考树在仓库里的物理位置(`<repo>/<reference.path>`)。
pub fn tree_path(r: &Reference) -> std::path::PathBuf {
    repo::repo_root().join(&r.path)
}

/// 一棵参考树的本地状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeState {
    /// 本地存在且有 pin 标记(submodule 的 .git 或 vendored 的 VENDORED.txt)。
    Present { kind: String, pin_recorded: String },
    /// 路径不存在(最常见:还没 vendor)。
    Missing,
    /// 路径存在但既无 .git 也无 VENDORED.txt(不规范)。
    Unmarked,
}

pub fn inspect(r: &Reference) -> TreeState {
    let p = tree_path(r);
    if !p.exists() {
        return TreeState::Missing;
    }
    let pin = read_pin_marker(&p);
    match pin {
        Some(recorded) => TreeState::Present {
            kind: r.kind.clone(),
            pin_recorded: recorded,
        },
        None => TreeState::Unmarked,
    }
}

/// 读 vendored 标记文件 `VENDORED.txt` 的首行(pin)。submodule 无此文件,
/// 由 `.gitmodules` 钉 pin,这里读 `.git`(目录或 gitlink 文件)的存在性作信号。
fn read_pin_marker(dir: &Path) -> Option<String> {
    let marker = dir.join("VENDORED.txt");
    if let Ok(s) = std::fs::read_to_string(&marker) {
        return s.lines().next().map(|l| l.trim().to_string());
    }
    let gitlink = dir.join(".git");
    if gitlink.exists() {
        return Some("(submodule; pin 由 .gitmodules 钉)".to_string());
    }
    None
}

/// `ldp ref list`:列出已注册参考树 + 本地状态。
pub fn list() -> usize {
    let refs = repo::load_references().unwrap_or_default();
    if refs.trees.is_empty() {
        println!("(未注册任何参考树。编辑 data/references.json 添加,如 linux/u-boot 子树。)");
        return 0;
    }
    println!("已注册参考树({} 棵):", refs.trees.len());
    println!("────────────────────────────────────────");
    for r in &refs.trees {
        let st = inspect(r);
        let tag = match &st {
            TreeState::Present { kind, pin_recorded } => {
                format!("✓ present [{kind}] pin={pin_recorded}")
            }
            TreeState::Missing => "✗ MISSING(需 vendor,见 references/README.md)".to_string(),
            TreeState::Unmarked => "⚠ unmarked(缺 VENDORED.txt / .git)".to_string(),
        };
        let req = if r.required { "required" } else { "optional" };
        println!("● {} [{req}] {tag}", r.id);
        println!("  path: {}", r.path);
        if !r.pin.is_empty() && matches!(st, TreeState::Present { .. }) {
            println!("  declared pin: {}", r.pin);
        }
    }
    refs.trees
        .iter()
        .filter(|r| r.required && matches!(inspect(r), TreeState::Missing))
        .count()
}

/// `ldp ref check`:fail-closed 校验所有 required 参考本地就位。返回错误数。
pub fn check() -> usize {
    let refs = repo::load_references().unwrap_or_default();
    if refs.trees.is_empty() {
        return 0;
    }
    let mut errors = 0usize;
    let mut warnings = 0usize;
    for r in &refs.trees {
        match inspect(r) {
            TreeState::Present { .. } => {
                println!("✓ 参考树 `{}` 就位({})", r.id, r.path);
            }
            TreeState::Missing => {
                if r.required {
                    errors += 1;
                    eprintln!(
                        "ERROR [ref] 必需参考树 `{}` 缺失:{}。vendor 方法见 \
                         data/references/README.md。",
                        r.id, r.path
                    );
                } else {
                    println!("ℹ 可选参考树 `{}` 缺失:{}(不阻塞)", r.id, r.path);
                }
            }
            TreeState::Unmarked => {
                warnings += 1;
                eprintln!(
                    "WARN  [ref] `{}` 存在但无 VENDORED.txt/.git(无法确认 pin):{}",
                    r.id, r.path
                );
            }
        }
    }
    println!("────────────────────────────────────────");
    println!("ref check: {errors} 缺失(required), {warnings} 无 pin 标记");
    errors
}
