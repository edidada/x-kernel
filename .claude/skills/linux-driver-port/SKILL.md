---
name: linux-driver-port
description: >-
  Use when porting a Linux C driver (platform/PCI) to x-kernel Rust by
  translating from the upstream driver and adapting to existing x-kernel APIs
  (MMIO/IRQ/DMA/sync/publish), reporting every missing subsystem as a gap.
  Covers API mapping, the two-layer driver structure, gap reporting, and a
  worked dwcmshc SD/MMC example. Trigger with /linux-driver-port.
  Do NOT use this for: writing a driver from scratch with no Linux reference
  (use driver-anatomy + code-guidelines instead); pure userspace/device-tree
  work; or choosing which SoC peripheral to enable next (that is a planning
  decision, not a port). This skill only translates an *existing* Linux driver
  faithfully — it will not invent hardware behavior.
---

# Linux Driver Port（入口）

这是一个**可触发入口**。完整的共享知识在 `docs/ai/skills/linux-driver-port/`（工具中立，亦经 `AGENTS.md`/`CLAUDE.md` 引用）。

## 它做什么

把一个 **Linux C 驱动** 转写成符合 x-kernel 框架的 **Rust 驱动**，并：

- **有对应实现的 Linux API** → 改调本项目接口（MMIO/IRQ/DMA/同步/发布）。
- **缺少对应实现的** → 写进缺口报告，由人决策（绝不静默 stub）。

## 快速启动

1. **先读总纲与铁律**: [`docs/ai/skills/linux-driver-port/SKILL.md`](../../../docs/ai/skills/linux-driver-port/SKILL.md)(五条铁律是质量底线)。
2. **这是 harness,不是纯文档**——skill 带可执行约束(`ldp`)。在仓库根跑:
   ```bash
   ldp help              # 全部子命令与边界(反例)
   ldp lookup <api>      # 查某条 Linux API 的 x-kernel 对应
   ldp gaps              # 列缺失子系统
   ldp check-bitfields   # 揪位段 false-pass(SCMI VERSION 那类 bug)
   ldp scan <linux源码>  # 扫源码用到的缺口符号
   ldp golden dwcmshc    # 跑 golden 回归
   ldp trace add <port> --kind gap --linux <sym>   # 记 port 轨迹(Ch8)
   ldp staging aggregate && ldp staging review    # 轨迹→候选,≥2 支持可发布
   ldp requirements gen --from-scan <src>         # 缺口→需求 issue(按类给实现路径)
   ldp requirements list                          # 看哪些待人工确认
   ```
   没装到 PATH 时:`cargo run --manifest-path xtask/Cargo.toml -p ldp -- <子命令>`。
3. **按工作流走**: [`workflow/port-workflow.md`](../../../docs/ai/skills/linux-driver-port/workflow/port-workflow.md)(Phase 0–6)。
4. **查映射表**: `ldp lookup <api>` 或 [`knowledge/api-mapping.md`](../../../docs/ai/skills/linux-driver-port/knowledge/api-mapping.md)。
5. **撞到缺失子系统**: `ldp gaps` 或 [`knowledge/gaps.md`](../../../docs/ai/skills/linux-driver-port/knowledge/gaps.md)。
6. **写代码**: [`templates/driver-skeleton.md`](../../../docs/ai/skills/linux-driver-port/templates/driver-skeleton.md)。
7. **写报告**: [`templates/gap-report.md`](../../../docs/ai/skills/linux-driver-port/templates/gap-report.md)。
8. **参照实例**: [`examples/dwcmshc-walkthrough.md`](../../../docs/ai/skills/linux-driver-port/examples/dwcmshc-walkthrough.md)(Linux dwcmshc → x-kernel 完整走查;也是 golden 基线)。

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
