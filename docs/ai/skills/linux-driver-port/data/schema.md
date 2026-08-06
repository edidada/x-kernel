# 数据层 Schema 说明

本目录的 `*.json` 是 **linux-driver-port skill 的唯一真相源**。
人类读的 `knowledge/api-mapping.md` / `knowledge/gaps.md` 由 `ldp gen-doc` 从
这里**生成**,不得手改。

> 设计依据见 `docs/ai/skills/linux-driver-port/docs/design.md` 的"上下文支柱"一节。
> 核心原则:**改数据(改 JSON),不改文档;文档是构建产物。**

## 文件职责(对齐 Ch8"四种更新方式")

| 文件 | 形态 | 内容 | 更新方式 |
|------|------|------|----------|
| `api-mapping.json` | 结构化数据(经验知识库) | Linux API → x-kernel API 映射,每条带 source 证据 + context 检索前缀 | 改 JSON → `ldp gen-doc` 重生成 md |
| `gaps.json` | 结构化数据(经验知识库) | 缺失子系统清单,带 status 状态机 + 生效/失效治理 | 改 JSON → `ldp gen-doc` 重生成 md |
| `bitfield-defs.json` | 结构化数据(经验知识库) | 寄存器/协议 wire 布局声明,每字段锡到参考符号 + 移植方 claim | 改 JSON(锡 references.json 注册的参考树) |
| `golden/dwcmshc.json` | 结构化数据(回归基线) | dwcmshc walkthrough 的期望映射/缺口/位段断言 | 改 JSON → `ldp golden` 验证 |
| `references.json` | 参考树注册表(机器可验参考) | Linux/U-Boot 等参考树(本地 submodule/vendored,带 pin) | 改 JSON + vendor 参考子树(见 references/README.md) |
| `references/` | 本地参考源 + fixture | vendored 参考子树(如 linux sparse 子集) + 合成测试 fixture | vendor 参考;改后 `ldp ref check` 验 |
| `provider-boundary.json` | 政策(provider 判据 + impl 坑) | 五判据(Stage 1 该不该抽)+ impl_pitfalls(Stage 2 怎么抽干净)+ A/B/concrete 三分 + 边界表 + per-gap 评估 | 改 JSON → requirements gen 渲染;check-data/check-provider-crate 查 |
| `trace/*.jsonl` | 不可变轨迹(Ch8 在线循环) | 每次 port 撞到的缺口/未映射 API/位段坑 | `ldp trace add`(append-only,无 edit/delete) |
| `staging/candidates.json` | 候选区(Ch8 离线循环) | 轨迹聚合的候选条目,带 support_count | `ldp staging aggregate` 生成;`promote` 标状态 |
| `requirements/*.md` + `index.json` | 需求 issue(可决策) | 每个缺口一个需求 md + 状态索引 | `ldp requirements gen` 生成;`confirm` 改状态 |

> `trace/`、`staging/`、`requirements/` 是 Ch8 进化闭环 + 需求决策的产物,**不是**正式
> 知识库——它们不会被 `lookup`/`gaps`/`check-*`/`gen-doc` 读取。要沉淀进正式库需过
> `staging promote` 门槛(≥2 支持)或 `requirements confirm` 确认 + 人工改
> `data/{api-mapping,gaps,bitfield-defs}.json` + 验证。

## 字段语义

### `api-mapping.json`

- `linux`: Linux API 或符号(原文,参数保真,Ch4 第 4 条)。
- `xkernel`: x-kernel 替换(原文)。
- `note`: 备注 / 限制。
- `source`: 证据对象 `{path, line?}`。**铁律 2 的数据化**:每条映射必须有源码依据,
  `ldp check-data` 会校验 `path` 在仓库内真实存在(防止编造)。
- `keywords`: 检索前缀(Ch3 上下文感知检索)。`ldp lookup` 用它做匹配,不只匹配 `linux` 字面。
- `status`(可选,默认 `landed`):`landed` / `partial` / `missing`——治理字段,
  表示"x-kernel 是否真的落地了这条映射"。

### `gaps.json`

