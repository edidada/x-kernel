// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 布局提取器:从参考树(C header)和驱动代码(Rust const/fn)机械导出位段真值。
//!
//! 这是把 `check-bitfields` 的**自指**范式(检查 JSON 跟自己的关系)重构为
//! **参考锚定**范式的核心:布局真值从钉死的参考机械导出,不由人手写。
//!
//! 两套提取器,各认受限但明确的词汇表(不需要通用 C/Rust 解析):
//! - [`extract_c_symbol`]:Linux C header 的 `#define NAME GENMASK(h,l)` /
//!   `BIT(n)` / 裸 `#define NAME 0xNNN`。覆盖 SCMI 协议头、绝大多数寄存器位/偏移。
//! - [`extract_rust_anchor`]:x-kernel 驱动里高度统一的位惯法 ——
//!   `const NAME: T = 1 << N;`(单位位)、`const NAME: T = 0xNNN;`(标量偏移)、
//!   `const fn name(p..) { (p & MASK) << SHIFT | ... }`(位段打包函数,抽每个参数的位段)。

use crate::data::{FieldSpan, LayoutRef};

/// 提取出来的位段真值,附来源行号(给可读 diff 提示用)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedSpan {
    pub name: String,
    pub span: FieldSpan,
    pub line: u32,
}

// ───────────────────────── A 档:C header 提取 ─────────────────────────

/// C 提取结果:位段(字段位)或标量(寄存器偏移)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractedC {
    Span(ExtractedSpan),
    Scalar { name: String, value: u64, line: u32 },
}

/// 从参考树的一个 C header 里提取 `symbol` 的位段真值。
///
/// 认 Linux 受限词汇:`#define NAME GENMASK(h,l)` / `GENMASK_ULL(h,l)` /
/// `BIT(n)` / `(1 << n)` / 裸 `0xNN`。不展开宏、不解析 `#include`(只匹配调用形式)。
/// 这是个受限词法器,不是 C 编译器 —— 但覆盖 Linux 现代 bitfield 绝大多数写法。
pub fn extract_c_symbol(src: &str, symbol: &str) -> Result<ExtractedC, String> {
    for (i, raw) in src.lines().enumerate() {
        let line = strip_c_comment(raw).trim();
        let Some(body) = match_define(line, symbol) else {
            continue;
        };
        let body = if raw.trim_end().ends_with('\\') {
            join_continuation(body, src, i)
        } else {
            body.to_string()
        };
        if let Some(span) = parse_genmask(&body) {
            return Ok(ExtractedC::Span(ExtractedSpan {
                name: symbol.to_string(),
                span,
                line: i as u32 + 1,
            }));
        }
        if let Some(span) = parse_bit_n(&body) {
            return Ok(ExtractedC::Span(ExtractedSpan {
                name: symbol.to_string(),
                span,
                line: i as u32 + 1,
            }));
        }
        if let Some(val) = parse_scalar_hex(&body) {
            return Ok(ExtractedC::Scalar {
                name: symbol.to_string(),
                value: val,
                line: i as u32 + 1,
            });
        }
    }
    Err(format!(
        "参考里找不到符号 `{symbol}` 的 GENMASK/BIT/标量定义(它可能用别的写法,或不在该文件)"
    ))
}

/// `#define <symbol>` 开头? 返回 symbol 之后的 body。
fn match_define<'a>(line: &'a str, symbol: &str) -> Option<&'a str> {
    let t = line.strip_prefix('#').unwrap_or(line).trim_start();
    let t = t.strip_prefix("define").unwrap_or(t).trim_start();
    let sym_end = t
        .find(|c: char| c.is_whitespace() || c == '(')
        .unwrap_or(t.len());
    if sym_end != symbol.len() {
        return None;
    }
    let (head, tail) = t.split_at(sym_end);
    if head != symbol {
        return None;
    }
    let body = tail.trim_start();
    // 函数宏 `NAME(args)`:NAME 与 `(` 之间无空白。判据是 t 里 head 之后紧跟 `(`,
    // 而非 `body.starts_with('(')`——后者会把 `NAME (1 << n)` 这类带括号的对象宏
    // 也误杀(body 已 trim 掉 head 与 `(` 之间的空白)。
    let is_function_macro = t.as_bytes().get(sym_end) == Some(&b'(');
    if is_function_macro && raw_head_symbol(t) == head {
        return None;
    }
    Some(body)
}

