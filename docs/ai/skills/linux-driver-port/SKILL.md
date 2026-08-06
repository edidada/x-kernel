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

# Linux Driver Port Skill

把一个 **Linux C 驱动** 转写成符合 x-kernel 框架的 **Rust 驱动**:
**有对应实现的 Linux API → 改调本项目接口;缺少对应实现的 → 写进缺口报告,由人决策。**

典型场景:为 RK3588 等新平台补外设驱动(MMC、网卡、I2C、SPI、看门狗…),以 Linux 上游驱动为蓝本翻译。

## 这个 skill 现在是 harness,不只是文档

本 skill 不止是 markdown。它有一组**可执行约束**(`ldp` CLI),把下面五条铁律里有四条变成**会失败的检查**:

| 铁律 | 散文 | ldp 怎么强制 |
|------|------|-------------|
| 1 硬件/协议是真相 | ✓ | `ldp check-layout` 从参考机械导出布局,diff claim || 2 绝不编造 API/位段 | ✓ | `ldp check-data`(source.path 必须在仓库存在)+ `check-layout` + `ref check` |
| 4 缺口必报、不静默绕过 | ✓ | `ldp scan <源码> --report <草稿>` 直接产出缺口报告草稿(先报告);`--gate` 在可阻塞缺口未决策时失败(后生成) |
| (数据/文档一致性) | — | `ldp golden`(walkthrough 断言对照数据)+ `ldp gen-doc`(md 由 JSON 生成) |
| (经验沉淀,Ch8) | — | `ldp trace`(记轨迹)+ `staging`(≥2 支持才发布,不污染正式库) |
| (需求决策) | — | `ldp requirements`(缺口→分类→需求 issue,人工确认后再移植) |
| (provider 边界) | — | provider 类 issue 渲染五判据 + check-data WARN nudge(防 provider 泛滥) |

> 铁律 3(RAII)和 5(规范)由 `code-guidelines` / `build-workflow` 兜底,不在 ldp 范围。

`ldp` 用法:`cd` 到仓库根,跑 `ldp help`。数据层(真相源)在 `data/*.json`,改数据后跑 `ldp gen-doc` 重生成 `knowledge/*.md`。

```bash
# 仓库根下:
ldp lookup readl            # 查某条 Linux API 的 x-kernel 对应
ldp gaps --status missing   # 列缺失子系统
ldp check-data              # 校验数据完整性(fail-closed)
ldp ref check                # 校验本地参考树就位(fail-closed,拒绝 web search)
ldp check-layout             # 布局声明 ↔ 参考真值(以及驱动代码)(fail-closed)
ldp scan drivers/mmc/host/sdhci-of-dwcmshc.c \
  --report docs/ai/skills/linux-driver-port/examples/dwcmshc-gap-report.md --gate
# 扫源码 → 报告草稿落盘(先报告)→ blocker 决策门(未决策退出 1,禁止先写代码)
ldp golden dwcmshc          # 跑 golden 回归(walkthrough 断言 vs 当前数据)
ldp gen-doc                 # 从 data/*.json 重生成 knowledge/*.md

# Ch8 经验沉淀(port 撞到东西时记轨迹,事后离线发布):
ldp trace add dw_wdt-rk3588 --kind gap --linux clk_set_rate --note 'probe needs rate'
ldp staging aggregate && ldp staging review
ldp staging promote --id gap:clk_set_rate   # ≥2 支持才放行;不自动改 data/

# 需求决策(扫驱动发现缺口→生成需求 issue→人工确认):
ldp requirements gen --from-scan drivers/watchdog/dw_wdt.c
ldp requirements list                      # 看哪些待确认(proposed)
ldp requirements confirm req-clk-framework --status confirmed
```

> 没装 `ldp` 到 PATH 时,直接 `cargo run --manifest-path xtask/Cargo.toml -p ldp -- <子命令>`。

> 完整命令参考、数据格式、验证门与已知坑见 [`docs/ldp-usage.md`](docs/ldp-usage.md)。

## 五条铁律(决定产出质量,必须遵守)

1. **硬件/协议是真相。** 寄存器布局、bit 位定义、**协议线格式(SCMI/SMBus/… 的 message header/payload 位段)**、初始化序列、magic number、时序与延时——这些是硬件/协议事实,**原样忠实翻译**,不做"优化"、不擅自合并步骤。怀疑时以上游 Linux 驱动/规范为准。位段声明写进 `data/bitfield-defs.json`,由 `ldp check-layout` 从钉死的参考树机械导出真值守护。

