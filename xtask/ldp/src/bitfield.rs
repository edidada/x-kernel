// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 位段打包工具(供 check-layout 的可读提示用)。
//!
//! **重构说明**:旧的 `check-bitfields` 是**自指**检查 —— 它比对
//! `bitfield-defs.json` 跟自己的关系(`known_wrong_layouts` vs
//! `verification_traces`),从不检查 JSON 跟 Linux 参考的关系。SCMI
//! `message_id`/`token` 记反的 bug 正是"人从参考抄错"造成的,而旧检查
//! 只能抓"已经被手写进 `known_wrong_layouts` 的那一种"抄错方式(设计
//! 文档的"已知限制"自己承认)。
//!
//! 已重构为**参考锚定**:`ldp check-layout` 从钉死的参考树机械导出布局
//! 真值,diff `fields[].claim`(以及可选的驱动代码实际写法)。本模块只
//! 保留位段打包的纯算法,供生成 diff 提示("若按正确布局打包值应是 X")
//! 时复用。`classify_trace`/`Verdict`/`merged_layout`/`trace_values`
//! 等自指逻辑已删除。

use crate::data::FieldSpan;

/// 把一组字段位段 + 字段值打包成整数(供 diff 提示用)。
#[allow(dead_code)]
pub fn pack(layout: &[(String, u32, u32)], values: &std::collections::HashMap<String, u64>) -> u64 {
    let mut acc: u64 = 0;
    for (name, lsb, msb) in layout {
        let span = FieldSpan { lsb: *lsb, msb: *msb };
        let mask = span.mask();
        let v = values.get(name).copied().unwrap_or(0) & mask;
        acc |= v << lsb;
    }
    acc
}