fn raw_head_symbol(s: &str) -> &str {
    let t = s.trim_start();
    let end = t
        .find(|c: char| c.is_whitespace() || c == '(')
        .unwrap_or(t.len());
    &t[..end]
}

/// `GENMASK(high, low)` / `GENMASK_ULL(high, low)` → [low, high]。
fn parse_genmask(body: &str) -> Option<FieldSpan> {
    let b = body.trim().trim_start_matches('(').trim_start();
    let inner = b
        .strip_prefix("GENMASK_ULL")
        .or_else(|| b.strip_prefix("GENMASK"))?;
    let inner = inner.trim_start().strip_prefix('(')?.strip_suffix(')')?;
    let parts: Vec<&str> = inner.split(',').map(|s| s.trim()).collect();
    if parts.len() != 2 {
        return None;
    }
    let high: u32 = parts[0].parse().ok()?;
    let low: u32 = parts[1].parse().ok()?;
    (high >= low).then_some(FieldSpan {
        lsb: low,
        msb: high,
    })
}

/// `BIT(n)` / `(1 << n)` → [n, n]。
fn parse_bit_n(body: &str) -> Option<FieldSpan> {
    let b = body.trim();
    if let Some(rest) = b.strip_prefix("BIT") {
        let inner = rest.trim_start().strip_prefix('(')?.strip_suffix(')')?;
        let n: u32 = inner.trim().parse().ok()?;
        return Some(FieldSpan { lsb: n, msb: n });
    }
    let inner = b.trim_start_matches('(').trim_end_matches(')').trim();
    if let Some(n) = parse_one_shift(inner) {
        return Some(FieldSpan { lsb: n, msb: n });
    }
    None
}

/// `1 << N` / `1ULL << N` → N。
fn parse_one_shift(s: &str) -> Option<u32> {
    let mut it = s.split("<<");
    let lhs = it.next()?.trim();
    let rhs = it.next()?.trim().trim_end_matches(')');
    it.next().is_none().then_some(())?;
    if lhs != "1" && lhs != "1ULL" && lhs != "1U" {
        return None;
    }
    rhs.parse().ok()
}

/// 裸标量 `0xNN` → 值(寄存器偏移用)。
fn parse_scalar_hex(body: &str) -> Option<u64> {
    let t = body.trim().trim_end_matches(';').trim();
    let t = t.trim_start_matches('(').trim_end_matches(')').trim();
    let hex = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(hex, 16).ok()
}

fn strip_c_comment(line: &str) -> &str {
    let s = line.split("//").next().unwrap_or(line);
    if s.contains("/*") {
        s.split("/*").next().unwrap_or(s)
    } else {
        s
    }
}

/// 把多行 `#define` 续行拼回一个 body 字符串。
/// `first_body` 是首行 symbol 之后的文本(可能带尾 `\`,由 match_define 返回);
/// 从 `start_line + 1` 起收集续行(逐行去尾 `\`)。
///
/// 注意:旧实现 `skip(start_line)` 把首行整行(含 `#define NAME`)也并进来,
/// 导致 parse_genmask 因前缀不是 `GENMASK` 而失败 —— 多行 define 静默提取不到。
fn join_continuation(first_body: &str, src: &str, start_line: usize) -> String {
    let mut out = first_body
        .trim_end()
        .trim_end_matches('\\')
        .trim_end()
        .to_string();
    for raw in src.lines().skip(start_line + 1) {
        let t = raw.trim();
        match t.strip_suffix('\\') {
            Some(stripped) => {
                out.push(' ');
                out.push_str(stripped.trim());
            }
            None => {
                out.push(' ');
                out.push_str(t);
                break;
            }
        }
    }
    out
}

// ───────────────────────── D2:Rust 驱动代码提取 ─────────────────────────

