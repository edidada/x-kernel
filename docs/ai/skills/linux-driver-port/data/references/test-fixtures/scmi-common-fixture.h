// SPDX-License-Identifier: Apache-2.0
//
// 合成测试 fixture —— **不是真实 Linux 源码**。
//
// 用途:验证 `ldp` 提取器从 C header 机械导出 GENMASK/BIT 位段的端到端链路。
// 真实 Linux 参考请按 data/references/README.md sparse-checkout vendored。
// 这里的数值刻意与 bitfield-defs.json 的 SCMI 条目一致,以便:
//   - `ldp check-layout --fixture` 能端到端 PASS(claim ↔ 参考);
//   - 改坏 claim 后能被抓住(回归测试人工演示)。
//
// 数值来自公开的 ARM SCMI 规范 message header 布局(亦即 Linux arm_scmi/common.h
// 里 MSG_*_MASK 的定义),此处仅作 fixture,不声称是从 kernel.org vendored。

#ifndef _SCMI_COMMON_FIXTURE_H
#define _SCMI_COMMON_FIXTURE_H

#define MSG_ID_MASK             GENMASK(7, 0)
#define MSG_TYPE_MASK           GENMASK(9, 8)
#define MSG_PROTOCOL_ID_MASK    GENMASK(17, 10)
#define MSG_TOKEN_ID_MASK       GENMASK(27, 18)

/* 单位字段形态(Linux BIT 宏),供提取器 BIT 分支覆盖。*/
#define SDMMC_CTRL_USE_IDMAC    BIT(25)

#endif /* _SCMI_COMMON_FIXTURE_H */
