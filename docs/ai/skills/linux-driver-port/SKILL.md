---
name: linux-driver-port
description: Use when porting a Linux C driver (platform/PCI) to x-kernel Rust, e.g. adapting RK3588 / new-SoC peripherals. Translates Linux kernel APIs to existing x-kernel APIs (MMIO/IRQ/DMA/sync/publish) and reports any subsystem that has no x-kernel equivalent as a gap for human decision. Invoke with /linux-driver-port.
---

# Linux Driver Port Skill

把一个 **Linux C 驱动** 转写成符合 x-kernel 框架的 **Rust 驱动**，并自动适配项目接口：
**有对应实现的 Linux API → 改调本项目接口；缺少对应实现的 → 写进缺口报告，由人决策。**

典型场景：为 RK3588 等新平台补外设驱动（MMC、网卡、I2C、SPI、看门狗…），不想逐个手写，而是以 Linux 上游驱动为蓝本翻译。

## 五条铁律（决定产出质量，必须遵守）

1. **硬件是真相。** 寄存器布局、bit 位定义、初始化序列、magic number、时序与延时——这些是硬件事实，**原样忠实翻译**，不做"优化"、不做"简化"、不擅自合并步骤。怀疑时以上游 Linux 驱动为准。

2. **绝不编造 API。** x-kernel 这边用到的每一个接口，都必须能在 [`knowledge/api-mapping.md`](knowledge/api-mapping.md) 或项目源码里找到依据。**查不到的 = 缺口**，不得凭印象写一个"看起来像"的调用。

3. **RAII 优先，最小化 `unsafe`。** 生命周期交给 `devm_*` / `Arc` / `Io` / `Box` / `DmaCoherent` 等 RAII 句柄。必须用 `unsafe` 的地方（如把映射后的 vaddr 交给硬件层）写 `// SAFETY:` 注释，说明不变量。遵循 [`code-guidelines`](../code-guidelines/SKILL.md)。

4. **缺口必报、不静默绕过。** 任何无法映射的 Linux 子系统（clk、regmap、gpio、pinctrl、reset、regulator、iommu、phy、workqueue…）必须进缺口报告（见 [`templates/gap-report.md`](templates/gap-report.md)），标注阻塞程度与可行替代，交人决策。**绝不悄悄 stub 掉一个关键依赖让驱动"看起来能跑"。**

5. **遵循项目规范。** 编码风格走 [`code-guidelines`](../code-guidelines/SKILL.md)；构建/检查/运行走 [`build-workflow`](../build-workflow/SKILL.md)。

## 这个 skill 的文件地图（progressive disclosure）

本文件只放总纲。按工作流阶段按需读取对应文件：

| 文件 | 何时读 | 内容 |
|------|--------|------|
| [`knowledge/driver-anatomy.md`](knowledge/driver-anatomy.md) | 开始前 | x-kernel 驱动的**双层结构**（胶水层 + 硬件逻辑层）与**注册三连**，附真实样例 |
| [`knowledge/api-mapping.md`](knowledge/api-mapping.md) | Phase 2 映射时 | **★ 核心**：Linux API → x-kernel API 分类映射表 |
| [`knowledge/gaps.md`](knowledge/gaps.md) | 撞到缺失子系统时 | **★ 核心**：已知缺失子系统清单 + 现状 + 替代方案 + 报告口径 |
| [`templates/driver-skeleton.md`](templates/driver-skeleton.md) | Phase 3 生成代码时 | 标准 Rust 驱动骨架（可直接填空） |
| [`templates/gap-report.md`](templates/gap-report.md) | Phase 4 写报告时 | 缺口报告固定格式 |
| [`workflow/port-workflow.md`](workflow/port-workflow.md) | 贯穿全程 | Phase 0–5 详解 |
| [`examples/dwcmshc-walkthrough.md`](examples/dwcmshc-walkthrough.md) | 需要参照时 | ★ 完整 demo：Linux `dwcmshc` MMC → x-kernel，对照项目真实实现 |

## 工作流一览（详见 `workflow/port-workflow.md`）

```
P0 采集 → P1 分析 → P2 映射 → P3 生成 → P4 缺口报告 → P5 集成校验
```

- **P0 采集**：确定 Linux 源文件路径、目标 `compatible`、目标 `DeviceKind`/总线、x-kernel 落点目录。
- **P1 分析**：通读 Linux 驱动，产出"驱动画像"（probe 步骤、依赖子系统、所用内核 API、寄存器初始化序列、中断逻辑、私有数据结构）。
- **P2 映射**：逐条 Linux API 查 `api-mapping.md`——命中记替换，未命中记缺口。
- **P3 生成**：按骨架生成**硬件逻辑层** + **胶水层** + **注册三连**；硬件序列忠实翻译。
- **P4 缺口报告**：按模板产出，标阻塞程度（blocker / works-around / optional）与替代方案。
- **P5 集成校验**：改 `firmware_specs.rs` / `<bus>/mod.rs` / `Cargo.toml`；按 `build-workflow` 跑 build + clippy；记录结果。

## 一次产出的交付物

1. **硬件逻辑层**：`drivers/<class>/src/<name>.rs`（如 `drivers/block/src/xxx.rs`）——寄存器操作、初始化序列、中断处理。
2. **胶水层**：`drivers/kdriver/src/driver_registry/<bus>/<name>.rs`——`impl DeviceDriver` + `probe_device` + `descriptor()`。
3. **注册三连改动**：`firmware_specs.rs`（compatible）、`<bus>/mod.rs`（`DRIVER_FACTORIES` + feature gate）、`drivers/kdriver/Cargo.toml`（feature）。
4. **缺口报告**：缺失子系统、阻塞程度、替代方案、可编译性结论。

## 触发方式

- 命令式：在 Claude Code 里输入 `/linux-driver-port`（由 `.claude/skills/linux-driver-port/SKILL.md` 入口加载本共享知识）。
- 文档式：直接读本目录下文件，或经 `AGENTS.md` / `CLAUDE.md` 引用加载。

## 相关 skill

- [`build-workflow`](../build-workflow/SKILL.md) —— 构建、clippy、运行、平台 target。
- [`code-guidelines`](../code-guidelines/SKILL.md) —— Rust 内核编码与审查规范（unsafe、命名、并发）。
