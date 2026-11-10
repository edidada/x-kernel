// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 序列门禁：把参考实现的**有序副作用序列**变成可检查的产物。
//!
//! 位段/常量/API 映射都是有形状的数据，错了一眼能看出来；初始化序列没有形状，
//! 所以"参考实现调了 N 步、我们调了 N-k 步"从来不会失败，只会表现为设备行为
//! 不对。这一层把它变成可机械检查的三件事：
//!
//! - **覆盖**：每个 step 要么在 v2 代码里有 `// ldp:step <id>` 锚点，要么显式
//!   `waived`（必须写理由）。漏掉的步骤直接出现在报告里。
//! - **可达**：锚点所在的函数必须能从 `entries` 调用到。防止"写了但没人调"。
//! - **顺序**：同一 path 内锚点的行号必须单调递增。防止"步骤都在、顺序错"。
//!
//! 数据在 `data/sequences/<port>.toml`，由 `ldp sequence extract` 从参考 C 源
//! 生成草稿，人工只做合并与状态填写。

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::repo;

/// 一个 port 的序列描述文件。
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SequenceDoc {
    pub port: PortMeta,
    #[serde(default)]
    pub path: Vec<PathSpec>,
}

/// port 元信息：权威源码、provenance、可达性起点。
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PortMeta {
    /// port 名（与 `data/sequences/<port>.toml` 同名）。
    pub name: String,
    /// v2 侧要扫描锚点的 crate 目录（相对仓库根）。
    #[serde(default)]
    pub crate_path: String,
    /// 额外的 crate 目录：一步可能落在别的 crate（例如 PCIe 配置调优属于
    /// host 侧的 bus crate），锚点因此要跨 crate 扫描。
    #[serde(default)]
    pub crate_paths: Vec<String>,
    /// 权威参考源码：`<tree>:<file>`。必须是**板子上真正在跑的那个驱动**。
    pub authoritative: String,
    /// 怎么确认它是权威源的（可复现命令 + 预期输出片段）。空 = 门禁失败。
    #[serde(default)]
    pub provenance: String,
    /// 可达性起点函数名。
    #[serde(default)]
    pub entries: Vec<String>,
}

/// 一条命名路径（init / start / stop / link / irq）。
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PathSpec {
    pub name: String,
    #[serde(default)]
    pub step: Vec<Step>,
}

/// 序列里的一个步骤。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    /// 全局唯一 id，锚点写 `// ldp:step <id>`。
    pub id: String,
    /// 参考源码位置 `file:line`。
    pub src: String,
    /// `write` / `rmw` / `read` / `poll` / `wait` / `call`
    pub op: String,
    /// 目标（寄存器名/OCP 地址/函数名）。
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// 生效条件（芯片版本/分支），例如 `ver_63`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    /// v2 侧的落地状态。
    pub v2: V2State,
    /// `waived` 的理由；`unported` 可以写"为什么还没做"。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 步骤在 v2 侧的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum V2State {
    /// 有锚点，且锚点可达。
    Anchored,
    /// 明确不做（必须写 note 说明理由与退出条件）。
    Waived,
    /// 还没移植（报告里列出，`--strict` 时算失败）。
    Unported,
}

impl V2State {
    fn is_anchored(self) -> bool {
        matches!(self, Self::Anchored)
    }
}

fn sequences_dir() -> PathBuf {
    repo::data_dir().join("sequences")
}