2. **绝不编造 API,也绝不凭记忆写常量/位段。** x-kernel 这边用到的每一个接口,**以及从 Linux 翻译过来的每一个 bit 位段、协议线格式、magic number、struct 字段偏移**,都必须能在 `data/api-mapping.json`(有 source 证据)、项目源码、或**本地参考树**里找到**逐位依据**。查不到的 = 缺口。位段最容易记反——SCMI `message_id`/`token` 那次 bug 就是凭记忆写错。`ldp ref check` 校验参考树本地就位(拒绝 web search),`check-layout` 从参考机械导出位段真值,diff `claim`(以及驱动代码实际写法),直接抓"从参考抄错"本身。

3. **RAII 优先,最小化 `unsafe`。** 生命周期交给 `devm_*`/`Arc`/`Io`/`Box`/`DmaCoherent` 等 RAII 句柄。必须用 `unsafe` 的地方写 `// SAFETY:` 注释。遵循 [`code-guidelines`](../code-guidelines/SKILL.md)。

4. **缺口必报、不静默绕过。** 任何无法映射的 Linux 子系统必须进缺口报告(用 [`templates/gap-report.md`](templates/gap-report.md)),标注阻塞程度。`data/gaps.json` 是已知缺口库;`ldp scan <源码>` 会告诉你这份 Linux 驱动用到了哪些缺口符号,提醒它们**必须**在报告里写明。

5. **遵循项目规范。** 编码风格走 [`code-guidelines`](../code-guidelines/SKILL.md);构建/检查/运行走 [`build-workflow`](../build-workflow/SKILL.md)。

## 协议/位段常量纪律(铁律 1/2 的延伸——最容易栽的地方)

位段、协议线格式、magic number 是**凭记忆最易记反、出错最隐蔽**的一类:API 写错编译器会报,**位段错了能编过,甚至可能恰好跑通**(全零字段掩盖错误)。纪律:

- **即使"记得",也必须打开源码逐位核对后再写**,优先复制参考实现的掩码(如 SCMI 的 `MSG_*_MASK = GENMASK(...)`)。
- **位段声明进 `data/bitfield-defs.json`**:每字段锡 `ref_symbol` + 写 `claim{lsb,msb}`,`ref` 锡 `references.json` 注册的本地参考树。`ldp check-layout` 从参考机械导出位段真值(MSG_ID_MASK GENMASK(7,0) → [0,7]),diff `claim`。旧的 `known_wrong_layouts`/`verification_traces` 自指范式已删。
- **验证位段必须用非零字段**,别用全零字段(VERSION)做基线——这正是 `bitfield-defs.json` 里 VERSION 被标 `distinguishes_layout:false` 的原因。
- **源码本地没有时**:联网取;取不到就显式标 `// UNVERIFIED — 待对照 Linux 源码` 并进缺口报告,**不得静默凭记忆落地**。把错的位段写进注释/rustdoc 当事实会让后续审查绕过去。

## 数据层(真相源)与文档层(生成物)

```
data/                          ← 真相源(改这里)
  api-mapping.json               Linux API → x-kernel API(每条带 source + keywords)
  gaps.json                      缺失子系统(status 状态机 + 维度分级 + 治理)
  bitfield-defs.json             布局声明(ref_symbol 锡参考 + claim;参考锡定范式)
  references.json + references/  本地参考树(必须本地,拒绝 web search;见 references/README.md)
  golden/dwcmshc.json            golden 回归基线(walkthrough 断言)
knowledge/                     ← 生成物(ldp gen-doc 产出,勿手改)
  api-mapping.md                 由 api-mapping.json 生成
  gaps.md                        由 gaps.json 生成
```

**原则:改数据(改 JSON),不改文档;文档是构建产物。** `schema.md` 解释字段语义。

## 文件地图(progressive disclosure)

本文件只放总纲。按工作流阶段按需读取对应文件:

