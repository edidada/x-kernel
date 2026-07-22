---
name: linux-driver-port
description: Port a Linux C driver (platform/PCI) to x-kernel Rust and adapt it to existing x-kernel APIs, flagging any missing subsystem as a gap. Use when adapting RK3588 / new-SoC peripherals (MMC, net, i2c, spi, watchdog, ...) by translating from the upstream Linux driver. Covers API mapping (MMIO/IRQ/DMA/sync/publish), the two-layer driver structure, gap reporting for missing subsystems (clk/regmap/gpio/phy/...), and a worked dwcmshc SD/MMC example. Trigger with /linux-driver-port.
---

# Linux Driver Port（入口）

这是一个**可触发入口**。完整的共享知识在 `docs/ai/skills/linux-driver-port/`（工具中立，亦经 `AGENTS.md`/`CLAUDE.md` 引用）。

## 它做什么

把一个 **Linux C 驱动** 转写成符合 x-kernel 框架的 **Rust 驱动**，并：

- **有对应实现的 Linux API** → 改调本项目接口（MMIO/IRQ/DMA/同步/发布）。
- **缺少对应实现的** → 写进缺口报告，由人决策（绝不静默 stub）。

## 快速启动

1. **先读总纲与铁律**：[`docs/ai/skills/linux-driver-port/SKILL.md`](../../../docs/ai/skills/linux-driver-port/SKILL.md)（五条铁律是质量底线）。
2. **按工作流走**：[`workflow/port-workflow.md`](../../../docs/ai/skills/linux-driver-port/workflow/port-workflow.md)（Phase 0–5）。
3. **查映射表**：[`knowledge/api-mapping.md`](../../../docs/ai/skills/linux-driver-port/knowledge/api-mapping.md)（Linux→x-kernel API）。
4. **撞到缺失子系统**：[`knowledge/gaps.md`](../../../docs/ai/skills/linux-driver-port/knowledge/gaps.md)。
5. **写代码**：[`templates/driver-skeleton.md`](../../../docs/ai/skills/linux-driver-port/templates/driver-skeleton.md)。
6. **写报告**：[`templates/gap-report.md`](../../../docs/ai/skills/linux-driver-port/templates/gap-report.md)。
7. **参照实例**：[`examples/dwcmshc-walkthrough.md`](../../../docs/ai/skills/linux-driver-port/examples/dwcmshc-walkthrough.md)（Linux dwcmshc → x-kernel 完整走查）。

## 用法示例

```
/linux-driver-port 把 drivers/watchdog/dw_wdt.c 移植到 RK3588
```

随后按工作流：采集 → 分析 → 映射 → 生成 → 缺口报告 → 集成校验。

## 五条铁律（摘要，详见总纲）

1. **硬件是真相**——寄存器/bit/时序原样翻译。
2. **绝不编造 API**——查不到的 = 缺口。
3. **RAII 优先**——`devm_*`/`Arc`/`Io`，最小化 `unsafe`，写 `SAFETY:` 注释。
4. **缺口必报**——不静默绕过。
5. **遵循** `code-guidelines` 与 `build-workflow`。

## 相关 skill

- [`build-workflow`](../../../docs/ai/skills/build-workflow/SKILL.md) — 构建/clippy/运行/平台 target。
- [`code-guidelines`](../../../docs/ai/skills/code-guidelines/SKILL.md) — Rust 内核编码规范。