/// 列出所有序列文件。
fn sequence_files() -> Result<Vec<PathBuf>, String> {
    let dir = sequences_dir();
    let mut out = Vec::new();
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| format!("读取 {} 失败: {e}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|e| format!("读取目录项失败: {e}"))?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("toml") {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn load_sequence(path: &Path) -> Result<SequenceDoc, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("解析 {} 失败: {e}", path.display()))
}

/// `ldp sequence check`：覆盖 / 可达 / 顺序。
pub fn check(only_port: Option<&str>, strict: bool, list_unported: bool) -> Result<bool, String> {
    let files = sequence_files()?;
    if files.is_empty() {
        return Err(format!(
            "{} 下没有序列文件：先用 `ldp sequence extract` 生成草稿",
            sequences_dir().display()
        ));
    }

    let mut failed = false;
    for file in &files {
        let doc = load_sequence(file)?;
        if let Some(name) = only_port {
            if doc.port.name != name {
                continue;
            }
        }
        let ok = check_one(&doc, strict)?;
        if !ok {
            failed = true;
        }
    }

    if list_unported {
        print_unported(&files, only_port)?;
    }
    // 默认只是警告：三个 port 全部补齐之前不阻塞 CI。
    Ok(!(strict && failed))
}

fn check_one(doc: &SequenceDoc, strict: bool) -> Result<bool, String> {
    let mut ok = true;
    println!("== sequence: {}", doc.port.name);

    if doc.port.authoritative.trim().is_empty() {
        println!("  ✗ authoritative 未填写（必须是板子上真正在跑的那个驱动）");
        ok = false;
    }
    if doc.port.provenance.trim().is_empty() {
        println!("  ✗ provenance 未填写（要给出可复现的确认命令 + 预期输出）");
        ok = false;
    }

    let mut sources = Vec::new();
    let mut dirs: Vec<&str> = Vec::new();
    if !doc.port.crate_path.is_empty() {
        dirs.push(&doc.port.crate_path);
    }
    dirs.extend(doc.port.crate_paths.iter().map(String::as_str));
    for dir in dirs {
        sources.extend(collect_sources(&repo::repo_root().join(dir))?);
    }
    let anchors = collect_anchors(&sources);
    let reachable = reachable_functions(&sources, &doc.port.entries);

    // 覆盖 + 可达
    for path in &doc.path {
        for step in &path.step {
            match step.v2 {
                V2State::Anchored => match anchors.get(&step.id) {
                    Some(site) => {
                        if !reachable.contains(&site.function) {
                            println!(
                                "  ✗ {} 锚点在 `{}` 里，从 entries 调不到（可达性）",
                                step.id, site.function
                            );
                            ok = false;
                        }
                    }
                    None => {
                        println!(
                            "  ✗ {} 声明 anchored 但代码里没有 `// ldp:step {}`",
                            step.id, step.id
                        );
                        ok = false;
                    }
                },
                V2State::Waived => {
                    if step.note.as_deref().unwrap_or("").trim().is_empty() {
                        println!("  ✗ {} waived 但没写理由（note）", step.id);
                        ok = false;
                    }
                }
                V2State::Unported => {
                    if strict {
                        println!("  ✗ {} 还没移植（--strict 下算失败）", step.id);
                        ok = false;
                    }
                }
            }
        }
    }

    // 顺序：同一 path 内已锚定的步骤，锚点行号必须单调递增（按文件分组比较）。
    for path in &doc.path {
        let mut by_file: BTreeMap<&str, Vec<(usize, &Step)>> = BTreeMap::new();
        for step in &path.step {
            if !step.v2.is_anchored() {
                continue;
            }
            // 顺序只在**同一个函数内**比较：源码行号是文件布局，不是执行顺序，
            // 跨函数比较会产生假报警（例如 init 段和 start 段本来就分属两个函数）。
            if let Some(site) = anchors.get(&step.id) {
                let key = format!("{}::{}", site.file, site.function);
                by_file
                    .entry(Box::leak(key.into_boxed_str()))
                    .or_default()
                    .push((site.line, step));
            }
        }
        for (file, mut steps) in by_file {
            steps.sort_by_key(|(line, _)| *line);
            let mut prev: Option<&Step> = None;
            for (_, step) in &steps {
                prev = Some(step);
            }
            // 参考顺序 -> 锚点顺序必须保序：用 path.step 的下标比较。
            let order: BTreeMap<&str, usize> = path
                .step
                .iter()
                .enumerate()
                .map(|(i, s)| (s.id.as_str(), i))
                .collect();
            let anchored_order: Vec<usize> = steps
                .iter()
                .map(|(l, s)| (l, order[s.id.as_str()]))
                .map(|(_, o)| o)
                .collect();
            if anchored_order.windows(2).any(|w| w[0] > w[1]) {
                println!("  ✗ path `{}` 在 {file} 内顺序与参考不一致", path.name);
                ok = false;
            }
            let _ = prev;
        }
    }

    let total = doc.path.iter().map(|p| p.step.len()).sum::<usize>();
    let anchored = doc
        .path
        .iter()
        .flat_map(|p| p.step.iter())
        .filter(|s| s.v2.is_anchored())
        .count();
    let waived = doc
        .path
        .iter()
        .flat_map(|p| p.step.iter())
        .filter(|s| matches!(s.v2, V2State::Waived))
        .count();
    println!(
        "  步骤 {total}：anchored {anchored} / waived {waived} / unported {}",
        total - anchored - waived
    );
    Ok(ok)
}

fn print_unported(files: &[PathBuf], only_port: Option<&str>) -> Result<(), String> {
    for file in files {
        let doc = load_sequence(file)?;
        if let Some(name) = only_port {
            if doc.port.name != name {
                continue;
            }
        }
        let missing: Vec<&Step> = doc
            .path
            .iter()
            .flat_map(|p| p.step.iter())
            .filter(|s| matches!(s.v2, V2State::Unported))
            .collect();
        if missing.is_empty() {
            continue;
        }
        println!("== 未移植步骤：{}（{} 条）", doc.port.name, missing.len());
        for step in missing {
            println!(
                "  - {:<28} {:<6} {:<22} {}",
                step.id, step.op, step.target, step.src
            );
        }
    }
    Ok(())
}

// ---- 锚点 ----------------------------------------------------------------

#[derive(Debug, Clone)]
struct AnchorSite {
    file: String,
    line: usize,
    function: String,
}

/// 扫描 v2 源码里的 `// ldp:step <id>[,<id>...]` 锚点。
fn collect_anchors(sources: &[PathBuf]) -> BTreeMap<String, AnchorSite> {
    let mut out = BTreeMap::new();
    for path in sources {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let rel = relative(path);
        let mut current_fn = String::from("<top>");
        for (idx, line) in text.lines().enumerate() {
            if let Some(name) = fn_name_on_line(line) {
                current_fn = name;
            }
            let Some(rest) = line.split("ldp:step").nth(1) else {
                let rest2 = line.split("ldp:steps").nth(1);
                let Some(rest2) = rest2 else { continue };
                register_ids(
                    rest2,
                    &mut out,
                    &AnchorSite {
                        file: rel.clone(),
                        line: idx + 1,
                        function: current_fn.clone(),
                    },
                );
                continue;
            };
            register_ids(
                rest,
                &mut out,
                &AnchorSite {
                    file: rel.clone(),
                    line: idx + 1,
                    function: current_fn.clone(),
                },
            );
        }
    }
    out
}

fn register_ids(rest: &str, out: &mut BTreeMap<String, AnchorSite>, site: &AnchorSite) {
    // 锚点形如 `// ldp:step a.b, a.c` 或 `// ldp:steps a.b, a.c`。
    let ids = rest
        .trim_start_matches([':', 's', 't', 'e', 'p'])
        .trim();
    for id in ids.split([',', ' ', '\t']) {
        let id = id.trim().trim_end_matches(['.', ';']);
        if id.is_empty() || !id.contains('.') {
            continue;
        }
        out.entry(id.to_string()).or_insert_with(|| site.clone());
    }
}

/// 仓库相对路径（找不到前缀时回退为原路径）。
fn relative(path: &Path) -> String {
    let root = repo::repo_root();
    path.strip_prefix(&root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn fn_name_on_line(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") || trimmed.starts_with("///") {
        return None;
    }
    let idx = line.find("fn ")?;
    let after = &line[idx + 3..];
    let name: String = after
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() { None } else { Some(name) }
}

// ---- 可达性 --------------------------------------------------------------

fn collect_sources(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }
    Ok(out)
}

/// 从 `entries` 出发做一次粗粒度调用闭包分析（按函数名匹配，够用）。
fn reachable_functions(sources: &[PathBuf], entries: &[String]) -> BTreeSet<String> {
    let mut calls: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for path in sources {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let mut current_fn: Option<String> = None;
        for line in text.lines() {
            if let Some(name) = fn_name_on_line(line) {
                calls.entry(name.clone()).or_default();
                current_fn = Some(name);
            }
            let Some(fname) = current_fn.clone() else {
                continue;
            };
            // `ident(` 形式的所有调用/引用。
            let mut rest = line;
            while let Some(pos) = rest.find('(') {
                let head = &rest[..pos];
                let name: String = head
                    .chars()
                    .rev()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                if !name.is_empty() && name != fname {
                    calls.entry(fname.clone()).or_default().insert(name);
                }
                rest = &rest[pos + 1..];
            }
        }
    }

    let mut seen = BTreeSet::new();
    let mut queue: VecDeque<String> = entries.iter().cloned().collect();
    while let Some(fname) = queue.pop_front() {
        if !seen.insert(fname.clone()) {
            continue;
        }
        if let Some(callees) = calls.get(&fname) {
            for callee in callees {
                if !seen.contains(callee) {
                    queue.push_back(callee.clone());
                }
            }
        }
    }
    seen
}

// ---- 抽取 ----------------------------------------------------------------

/// `ldp sequence extract`：从参考 C 函数抽取有序步骤，打印 TOML 草稿。
pub fn extract(file: &Path, function: &str, path_name: &str) -> Result<(), String> {
    let text =
        std::fs::read_to_string(file).map_err(|e| format!("读取 {} 失败: {e}", file.display()))?;
    let lines: Vec<&str> = text.lines().collect();
    // Function signatures are frequently split across lines in kernel C
    // (`void` on one line, `name(args)` on the next), so accept both forms:
    // the name opening a line, or the name trailing a type on the same line.
    let call = format!("{function}(");
    let start = lines
        .iter()
        .enumerate()
        .position(|(i, l)| {
            let t = l.trim_start();
            let same_line = (t.contains("void ") || t.contains("int ") || t.contains("static"))
                && t.contains(&call);
            let name_line = t.starts_with(&call);
            let typed_above = if name_line && i > 0 {
                let above = lines[i - 1].trim();
                above.ends_with("void")
                    || above.ends_with("int")
                    || above.ends_with("bool")
                    || above.ends_with("static")
                    || above.starts_with("static ")
            } else {
                false
            };
            same_line || (name_line && typed_above)
        })
        .ok_or_else(|| format!("在 {} 里找不到函数 {function}", file.display()))?;

    let file_name = file
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("source.c");
    let mut rows: Vec<Step> = Vec::new();
    let mut depth = 0i32;
    let mut started = false;
    let mut seq = 0usize;

    for (offset, line) in lines.iter().enumerate().skip(start) {
        depth += line.matches('{').count() as i32;
        depth -= line.matches('}').count() as i32;
        if !started {
            if line.contains('{') {
                started = true;
            }
            continue;
        }
        if depth <= 0 {
            break;
        }
        let Some((op, target, mask, value)) = classify(line) else {
            continue;
        };
        seq += 1;
        let slug: String = target
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect();
        rows.push(Step {
            id: format!("{path_name}.{seq:02}-{}", slug.trim_matches('-')),
            src: format!("{file_name}:{}", offset + 1),
            op: op.to_string(),
            target,
            mask,
            value,
            when: None,
            v2: V2State::Unported,
            note: None,
        });
    }

    if rows.is_empty() {
        return Err(format!("函数 {function} 里没有抽取到任何步骤"));
    }

    let doc = SequenceDoc {
        port: PortMeta {
            name: function.to_string(),
            crate_path: String::from("TODO/填入 v2 crate 路径"),
            crate_paths: Vec::new(),
            authoritative: format!("TODO:<tree>:{file_name}"),
            provenance: String::new(),
            entries: vec![String::from("TODO")],
        },
        path: vec![PathSpec {
            name: path_name.to_string(),
            step: rows,
        }],
    };
    let rendered = toml::to_string_pretty(&doc).map_err(|e| format!("序列化失败: {e}"))?;
    println!("{rendered}");
    Ok(())
}

/// 从一行 C 代码判断它是不是一个寄存器操作/等待/调用。
fn classify(line: &str) -> Option<(&'static str, String, Option<String>, Option<String>)> {
    let t = line.trim();
    if t.starts_with("//") || t.starts_with("/*") || t.starts_with('*') {
        return None;
    }
    if let Some(args) = call_args(t, "RTL_W32") {
        let (reg, val) = split2(args)?;
        return Some(("write", format!("{reg}@32"), None, Some(val)));
    }
    if let Some(args) = call_args(t, "RTL_W16") {
        let (reg, val) = split2(args)?;
        return Some(("write", format!("{reg}@16"), None, Some(val)));
    }
    if let Some(args) = call_args(t, "RTL_W8") {
        let (reg, val) = split2(args)?;
        return Some(("write", format!("{reg}@8"), None, Some(val)));
    }
    if let Some(args) = call_args(t, "r8168_mac_ocp_modify") {
        let parts = split_top(args);
        if parts.len() == 4 {
            return Some((
                "rmw",
                format!("OCP({})", parts[1].trim()),
                Some(parts[2].trim().to_string()),
                Some(parts[3].trim().to_string()),
            ));
        }
    }
    if let Some(args) = call_args(t, "r8168_mac_ocp_write") {
        let (reg, val) = split2(args)?;
        return Some(("write", format!("OCP({})", reg), None, Some(val)));
    }
    // 通用 MMIO 助记（writel/readl）：大多数 Linux 驱动的寄存器访问原语。
    // writel(VALUE, base + REG) → write REG；readl(base + REG) → read REG。
    // 基址表达式（`ldata->base +` 等）不是硬件事实，剥掉只留寄存器名。
    if let Some(args) = call_args(t, "writel") {
        let parts = split_top(args);
        if parts.len() == 2 {
            let reg = strip_base_expr(&parts[1]);
            return Some(("write", reg, None, Some(parts[0].trim().to_string())));
        }
    }
    if let Some(args) = call_args(t, "readl") {
        let parts = split_top(args);
        if parts.len() == 1 {
            return Some(("read", strip_base_expr(&parts[0]), None, None));
        }
    }
    for waiter in ["rtl_loop_wait_low", "rtl_loop_wait_high"] {
        if let Some(args) = call_args(t, waiter) {
            let parts = split_top(args);
            let cond = parts.get(1).cloned().unwrap_or_default();
            return Some(("poll", cond, None, None));
        }
    }
    for wait in ["msleep", "udelay", "rtl_msleep_loop_wait"] {
        if let Some(args) = call_args(t, wait) {
            let arg = split_top(args).first().cloned().unwrap_or_default();
            return Some(("wait", format!("{wait}({arg})"), None, None));
        }
    }
    // 参考驱动自己的辅助函数调用（rtl*/r8168*），是"这一步做了什么"的粗粒度记录。
    let mut rest = t;
    while let Some(pos) = rest.find('(') {
        let head = &rest[..pos];
        let name: String = head
            .chars()
            .rev()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        if (name.starts_with("rtl") || name.starts_with("r8168")) && name != "RTL_W32" {
            return Some(("call", name, None, None));
        }
        rest = &rest[pos + 1..];
    }
    None
}

/// 取出 `name(...)` 的参数串（同一行内）。
/// `ldata->base + RTC_CR` / `base + REG` / `REG` → `REG`。
///
/// 参考驱动把寄存器名拼在基址表达式里（铁律 1 关心的是寄存器名与顺序，
/// 基址写法是 C 侧的局部变量，不属于硬件事实）。
fn strip_base_expr(expr: &str) -> String {
    expr.trim()
        .rsplit('+')
        .next()
        .unwrap_or(expr)
        .trim()
        .to_string()
}

fn call_args<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let idx = line.find(name)?;
    let after = &line[idx + name.len()..];
    let open = after.find('(')?;
    let mut depth = 0usize;
    for (i, ch) in after[open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&after[open + 1..open + i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// 按顶层逗号切分（忽略括号/方括号内的逗号）。
fn split_top(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in args.chars() {
        match ch {
            '(' | '[' => {
                depth += 1;
                cur.push(ch);
            }
            ')' | ']' => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn split2(args: &str) -> Option<(String, String)> {
    // RTL_W32(tp, REG, value) — 第一项是 tp。
    let parts = split_top(args);
    if parts.len() < 3 {
        return None;
    }
    Some((parts[1].clone(), parts[2].clone()))
}