// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 位段打包与 false-pass 检测。
//!
//! 这是"约束支柱"的可执行核心:把 SKILL.md 里"VERSION 会骗人""验证位段必须用
//! 非零字段"这类散文铁律,变成会失败的检查。
//!
//! 判据(精确,无启发式):对每条 verification_trace,在正确布局与每个
//! known_wrong_layout 下分别打包;打包值相同 → 这条 trace **无法区分**这两种布局
//! (即 VERSION 骗人那一类)。再与数据自评 `distinguishes_layout` 交叉验证。

use std::collections::HashMap;

use crate::data::{BitField, BitfieldDef, VerificationTrace};

/// 把一组字段位置 + 字段值打包成整数。
/// `layout` 给出每个字段占用的位段(按名);`values` 给出每个字段的值。
fn pack(layout: &[(String, u32, u32)], values: &HashMap<String, u64>) -> u64 {
    let mut acc: u64 = 0;
    for (name, lsb, msb) in layout {
        let width = msb - lsb + 1;
        let mask = if width >= 64 {
            u64::MAX
        } else {
            (1u64 << width) - 1
        };
        let v = values.get(name).copied().unwrap_or(0) & mask;
        acc |= v << lsb;
    }
    acc
}

/// 由正确布局衍生出"错误布局"的字段位段表:
/// 以正确字段名为准,逐个用 wrong_layout 里的同名字段覆盖位段(未覆盖的沿用正确位段)。
/// 这忠实表达"记反了某些字段的位置"。
fn merged_layout(def: &BitfieldDef, wrong: &[BitField]) -> Vec<(String, u32, u32)> {
    let wrong_map: HashMap<&str, (u32, u32)> = wrong
        .iter()
        .map(|f| (f.name.as_str(), (f.lsb, f.msb)))
        .collect();
    def.fields
        .iter()
        .map(|f| {
            let (lsb, msb) = wrong_map
                .get(f.name.as_str())
                .copied()
                .unwrap_or((f.lsb, f.msb));
            (f.name.clone(), lsb, msb)
        })
        .collect()
}

/// 从 trace.fields(JSON object)解出 字段名→u64。
pub fn trace_values(trace: &VerificationTrace) -> Result<HashMap<String, u64>, String> {
    let obj = trace
        .fields
        .as_object()
        .ok_or_else(|| format!("trace `{}` 的 fields 不是对象", trace.name))?;
    let mut map = HashMap::new();
    for (k, v) in obj {
        let n = v
            .as_u64()
            .or_else(|| v.as_i64().map(|i| i as u64))
            .ok_or_else(|| format!("trace `{}` 字段 `{}` 不是整数", trace.name, k))?;
        map.insert(k.clone(), n);
    }
    Ok(map)
}

/// 数据自评"能否区分"的真值:只把 JSON 的 `true` 当真,其它(false/"partial"/缺省)当假。
fn claims_distinguish(trace: &VerificationTrace) -> bool {
    trace.distinguishes_layout.as_bool().unwrap_or(false)
}

/// 单条 trace 相对单个错误布局的判定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 打包值相同,trace 自评也说不能区分 → 文档化 false-pass(正常,如 VERSION)。
    DocumentedFalsePass { packed: u64 },
    /// 打包值相同,但 trace 自评说能区分 → **矛盾,数据在撒谎** → fail-closed。
    Contradiction { correct: u64, wrong: u64 },
    /// 打包值不同,trace 自评说能区分 → 正常确认。
    Confirmed { correct: u64, wrong: u64 },
    /// 打包值不同,trace 自评却说不能区分 → 欠精确(无害,提示)。
    OverPessimistic { correct: u64, wrong: u64 },
}

/// 判定单条 trace 相对单个错误布局。
pub fn classify_trace(
    def: &BitfieldDef,
    wrong: &[BitField],
    trace: &VerificationTrace,
) -> Result<Verdict, String> {
    let values = trace_values(trace)?;
    let correct_layout: Vec<(String, u32, u32)> = def
        .fields
        .iter()
        .map(|f| (f.name.clone(), f.lsb, f.msb))
        .collect();
    let wrong_layout = merged_layout(def, wrong);
    let correct = pack(&correct_layout, &values);
    let wrong = pack(&wrong_layout, &values);
    let claims = claims_distinguish(trace);
    Ok(if correct == wrong {
        if claims {
            Verdict::Contradiction { correct, wrong }
        } else {
            Verdict::DocumentedFalsePass { packed: correct }
        }
    } else if claims {
        Verdict::Confirmed { correct, wrong }
    } else {
        Verdict::OverPessimistic { correct, wrong }
    })
}