| 文件 | 何时读 | 内容 |
|------|--------|------|
| [`knowledge/driver-anatomy.md`](knowledge/driver-anatomy.md) | 开始前 | x-kernel 驱动**双层结构**(胶水层+硬件逻辑层)与**注册三连**,附真实样例 |
| `ldp lookup` / [`knowledge/api-mapping.md`](knowledge/api-mapping.md) | Phase 2 映射时 | **★ 核心**:Linux API → x-kernel API 分类映射表(数据为源) |
| `ldp gaps` / [`knowledge/gaps.md`](knowledge/gaps.md) | 撞到缺失子系统时 | **★ 核心**:已知缺失子系统清单 + 现状 + 替代方案(数据为源) |
| [`knowledge/clk-subsystem.md`](knowledge/clk-subsystem.md) | 撞 clk / `clk_get_rate` 时 | Tier-0 clk 框架分层 + 消费者用法 + SCMI header 位段坑 |
| [`docs/ldp-usage.md`](docs/ldp-usage.md) | 跑命令/维护数据/过验证门时 | ldp CLI 详细使用说明:命令参考、数据格式、工作流、退出码与坑 |
| [`templates/driver-skeleton.md`](templates/driver-skeleton.md) | Phase 3 生成代码时 | 标准 Rust 驱动骨架(可直接填空) |
| [`templates/gap-report.md`](templates/gap-report.md) | Phase 4 写报告时 | 缺口报告固定格式 |
| [`workflow/port-workflow.md`](workflow/port-workflow.md) | 贯穿全程 | Phase 0–6 详解(P6=真机带机校验/triage) |
| [`examples/dwcmshc-walkthrough.md`](examples/dwcmshc-walkthrough.md) | 需要参照时 | ★ 完整 demo:Linux dwcmshc → x-kernel(golden 基线) |

## 工作流一览(详见 `workflow/port-workflow.md`)

```
P0 采集 → P1 分析 → P2 映射(+scan 报告草稿) → P2.5 blocker 决策门 → P3 生成 → P4 缺口报告(补全草稿) → P5 集成校验 → P6 带机校验/triage → P7 经验沉淀
```

- **P0 采集**:确定 Linux 源文件路径、目标 `compatible`、`DeviceKind`/总线、x-kernel 落点。
- **P1 分析**:通读 Linux 驱动,产出"驱动画像" + **编号参考序列清单**(钉死在一个来源:U-Boot 或 Linux)。
- **P2 映射**:逐条 Linux API 跑 `ldp lookup`(命中记替换) / `ldp scan --report`(揪缺口符号 + 报告草稿落盘)。
- **P2.5 blocker 决策门**:可阻塞缺口(严重度模型含 blocker/conditional)先生成需求 issue 并人工决策,`ldp scan <源码> --gate` 退出 0 才允许生成代码。
- **P3 生成**:按骨架生成硬件逻辑层 + 胶水层 + 注册三连;硬件序列忠实翻译 + 可追溯校验。
- **P4 缺口报告**:往 P2 的草稿里填证据(逐条处置、参考序列对照表、集成结论),标阻塞程度。
- **P5 集成校验**:改 `firmware_specs.rs`/`<bus>/mod.rs`/`Cargo.toml`;按 `build-workflow` 跑 build + clippy。**编译过≠可用**。
- **P6 带机校验/triage**:加错误路径可观测性;真机跑通该设备类的代表操作。**P6 没过 = 驱动不可用**。
- **P7 经验沉淀(Ch8)**:`ldp trace` 记撞墙点,`ldp staging` 把 ≥2 支持的候选沉淀进正式知识库。

## 一次产出的交付物

1. **硬件逻辑层**:`drivers/<class>/src/<name>.rs`——寄存器操作、初始化序列、中断处理。
2. **胶水层**:`drivers/kdriver/src/driver_registry/<bus>/<name>.rs`——`impl DeviceDriver` + `probe_device` + `descriptor()`。
3. **注册三连改动**:`firmware_specs.rs`、`<bus>/mod.rs`、`drivers/kdriver/Cargo.toml`。
4. **缺口报告**:缺失子系统、阻塞程度、替代方案、可编译性结论。

## 触发方式

- 命令式:在 Claude Code 里输入 `/linux-driver-port`(由 `.claude/skills/linux-driver-port/SKILL.md` 入口加载本共享知识)。
- 文档式:直接读本目录下文件,或经 `AGENTS.md`/`CLAUDE.md` 引用加载。

## 相关 skill

- [`build-workflow`](../build-workflow/SKILL.md) —— 构建、clippy、运行、平台 target。
- [`code-guidelines`](../code-guidelines/SKILL.md) —— Rust 内核编码与审查规范(unsafe、命名、并发)。
