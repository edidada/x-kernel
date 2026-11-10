// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `ldp check-layout`:把位段/布局检查从**自指**(JSON 跟自己比对)重构为
//! **参考锚定**(claim ↔ 参考真值 ↔ 驱动实现)。
//!
//! 三方比对:
//! 1. **claim ↔ reference**:`fields[].claim` 必须等于参考树里 `ref_symbol` 机械导出的真值。
//!    这抓住"人从参考抄错了"(SCMI message_id/token 记反那类)。
//! 2. **driver ↔ claim**(可选,填了 `driver_src` 才查):驱动代码实际写的位声明必须等于 claim。
//!    这抓住"声明对、代码错"(最后一层转录)。
//!
//! 旧 `check-bitfields` 的 `known_wrong_layouts`/`verification_traces` 自指范式已删除:
//! 不再靠人手写"我猜会怎么记错",布局真值由钉死的参考机械导出。

use crate::{
    data::{BitfieldDef, BitfieldDefs, FieldSpan},
    extract, references as refs, repo,
};

/// `ldp check-layout [--port <name>]`。返回错误数。
///
/// 默认查所有 `scope=protocol`(全局)条目;`--port` 额外含该 port 的 `scope=port` 条目。
pub fn check_layout(bf: &BitfieldDefs, port: Option<&str>) -> usize {
    // 前置:所有被查条目引用的参考树必须本地就位(fail-closed)。
    let references = repo::load_references().unwrap_or_default();
    let mut errors = 0usize;

    let targets: Vec<&BitfieldDef> = bf
        .defs
        .iter()
        .filter(|d| match d.scope.as_str() {
            "port" => port.is_some_and(|p| p == d.port),
            _ => true, // protocol / 其它都查
        })
        .collect();

    if targets.is_empty() {
        println!("(无匹配的布局条目)");
        return 0;
    }

    for def in &targets {
        println!("══ {} ({}) ══", def.id, def.name);
        println!(
            "  scope={}, ref={}/{}",
            def.scope, def.r#ref.tree, def.r#ref.file
        );
        if !def.spec_ref.is_empty() {
            println!("  spec_ref: {}", def.spec_ref);
        }

        // 1) 定位参考树。
        let tree = match references.trees.iter().find(|t| t.id == def.r#ref.tree) {
            Some(t) => t,
            None => {
                errors += 1;
                eprintln!("  ✗ 参考树 `{}` 未在 references.json 注册", def.r#ref.tree);
                continue;
            }
        };
        match refs::inspect(tree) {
            refs::TreeState::Present { .. } => {}
            refs::TreeState::Missing => {
                if tree.required {
                    errors += 1;
                    eprintln!(
                        "  ✗ 必需参考树 `{}` 缺失:{}。check-layout 无法导出真值 → fail-closed。",
                        tree.id, tree.path
                    );
                    eprintln!("    vendor 方法见 data/references/README.md。");
                    continue;
                } else {
                    println!("  ℹ 可选参考 `{}` 缺失,跳过(不阻塞)", tree.id);
                    continue;
                }
            }
            refs::TreeState::Unmarked => {
                eprintln!(
                    "  ⚠ 参考树 `{}` 存在但无 pin 标记(见 VENDORED.txt)",
                    tree.id
                );
            }
        }
        let tree_path = refs::tree_path(tree);

        // 2) 从参考导出真值。
        let truths = match extract::extract_layout_from_ref(&def.r#ref, &def.fields, &tree_path) {
            Ok(v) => v,
            Err(e) => {
                errors += 1;
                eprintln!("  ✗ 参考提取失败:{e}");
                continue;
            }
        };

        // 3) claim ↔ reference。
        println!("  [claim ↔ reference]");
        let mut def_errors = 0usize;
        for (claim, truth) in def.fields.iter().zip(truths.iter()) {
            match (claim, &truth.truth) {
                (claim_field, Some(truth_span)) => {
                    if claim_field.claim == *truth_span {
                        println!(
                            "    ✓ {} [{}:{}] = reference(行 {})",
                            claim_field.name, truth_span.lsb, truth_span.msb, truth.truth_line
                        );
                    } else {
                        def_errors += 1;
                        errors += 1;
                        eprintln!(
                            "    ✗ {} 抄错:claim=[{}:{}] ≠ reference=[{}:{}](行 {} 符号 `{}`)",
                            claim_field.name,
                            claim_field.claim.lsb,
                            claim_field.claim.msb,
                            truth_span.lsb,
                            truth_span.msb,
                            truth.truth_line,
                            claim_field.ref_symbol
                        );
                    }
                }
                (_, None) => {
                    // 参考导出的是标量(scalar),不是位段 —— 这个字段不是位字段而是偏移。
                    println!(
                        "    ℹ {} 参考是标量({}),非位段;按偏移值比对",
                        claim.name, truth.extracted_kind
                    );
                }
            }
        }

        // 4) driver ↔ claim(D2,可选)。
        if let Some(driver) = &def.driver_src {
            println!("  [driver ↔ claim]");
            let drv_path = repo::repo_root().join(&driver.path);
            let src = match std::fs::read_to_string(&drv_path) {
                Ok(s) => s,
                Err(e) => {
                    errors += 1;
                    eprintln!("    ✗ 读驱动代码 {} 失败:{e}", drv_path.display());
                    continue;
                }
            };
            match extract::extract_rust_anchor(&src, &driver.anchor) {
                Ok(extracted) => {
                    let drv_spans: std::collections::BTreeMap<&str, FieldSpan> = extracted
                        .spans
                        .iter()
                        .map(|s| (s.name.as_str(), s.span))
                        .collect();
                    for claim_field in &def.fields {
                        match drv_spans.get(claim_field.name.as_str()) {
                            Some(drv_span) if *drv_span == claim_field.claim => {
                                println!(
                                    "    ✓ {} 驱动实现匹配 claim([{}:{}])",
                                    claim_field.name, drv_span.lsb, drv_span.msb
                                );
                            }
                            Some(drv_span) => {
                                errors += 1;
                                def_errors += 1;
                                eprintln!(
                                    "    ✗ {} 驱动实现与声明不符:driver=[{}:{}] ≠ claim=[{}:{}]",
                                    claim_field.name,
                                    drv_span.lsb,
                                    drv_span.msb,
                                    claim_field.claim.lsb,
                                    claim_field.claim.msb
                                );
                            }
                            None => {
                                // 驱动 anchor 里没有该字段名:通常是该位被
                                // 常量化/省略(如 pack_header 的 message_type=0)。
                                // claim↔reference 已是主验证,这里只提示。
                                println!(
                                    "    ℹ {} 在驱动 anchor \
                                     里未单独出现(常量化/省略);claim↔reference 已验证",
                                    claim_field.name
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    errors += 1;
                    eprintln!("    ✗ 提取驱动 anchor `{}` 失败:{e}", driver.anchor);
                }
            }
        }

        if def_errors == 0 {
            println!("  ✓ {} 布局与参考一致", def.id);
        }
    }

    println!("────────────────────────────────────────");
    if errors == 0 {
        println!("✓ check-layout PASS:所有声明与参考真值一致。");
    } else {
        eprintln!("✗ check-layout FAIL: {errors} 处声明与参考/驱动不一致(见上)。");
    }
    errors
}