/// 从 x-kernel 驱动代码里提取 `anchor`(const fn 或 const)的位声明。
///
/// 认三种统一惯法:
/// - `const fn name(p1, p2, ..) { (p1 & M1) << S1 | (p2 & M2) << S2 | .. }`
///   位段打包函数:抽每个参数的位段(S 可省略,=0)。
/// - `const NAME: T = 1 << N;` → 单位字段 [N, N]。
/// - `const NAME: T = 0xNN;` → 标量(寄存器偏移)。
pub fn extract_rust_anchor(src: &str, anchor: &str) -> Result<RustExtracted, String> {
    if let Some(spans) = try_const_fn_packer(src, anchor)? {
        return Ok(RustExtracted {
            kind: RustKind::Packer,
            spans,
            scalars: Vec::new(),
        });
    }
    if let Some((span, line)) = try_const_single_bit(src, anchor) {
        return Ok(RustExtracted {
            kind: RustKind::SingleBit,
            spans: vec![ExtractedSpan {
                name: anchor.to_string(),
                span,
                line,
            }],
            scalars: Vec::new(),
        });
    }
    if let Some((val, line)) = try_const_scalar(src, anchor) {
        return Ok(RustExtracted {
            kind: RustKind::Scalar,
            spans: Vec::new(),
            scalars: vec![RustScalar {
                name: anchor.to_string(),
                value: val,
                line,
            }],
        });
    }
    Err(format!(
        "驱动代码里找不到 anchor `{anchor}` 的位声明(认 const fn 位段打包 / `1 << N` / `0xNN` \
         三种形态)"
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustExtracted {
    pub kind: RustKind,
    pub spans: Vec<ExtractedSpan>,
    pub scalars: Vec<RustScalar>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RustKind {
    Packer,
    SingleBit,
    Scalar,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustScalar {
    pub name: String,
    pub value: u64,
    pub line: u32,
}

/// `const fn <anchor>(params) { body }` 位段打包:抽每个参数 `(p & M) << S`。
fn try_const_fn_packer(src: &str, anchor: &str) -> Result<Option<Vec<ExtractedSpan>>, String> {
    let Some(body) = find_const_fn_body(src, anchor) else {
        return Ok(None);
    };
    let params = find_const_fn_params(src, anchor).unwrap_or_default();
    let mut spans = Vec::new();
    for p in &params {
        if let Some((mask, shift)) = scan_param_term(body, p) {
            let width = mask.count_ones();
            if width == 0 {
                return Err(format!(
                    "anchor `{anchor}` 的字段 `{p}` 掩码 0x{mask:x} 位宽为 0"
                ));
            }
            let lsb = shift;
            let msb = shift + width - 1;
            spans.push(ExtractedSpan {
                name: p.clone(),
                span: FieldSpan { lsb, msb },
                line: 0,
            });
        }
    }
    if spans.is_empty() {
        return Err(format!(
            "anchor `{anchor}` 是 const fn 但 body 里没解析到 `(param & MASK) << SHIFT` 模式"
        ));
    }
    Ok(Some(spans))
}

/// 在 body 里找 `param & 0xMASK [<< SHIFT]`,返回 (mask, shift)。SHIFT 省略=0。
fn scan_param_term(body: &str, param: &str) -> Option<(u64, u32)> {
    let bytes = body.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = body[from..].find(param) {
        let abs = from + rel;
        let before_ok = abs == 0 || !is_ident_byte(bytes[abs - 1]);
        let after = abs + param.len();
        let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
        from = abs + param.len();
        if !(before_ok && after_ok) {
            continue;
        }
        // 期望跳过空白后是 `& 0xMASK`。
        let rest = &body[after..];
        let r = rest.trim_start();
        let post_amp = r.strip_prefix('&')?.trim_start();
        let (mask, after_mask) = parse_hex_literal(post_amp)?;
        let am = after_mask.trim_start().trim_start_matches(')').trim_start();
        // 可选 `<< SHIFT`。
        let shift = if let Some(post_shl) = am.strip_prefix("<<") {
            let (n, _) = parse_decimal(post_shl.trim_start())?;
            n as u32
        } else {
            0
        };
        return Some((mask, shift));
    }
    None
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// 解析开头的一个十六进制字面量 `0xNN`,返回 (值, 剩余切片)。
fn parse_hex_literal(s: &str) -> Option<(u64, &str)> {
    let t = s.trim_start();
    let hex = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X"))?;
    let end = hex
        .find(|c: char| !c.is_ascii_hexdigit())
        .unwrap_or(hex.len());
    if end == 0 {
        return None;
    }
    let val = u64::from_str_radix(&hex[..end], 16).ok()?;
    Some((val, &hex[end..]))
}

fn parse_decimal(s: &str) -> Option<(u64, &str)> {
    let t = s.trim_start();
    let end = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    if end == 0 {
        return None;
    }
    let val = t[..end].parse().ok()?;
    Some((val, &t[end..]))
}

/// `const fn <name>` 的 body(配对大括号内容)。
fn find_const_fn_body<'a>(src: &'a str, name: &str) -> Option<&'a str> {
    let open = find_const_fn_open(src, name)?;
    let body_start = src[open..].find('{')? + open + 1;
    let bytes = src.as_bytes();
    let mut depth = 1i32;
    let mut i = body_start;
    while i < bytes.len() && depth > 0 {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    (depth == 0).then_some(&src[body_start..i - 1])
}

/// `const fn <name>(` 的起始位置。
fn find_const_fn_open(src: &str, name: &str) -> Option<usize> {
    let pat = "const fn";
    let mut from = 0usize;
    loop {
        let abs = from + src[from..].find(pat)?;
        let after = src[abs + pat.len()..].trim_start();
        if let Some(post) = after.strip_prefix(name)
            && post.trim_start().starts_with('(')
        {
            return Some(abs);
        }
        from = abs + pat.len();
    }
}

/// `const fn <name>(p1: T, p2: T, ..)` 的参数名列表。
fn find_const_fn_params(src: &str, name: &str) -> Option<Vec<String>> {
    let open = find_const_fn_open(src, name)?;
    let lp = src[open..].find('(')? + open;
    let rp = src[lp..].find(')')? + lp;
    let inner = &src[lp + 1..rp];
    let mut params = Vec::new();
    for part in inner.split(',') {
        let t = part.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(pname) = t.split(':').next() {
            let pname = pname.trim();
            if !pname.is_empty() {
                params.push(pname.to_string());
            }
        }
    }
    Some(params)
}

/// `const NAME: T = 1 << N;` → ([N,N], 行号)。
fn try_const_single_bit(src: &str, name: &str) -> Option<(FieldSpan, u32)> {
    for (i, raw) in src.lines().enumerate() {
        let line = strip_rust_line_comment(raw).trim();
        let Some(rest) = match_rust_const(line, name) else {
            continue;
        };
        let inner = rest.trim().trim_end_matches(';').trim();
        let inner = inner.trim_start_matches('(').trim_end_matches(')').trim();
        if let Some(n) = parse_rust_one_shift(inner) {
            return Some((FieldSpan { lsb: n, msb: n }, i as u32 + 1));
        }
    }
    None
}

/// `const NAME: T = 0xNN;` → (值, 行号)。
fn try_const_scalar(src: &str, name: &str) -> Option<(u64, u32)> {
    for (i, raw) in src.lines().enumerate() {
        let line = strip_rust_line_comment(raw).trim();
        let Some(rest) = match_rust_const(line, name) else {
            continue;
        };
        let inner = rest.trim().trim_end_matches(';').trim();
        let inner = inner.trim_start_matches('(').trim_end_matches(')').trim();
        if let Some(v) = inner
            .strip_prefix("0x")
            .or_else(|| inner.strip_prefix("0X"))
            && !v.is_empty()
            && v.chars().all(|c| c.is_ascii_hexdigit())
            && let Ok(val) = u64::from_str_radix(v, 16)
        {
            return Some((val, i as u32 + 1));
        }
    }
    None
}

/// `1 << N` / `1u32 << N` → N。
fn parse_rust_one_shift(s: &str) -> Option<u32> {
    let mut it = s.split("<<");
    let lhs = it.next()?.trim();
    let rhs = it.next()?.trim();
    it.next().is_none().then_some(())?;
    let one = lhs
        .strip_suffix("u32")
        .or_else(|| lhs.strip_suffix("u64"))
        .or_else(|| lhs.strip_suffix("usize"))
        .unwrap_or(lhs);
    if one != "1" {
        return None;
    }
    rhs.trim().parse().ok()
}

/// 行首 `const NAME: ... =` 的 `=` 之后的 body。
fn match_rust_const<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    // `pub const` 与私有 `const` 都认：内核驱动大量使用 crate 内私有常量，
    // 锚提取不该因可见性失明（pl031-qemu port 就栽在这）。
    let t = line
        .strip_prefix("pub ")
        .unwrap_or(line)
        .trim_start();
    let t = t.strip_prefix("const ")?.trim_start();
    let head_end = t
        .find(|c: char| c.is_whitespace() || c == ':')
        .unwrap_or(t.len());
    if &t[..head_end] != name {
        return None;
    }
    let after = &t[head_end..];
    let eq = after.find('=')?;
    Some(&after[eq + 1..])
}

fn strip_rust_line_comment(line: &str) -> &str {
    line.split("//").next().unwrap_or(line)
}

// ───────────────────────── 布局条目真值 ─────────────────────────

/// 从参考树解析一个布局条目的参考真值:对每个 `fields[].ref_symbol`,
/// 读 `<tree>/<ref.file>`,extract_c_symbol 出位段。
pub fn extract_layout_from_ref(
    r#ref: &LayoutRef,
    fields: &[crate::data::FieldClaim],
    tree_path: &std::path::Path,
) -> Result<Vec<RefField>, String> {
    let file_path = tree_path.join(&r#ref.file);
    let src =
        std::fs::read_to_string(&file_path).map_err(|e| format!("{}: {e}", file_path.display()))?;
    let mut out = Vec::new();
    for f in fields {
        match extract_c_symbol(&src, &f.ref_symbol)? {
            ExtractedC::Span(s) => out.push(RefField {
                name: f.name.clone(),
                truth: Some(s.span),
                truth_line: s.line,
                extracted_kind: "GENMASK/BIT".to_string(),
            }),
            ExtractedC::Scalar { value, line, .. } => out.push(RefField {
                name: f.name.clone(),
                truth: None,
                truth_line: line,
                extracted_kind: format!("scalar(0x{value:x})"),
            }),
        }
    }
    Ok(out)
}

/// 一个字段的参考真值(已提取)。
#[derive(Debug, Clone)]
pub struct RefField {
    #[allow(dead_code)]
    pub name: String,
    pub truth: Option<FieldSpan>,
    pub truth_line: u32,
    pub extracted_kind: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_private_const_anchor_is_visible() {
        // 私有 const（无 pub）也是合法锚：可见性不应让提取器失明。
        let src = "const RTC_CR_EN: u32 = 1 << 0;\nconst OFF: usize = 0x0c;\n";
        assert_eq!(
            extract_rust_anchor(src, "RTC_CR_EN").unwrap().spans[0].span,
            FieldSpan { lsb: 0, msb: 0 }
        );
        assert_eq!(
            extract_rust_anchor(src, "OFF").unwrap().scalars[0].value,
            0x0c
        );
    }

    #[test]
    fn c_genmask_parses() {
        let src = "#define MSG_ID_MASK GENMASK(7, 0)\n#define X GENMASK_ULL(27,18)\n";
        assert_eq!(
            extract_c_symbol(src, "MSG_ID_MASK").unwrap(),
            ExtractedC::Span(ExtractedSpan {
                name: "MSG_ID_MASK".into(),
                span: FieldSpan { lsb: 0, msb: 7 },
                line: 1,
            })
        );
        assert_eq!(
            extract_c_symbol(src, "X").unwrap(),
            ExtractedC::Span(ExtractedSpan {
                name: "X".into(),
                span: FieldSpan { lsb: 18, msb: 27 },
                line: 2,
            })
        );
    }

    #[test]
    fn c_bit_n_parses() {
        let src = "#define SDMMC_CTRL_USE_IDMAC BIT(25)\n";
        assert_eq!(
            extract_c_symbol(src, "SDMMC_CTRL_USE_IDMAC").unwrap(),
            ExtractedC::Span(ExtractedSpan {
                name: "SDMMC_CTRL_USE_IDMAC".into(),
                span: FieldSpan { lsb: 25, msb: 25 },
                line: 1,
            })
        );
    }

    #[test]
    fn c_scalar_hex_parses() {
        let src = "#define SDMMC_CTRL 0x000\n";
        let r = extract_c_symbol(src, "SDMMC_CTRL").unwrap();
        assert!(matches!(
            r,
            ExtractedC::Scalar {
                value: 0,
                line: 1,
                ..
            }
        ));
    }

    #[test]
    fn c_missing_symbol_errors() {
        assert!(extract_c_symbol("#define FOO GENMASK(1,0)\n", "BAR").is_err());
    }

    const SCMI_PACKER: &str = r#"
const fn pack_header(protocol: u32, message: u32, token: u32) -> u32 {
    (message & 0xff) | ((protocol & 0xff) << 10) | ((token & 0x3ff) << 18)
}
"#;

    #[test]
    fn rust_packer_scmi_parses() {
        let r = extract_rust_anchor(SCMI_PACKER, "pack_header").unwrap();
        assert_eq!(r.kind, RustKind::Packer);
        let by_name: std::collections::BTreeMap<&str, FieldSpan> =
            r.spans.iter().map(|s| (s.name.as_str(), s.span)).collect();
        assert_eq!(by_name.get("message"), Some(&FieldSpan { lsb: 0, msb: 7 }));
        assert_eq!(
            by_name.get("protocol"),
            Some(&FieldSpan { lsb: 10, msb: 17 })
        );
        assert_eq!(by_name.get("token"), Some(&FieldSpan { lsb: 18, msb: 27 }));
    }

    #[test]
    fn rust_single_bit_parses() {
        let src = "pub const USE_IDMAC: u32 = 1 << 25;\n";
        let (span, line) = try_const_single_bit(src, "USE_IDMAC").unwrap();
        assert_eq!(span, FieldSpan { lsb: 25, msb: 25 });
        assert_eq!(line, 1);
    }

    #[test]
    fn rust_scalar_parses() {
        let src = "pub const CTRL: usize = 0x000;\n";
        let (val, line) = try_const_scalar(src, "CTRL").unwrap();
        assert_eq!(val, 0);
        assert_eq!(line, 1);
    }

    #[test]
    fn c_one_shift_form_parses() {
        // `(1 << n)` 形态的位定义。
        let src = "#define FOO (1 << 5)\n";
        assert_eq!(
            extract_c_symbol(src, "FOO").unwrap(),
            ExtractedC::Span(ExtractedSpan {
                name: "FOO".into(),
                span: FieldSpan { lsb: 5, msb: 5 },
                line: 1,
            })
        );
    }

    #[test]
    fn c_function_macro_is_skipped() {
        // 函数宏 `NAME(args)` 不是位段 MASK,必须跳过(否则会把 (x) 当 body)。
        let src = "#define FOO(x) ((x) << 1)\n#define BAR BIT(3)\n";
        assert!(
            extract_c_symbol(src, "FOO").is_err(),
            "函数宏不应被当位段提取"
        );
        // 同名 symbol 后面有真正的位定义也照常取到。
        assert_eq!(
            extract_c_symbol(src, "BAR").unwrap(),
            ExtractedC::Span(ExtractedSpan {
                name: "BAR".into(),
                span: FieldSpan { lsb: 3, msb: 3 },
                line: 2,
            })
        );
    }

    #[test]
    fn c_multiline_genmask_parses() {
        // 多行续行 define(内核 header 常见)。旧 join_continuation 会把 `#define NAME`
        // 也并进来导致 parse_genmask 失败 —— 这里回归它。
        let src = "#define BIG GENMASK(31, \\\n                   0)\n";
        assert_eq!(
            extract_c_symbol(src, "BIG").unwrap(),
            ExtractedC::Span(ExtractedSpan {
                name: "BIG".into(),
                span: FieldSpan { lsb: 0, msb: 31 },
                line: 1,
            })
        );
    }

    #[test]
    fn c_genmask_high_lt_low_errors() {
        // high < low 的 GENMASK 非法,提取器应拒绝(fail-closed,不臆造布局)。
        let src = "#define BAD GENMASK(3, 5)\n";
        assert!(extract_c_symbol(src, "BAD").is_err());
    }

    #[test]
    fn rust_missing_anchor_errors() {
        // 驱动里没有该 anchor 时 fail-closed,不静默返回空。
        let src = "pub const UNRELATED: u32 = 0;\n";
        assert!(extract_rust_anchor(src, "pack_header").is_err());
    }
}