- `id`: 稳定标识(供 golden / 报告引用)。
- `name`: 子系统名(报告口径)。
- `symbols`: 该子系统涉及的 Linux 符号(`ldp scan` 拿它去源码里找用法)。
- `status`: 状态机 `open` / `workaround` / `partial` / `landed`(Ch3 治理)。
- `infra_type`: 基础设施类型 `multi-provider` / `single-provider` / `framework` /
  `subsystem-class` / `driver-missing`。决定 `requirements gen` 选哪个实现路径模板。
  分类依据(贴合 clk-subsystem.md 的 provider 模式论断):
  - `multi-provider`: 多 provider 子系统(clk/gpio/pinctrl/reset/regulator/iommu/phy),
    Linux 里多 controller,需 phandle 键多 provider 注册表。
  - `single-provider`: 单全局 provider(dma/device-res 模式,`Once<&dyn>`)。
  - `framework`: 非 provider 的子框架(regmap/workqueue/timer/completion/busy-wait)。
  - `subsystem-class`: 需新 DeviceKind + publish_*(i2c/spi/watchdog/rtc)。
  - `driver-missing`: 基础设施都在,只是缺这个具体驱动。
- `effective`: `{from, until?}` 生效/失效区间(Ch3 时效治理)。
- `dimensions`(可选)： 同一子系统按使用维度分别定级(clk 的 enable/rate 二分)。
- `severity_branches`(可选, 与 dimensions 二选一)： 条件定级——一个 map, 键为分支名, 值为 `{severity, when?, label?}`。
  clk 用 `dimensions`;gpio/reset/regulator/iommu/phy/device-class 用 `severity_branches`。
  两者表达同一件事的不同形状, `ldp golden` 会同时检查主 `severity`/`dimensions`/`severity_branches` 三处的取值。
- `workaround` / `report_template`: 替代方案 + 报告口径原文。

### `bitfield-defs.json`

布局声明采用**参考锡定**范式(取代旧的自指范式):

- `scope`:`protocol`(全局共享,如 SCMI header)或 `port`(某次移植专属,配 `port` 字段)。
- `ref`:`{tree, file}`。`tree` 是 `references.json` 里注册的参考树 id;`file` 是该树内相对路径。
  `ldp check-layout` 从 `<tree>/<file>` 机械导出位段真值。
- `driver_src`(可选):`{path, anchor}`。锡到 x-kernel 驱动代码里的一个 `const fn`(位段打包函数)
  或 `const`(单位字段);`check-layout` 额外提取驱动实际写法,diff claim(D2:驱动实现 ↔ 声明)。
- `fields[]`:每项 `{name, ref_symbol, claim}`。
  - `ref_symbol`:参考树里机械可导出的符号名(如 Linux `#define MSG_ID_MASK GENMASK(7,0)`)。
  - `claim`:`{lsb, msb}` 移植方声明。**被验证的断言**,非自由字段。
  - `check-layout` 对每个字段:`claim` 必须等于参考机械导出的真值(以及,若有 `driver_src`,
    驱动实际写法)。
- `status_codes` / `spec_ref` / `diagnostic_note`:人类文档,不承重。

**已删除(旧自指范式)**:`known_wrong_layouts` + `verification_traces` + `fields[].genmask`。
旧的 `check-bitfields` 检查 JSON 跟自己的关系(`known_wrong_layouts` vs `verification_traces`),
从不检查 JSON 跟 Linux 参考的关系 —— SCMI `message_id`/`token` 记反那类 bug 只能被
"已被手写进 known_wrong_layouts 的那一种"抄错方式抓到(事后回归,非预防)。
`check-layout` 从钉死参考机械导出真值,直接抓"从参考抄错"本身。

### `references.json`

- `trees[]`:每项 `{id, path, kind, pin, required}`。
- `path`:仓库内路径(submodule 或 vendored)。**必须本地目录,拒绝 web search** ——
  缺参考即 fail-closed,这是"太容易忘了拉参考"的结构性解法。
- `kind`:`linux`/`u-boot`/`other`(决定提取器期望的文件形态)。
- `pin`:git tag/commit(submodule 由 `.gitmodules` 钉;vendored 由目录下 `VENDORED.txt` 首行记)。
- `required`:`true` = 缺失即 fail-closed。

## 验证门(`ldp` 的约束支柱,fail-closed)

- `ldp ref check`:所有 required 参考树本地就位且可确认 pin。
- `ldp check-data`:`source.path` 必须在仓库内存在;JSON 必须能解析;必填字段不缺。
- `ldp check-layout [--port]`:布局声明 ↔ 参考真值(以及可选驱动代码)。从参考机械导出位段,diff claim。
- `ldp golden <name>`:golden 断言全部命中当前数据。
- `ldp validate-gap-report <file>`:报告里声明的缺口/映射必须在数据里有据可查。
