# ldp CLI 详细使用说明

> 本文是 `ldp`(linux-driver-port skill harness)的完整使用手册:
> 覆盖调用方式、每个子命令的语法与行为、数据文件格式、典型工作流、验证门禁与已知坑。
> 设计原理见 [`design.md`](design.md),数据字段语义见 [`../data/schema.md`](../data/schema.md),
> 总纲与铁律见 [`../SKILL.md`](../SKILL.md)。

## 1. ldp 是什么

`ldp` 是 linux-driver-port skill 的**执行层**:一个 Rust xtask 二进制
(源码在 `xtask/ldp/`),把 SKILL.md 里的散文铁律变成**会失败的检查**:

- 铁律 2(不编造 API)→ `ldp check-data`(source.path 必须真实存在)
- 铁律 1/2(位段忠实)→ `ldp check-layout`(从钉死的本地参考树机械导出位段真值,diff 移植方声明)
- 铁律 4(缺口必报、先报告后生成)→ `ldp scan --report`(报告草稿直接落盘)+ `--gate`
  (可阻塞缺口未决策即退出 1,禁止进入代码生成)
- 数据/文档一致性 → `ldp golden` + `ldp gen-doc`
- 经验沉淀(Ch8)→ `ldp trace` + `ldp staging`
- 需求决策 → `ldp requirements`

数据真相源在 `docs/ai/skills/linux-driver-port/data/*.json`;`knowledge/*.md` 是
`ldp gen-doc` 的生成物,**不要手改**。

## 2. 安装与调用方式

`ldp` 是宿主 std 程序(xtask),不是内核代码,不需要交叉编译。三种调用方式:

### 2.1 通过 Makefile(推荐,仓库内)

```bash
make ldp                    # 等价于 ldp help
make ldp-check              # ref check && check-data && check-layout(完整验证门,需 vendor 参考树)
make ldp-check-data         # check-data && golden && gen-doc(轻量门,不依赖 vendor;改完 JSON 快速自检)
make ldp-gen-doc            # 从 JSON 重生成 knowledge/*.md
make ldp-golden             # 跑全部 golden 回归
make ldp-evolve             # staging aggregate && staging review(Ch8 候选聚合)
make ldp-requirements       # requirements list(看待确认需求)
```

Makefile 内部用 `cargo run --manifest-path xtask/Cargo.toml -p ldp -- <args>`,并把
target 目录隔离到 `$(TARGET_DIR)/tools/ldp`,不会污染主 workspace target。

### 2.2 直接 cargo run

```bash
cd <仓库根>
cargo run --manifest-path xtask/Cargo.toml -p ldp -- <子命令> [参数]
```

### 2.3 安装到 PATH(可选)

```bash
cargo install --path xtask/ldp
ldp help
```

> 仓库根由编译期 `CARGO_MANIFEST_DIR` 定位(`xtask/ldp` 上溯两级),与当前工作目录
> **无关**——agent 可以在任意目录调用。

## 3. 数据文件总览

| 文件/目录 | 角色 | 谁来写 |
|-----------|------|--------|
| `data/api-mapping.json` | Linux API → x-kernel API 映射(真相源) | 人/agent 手改,`gen-doc` 出 md |
| `data/gaps.json` | 缺失子系统清单 + status 状态机 + 分级(真相源) | 人/agent 手改 |
| `data/bitfield-defs.json` | 布局声明(claim ↔ 参考锚,真相源) | 人/agent 手改 |
| `data/references.json` | 参考树注册表(本地 Linux/U-Boot 子集) | 人/agent 手改 + vendor |
| `data/provider-boundary.json` | provider 判据 + impl 坑 + per-gap 评估 | 人/agent 手改 |
| `data/golden/<name>.json` | golden 回归基线(walkthrough 断言) | 人/agent 手改 |
| `data/trace/*.jsonl` | 不可变 port 轨迹(append-only) | `ldp trace add` |
| `data/staging/candidates.json` | 轨迹聚合出的候选区 | `ldp staging aggregate` |
| `data/requirements/*.md` + `index.json` | 需求 issue + 状态索引 | `ldp requirements gen/confirm` |
| `knowledge/*.md` | **生成物**(api-mapping.md / gaps.md) | `ldp gen-doc`;附录块除外 |

核心原则:**改数据(改 JSON),不改文档;文档是构建产物。** 改了 JSON 之后必须
`ldp gen-doc` 重生成 md,并跑验证门(见 §8)。

## 4. 命令参考

### 4.1 `ldp help`

打印全部子命令的用途、边界(反例)与示例。无参数运行 `ldp` 也等同 help。

```bash
ldp help
```

> 反例:help 不执行任何检查;要校验请用 `check-data` / `check-layout` / `golden`。

### 4.2 `ldp summary`

知识库概览卡片(常驻级记忆):API 映射条数/分区数、各 status 计数、缺失子系统计数、
位段定义数、golden 基线数,以及下一步命令提示。

```bash
ldp summary
```

### 4.3 `ldp lookup <api>`

查某条 Linux API 的 x-kernel 对应,返回**结构化候选**(不是全文倾倒),最多展示 12 条,
超出显式截断并提示用更窄的查询词。

```bash
ldp lookup readl
ldp lookup clk_get_rate
ldp lookup of_match_table
```

匹配逻辑(大小写不敏感,按得分排序):

- `keywords` 精确等于查询词:+10
- `keywords` 与查询词互为子串:+4
- `linux` 包含查询词:+3
- `xkernel` 包含查询词:+2

输出每条命中的分区、linux 原名、xkernel 对应、note、source(行号)。

> - 未命中退出码仍是 0,输出提示"这可能是缺口,查 `ldp gaps` 或补一条映射"。
> - 匹配是**子串匹配**:短关键词(如 `up`/`down`)可能被长查询词误命中
>   (例:`ldp lookup totally_made_up_api_xyz` 会因含 "up" 命中 semaphore)。
>   命中太多时换更精确的词。
> - **弱命中会补缺口提示**:若所有命中都只是模糊子串匹配(无 keyword/linux 名精确
>   命中),lookup 末尾打印 ⚠「按铁律 4 先当潜在缺口处理」。这避免一个真实但未映射
>   的 API 因为含某个短词被假命中,从而被误判成"已映射"而漏报缺口。
> - 无 `source` 的条目会标注「无 source — 注意核对」,通常是 Mutex/atomic/log/Box
>   这类 std 等价物,属正常。

### 4.4 `ldp gaps [--status <s>] [--id <id>]`

列出/筛选缺失子系统。

```bash
ldp gaps                      # 全部
ldp gaps --status missing     # 按状态过滤:open/workaround/partial/missing/landed
ldp gaps --id clk-framework   # 按 id 精确取一条
```

输出含:id、status、severity、symbols、现状、替代方案,以及(如有)dimensions 按维度定级。

> gaps 只查数据,不扫你的源码;扫源码用 `ldp scan`。
> `--status` 的值必须在状态机 `open/workaround/partial/missing/landed` 内,否则报错
> 退出 1(防止 `--status missng` 这类拼写错误静默返回空结果)。`--id` 无匹配只提示,
> 不报错(可用它探测某 id 是否存在)。

### 4.5 `ldp check-data`

数据完整性校验(**fail-closed**)。检查项:

- api-mapping:每条带 `source` 的映射,`source.path` 必须是仓库内相对路径
  (拒绝绝对路径与 `..` 逃逸)且**真实存在**(铁律 2 数据化);`xkernel` 不得为空。
- gaps:每条 `status` 必须在 `open/workaround/partial/missing/landed` 状态机内;
  `name` 非空;`infra_type` 合法(见 §7.5)。
- bitfield-defs:每个字段 `msb >= lsb`、`msb < 总宽`、字段位段互不重叠。
- bitfield-defs 锚存在性:每个 def 的 `ref.tree` 必须在 `references.json` 注册;
  声明了 `driver_src.path` 的必须是仓库内存在的相对路径(`scope=port` 的 def 即使
  不带 `--port` 跑 check-layout,也在这里被兜住——避免写错的锚长期藏住)。
- 重复 id 检测:gaps 的 `id`、api-mapping 的 `linux` 名、bitfield-defs 的 def `id`
  重复即报错(同名/同 id 会让 lookup 与 golden 的命中依赖顺序,隐蔽)。
- provider 边界 nudge:provider 类 gap(`multi-provider`/`single-provider`)在
  `provider-boundary.json` 无判据评估记录 → WARN(不 fail-closed,提示补论证)。

```bash
ldp check-data
```

输出末尾汇总:`check-data: N 错误, M 警告 (api=…, gaps=…, bitfields=…; …)`。

> 无 `source` 的 landed 映射(counted separately)不算错——std 等价物本来就没有
> 源码路径,报错会产生 30+ 噪音。只对"声明了 source.path 但不存在"(编造)报错。

### 4.6 `ldp ref list` / `ldp ref check`

参考树注册表管理。参考树必须是**本地目录**(submodule 或 vendored 快照),带 pin 标记,
拒绝 web search——`ldp check-layout` 从它机械导出位段真值。

```bash
ldp ref list      # 列已注册参考树 + 本地状态(✓ present / ✗ MISSING / ⚠ unmarked)
ldp ref check     # fail-closed:所有 required 参考必须本地就位且可确认 pin
```

就位判据:

- 路径存在 + 目录下有 `VENDORED.txt`(vendored,首行记 pin)或 `.git`(submodule,
  pin 由 `.gitmodules` 钉)→ `Present`
- 路径不存在 → `Missing`(required=true 时 `ref check` 报 ERROR、退出 1)
- 路径存在但无 pin 标记 → `Unmarked`(WARN)

vendor 方法见 [`data/references/README.md`](../data/references/README.md)
(推荐 Linux sparse-checkout 最小子集)。

### 4.7 `ldp check-layout [--port <name>]`

布局声明 ↔ 参考真值(以及可选驱动代码)三方比对(**fail-closed**):

1. **claim ↔ reference**:`bitfield-defs.json` 里每个 `fields[].claim{lsb,msb}` 必须等于
   参考树 `<ref.tree>/<ref.file>` 中 `ref_symbol` 机械导出的真值。C 提取器认受限词汇表:
   `#define NAME GENMASK(h,l)` / `GENMASK_ULL` / `BIT(n)` / `(1 << n)` / 裸 `0xNN`。
2. **driver ↔ claim**(可选):填了 `driver_src{path,anchor}` 才查,从驱动代码提取实际写法
   与 claim 比对。Rust 提取器认三种统一惯法:`const fn` 位段打包
   (`(p & MASK) << SHIFT | …`)、`const NAME = 1 << N`、`const NAME = 0xNN`。

```bash
ldp check-layout              # 查全部 scope=protocol(全局)条目
ldp check-layout --port dw_wdt-rk3588   # 额外查该 port 的 scope=port 条目
```

行为要点:

- 默认只查 `scope=protocol` 的 def;`--port` 只追加该 port 名下 `scope=port` 的 def。
- 引用的参考树未注册/缺失(required)→ fail-closed;可选参考缺失则跳过并提示。
- 参考导出为标量(非位段)时按"偏移"处理,只提示不硬断言。
- 驱动 anchor 里某字段未单独出现(常量化/省略)→ 提示,不报错(claim↔reference 是主验证)。

> 撞到提取器认不出的 C/Rust 写法会 fail-closed("找不到符号的位声明"),不会静默通过;
> 这是设计使然,不是 bug。A 档抓不了 `struct foo { u32 a; }` 字段偏移(要 B 档 pahole/BTF,未实现)。

### 4.8 `ldp check-bitfields [--port <name>]`(兼容别名)

旧 `check-bitfields` 的自指逻辑(`known_wrong_layouts`/`verification_traces`)已删除,
本命令**转发到 `check-layout`**,参数 `--port` 原样传递(另有别名 `--def`)。

```bash
ldp check-bitfields           # 等价 ldp check-layout
ldp check-bitfields --port dw_wdt-rk3588
```

### 4.9 `ldp check-provider-crate <Cargo.toml>`

机械查 provider crate 的 **OS-agnostic 红线**(fail-closed):声称 OS-agnostic 的
provider crate 的 `Cargo.toml` 不得依赖任何 `k*` 内核 crate 或 `of`(DT)crate。

```bash
ldp check-provider-crate drivers/clk/Cargo.toml
```

判定规则:dep 名 == `of`,或 dep 名以 `k` 开头且第二字符是小写字母
(不误伤 `smallvec`/`log`/`device_res`/`unittest` 等)。

> - 只查依赖红线;trait 完整性、注册表可注入是设计判断,走 requirements issue。
> - **不要对内核胶水 crate 跑这个命令**(如 `drivers/kdriver` 依赖 kalloc/khal 等,
>   必然报错——它不是 provider crate)。

### 4.10 `ldp scan <path> [--report <out.md>] [--gate]`

在一份 Linux 源码(文件或目录)里扫出用到的、且当前是缺口的子系统符号(铁律 4 自动化)。

```bash
ldp scan drivers/mmc/host/sdhci-of-dwcmshc.c \
  --report dwcmshc-gap-report.md --gate
# 单文件 → 缺口报告草稿落盘 + blocker 决策门检查
ldp scan drivers/watchdog/                      # 目录(递归,跳过 .git/Documentation/tools/samples),只打印命中
```

行为要点:

- 只收集 `.c` / `.h` 文件;目录下没有则报错退出 1。
- 对每个 gap subsystem 的 `symbols` 做**字面子串匹配**,命中即提示:
  `● <name> [<status>] 命中符号: … → 严重度模型: …`。
- `--report <out.md>`:把命中结果渲染成**缺口报告草稿**(依赖子系统清单 + 逐条缺口表 +
  阻塞项汇总 + 可编译性/集成占位)并落盘。这是「先报告、后生成」的第一步——草稿在
  **写代码之前**存在,P4 是往草稿里填证据,不是事后补。
- `--gate`:检查 **blocker 决策门**——命中的**可阻塞缺口**(严重度模型含 `blocker`/
  `conditional`,或未声明;`ldp scan` 会显示「严重度模型」)若没有**已决策**的需求 issue
  (`confirmed`/`in-progress`/`landed`/`rejected`),打印未决策清单并**退出 1**,禁止进入
  代码生成。纯 `works-around`/`optional` 缺口(regmap/pinctrl/workqueue…)不触发门。
  解决路径:`ldp requirements gen --from-scan <path>` → `ldp requirements confirm <id>
  --status confirmed`(或明确接受"驱动当前不可用"标 `rejected`)→ 重跑 `--gate` 通过。
- 不带 `--report`/`--gate` 时保持旧行为:只提示,退出码 0(信息性)。
- 未命中任何符号时输出提示,退出 0。

> scan 是字面匹配,不解析 C 语法:可能漏掉宏包装的调用;过短符号(≤3 字符,如 `RCU`)
> 命中会就地标注「⚠短词,可能误匹配」(代价低:多提示一条,不会静默漏报)。`--gate` 只查
> "缺口有没有决策",不替你判断 conditional 分支——分支命中哪个取决于本驱动实际用法,由
> 人工在需求 issue/报告里判定。

### 4.11 `ldp gen-doc`

从 `data/*.json` 重新生成 `knowledge/api-mapping.md` 与 `knowledge/gaps.md`(数据为源)。

```bash
ldp gen-doc
```

行为要点:

- 生成的 md 顶部带 banner(「勿手改——改 JSON 后重跑 gen-doc」)。
- 表格部分由 JSON 生成;`GEN-DOC:APPENDIX-START/END` 之间的**受保护参考块**
  (trait 签名、bring-up 坑叙事等手写内容)会被抽出来原样回填,不被覆盖。
- `write_if_changed`:内容不变不写文件(不抖 mtime,幂等)。
- 只写 `knowledge/` 两个文件,**不改 `data/*.json`**。

### 4.12 `ldp golden [<name>]`

跑 golden 回归:把 walkthrough 声明的期望映射/缺口对照当前数据。

```bash
ldp golden                    # 列全部基线并逐个跑
ldp golden dwcmshc            # 只跑指定基线
```

断言语义:

- **映射断言(FAIL_TO_PASS)**:`expected_api_mappings` 每条必须在 `api-mapping.json`
  找到对应(linux 名或 keyword 命中 + `xkernel` 含指定子串 + `source.path` 含指定子串)。
  walkthrough 引用编造/已删 API 即 FAIL。
- **缺口断言(PASS_TO_PASS)**:带标准 id 的 `expected_gaps` 必须命中 `gaps.json`
  且 severity 一致。severity 匹配覆盖三种形态:主 `severity` / `dimensions` /
  `severity_branches`。**空 id 的缺口只提示不硬断言**(本项目独有的子框架概念,
  如 sdhci-core/mmc-core)。

退出码:全部 PASS = 0;任一 FAIL = 1。无基线文件时报错退出 1。

### 4.13 `ldp trace add/list/show`

Port 轨迹(Ch8 在线循环):**不可变、append-only** 的观察记录,每个 port 一个
`data/trace/<port>.jsonl`。

```bash
# 追加一条事件
ldp trace add dw_wdt-rk3588 \
  --kind gap \
  --linux clk_set_rate \
  --note 'probe needs rate' \
  --evidence 'drivers/clk/clk.c:1234'

ldp trace list                 # 列所有轨迹文件 + 事件数
ldp trace show dw_wdt-rk3588   # 看某 port 的全部事件(含时间戳/kind/linux/note/evidence)
```

三种 kind:

| kind | 含义 |
|------|------|
| `lookup-miss` | `ldp lookup` 未命中(Linux API 在 api-mapping 没有) |
| `gap` | 撞到缺失子系统的用法 |
| `bitfield-pitfall` | 位段/协议常量易错点 |

参数约束:

- `port`:只允许字母数字/连字符/下划线(文件名直接拼,防路径遍历)。
- `kind`:仅 `lookup-miss` / `gap` / `bitfield-pitfall`,其它报错退出 1。
- **没有 edit/delete 命令**:写错的当场观察当历史保留;真错靠 `staging promote`
  的 ≥2 门槛挡住,不靠改写历史。
- **坏行跳过 + warn,不连坐整文件**:`trace show`/内部读取遇某行 JSON 损坏时跳过该行
  并打 WARN,仍返回其余合法事件(轨迹是不可变证据,一行坏数据不该让整个 port 的轨迹
  被 `staging aggregate` 静默丢弃)。

### 4.14 `ldp staging aggregate / review / promote`

离线进化循环(Ch8):把轨迹聚合成候选,过门槛后标 promoted(不自动改正式 data/)。

```bash
ldp staging aggregate         # 聚合所有 trace/*.jsonl → staging/candidates.json
ldp staging review            # 列候选,标 ★ 可发布(support≥2 且未 promoted)/ · 候选
ldp staging promote --id gap:clk_set_rate   # 标 promoted + 打印入库指引
```

规则:

- 候选 id = `<kind>:<linux>`(如 `gap:clk_set_rate`、`lookup-miss:foo_api`),
  跨 aggregate 稳定。
- `support_count` = 支持该 (kind,linux) 的**不同 port 轨迹数**;note 取最长那条。
- 发布门槛 `PROMOTE_THRESHOLD = 2`(硬编码在二进制,验证器不可自我修改):
  `support_count < 2` 或已 `promoted` 时 `promote` fail-closed(退出 1)。
- `aggregate` 保留已 promoted 候选的状态(不会重置回 new),避免重复发布。
- `promote` 只改 `candidates.json` 状态 + 打印入库指引,**不自动写正式 data/**
  (安全边界:入库要人判断"加进哪个 section / 用什么 xkernel")。

> `promote` 对 `bitfield-pitfall` 候选打印的指引已是参考锚定范式:加一个带
> `ref`/`ref_symbol` 锚的 def 进 `data/bitfield-defs.json`(旧字段 `known_wrong_layouts`/
> `verification_traces` 已删,指引里也明确告知别再用)。

### 4.15 `ldp requirements gen / list / show / confirm`

把"发现缺口"升级为"可决策的需求 issue"。

```bash
# 从一份 Linux 源码扫描发现的缺口生成 issue
ldp requirements gen --from-scan drivers/watchdog/dw_wdt.c

# 从一个 port 的轨迹生成(lookup-miss/gap 事件)
ldp requirements gen --from-trace dw_wdt-rk3588

ldp requirements list                          # 列全部(proposed 高亮,提示待确认)
ldp requirements list --status confirmed       # 按状态过滤
ldp requirements show req-clk-framework        # 看某个 issue 全文
ldp requirements confirm req-clk-framework --status confirmed
```

行为要点:

- `--from-scan` 与 `--from-trace` **互斥**,必须二选一;都不给则报错退出 1。
- 每个命中的 gap 生成一个 `requirements/req-<id>.md` + 更新 `index.json`。
  issue id 稳定(`req-<gap_id>` 或新发现的 `req-<symbol>` 净化形式),幂等:
  同 gap_id+symbol 重复 gen 保留 status/confirmed_ts,只更新内容字段。
- 每个 issue 按 `gaps.json` 的 `infra_type` 分类,渲染「建议实现路径」与「验收标准」
  (分类不对就改 gaps.json 的 infra_type 后重新 gen)。
- provider 类 issue(`multi-provider`/`single-provider`)额外渲染
  「分类依据(provider 判据)」五判据表 + 「实现质量清单(cross-kernel 坑,Stage 2)」。
- 状态机:`proposed → confirmed → in-progress → landed`;`rejected` 是另一终态。
  `landed`/`rejected` 拒绝重复变更(避免反复)。
- `gen` **不改 `gaps.json`**;`confirm` 才改 `index.json` 里的状态。
- confirm 成功后有收尾提示:实现后把 gaps.json 对应 subsystem status 改 `landed`,
  跑 `ldp check-data && ldp gen-doc`,再 `confirm <id> --status landed` 收尾。

## 5. 典型工作流

### 5.1 移植驱动(P0–P7)

```
P0 采集  确定 Linux 源路径 / compatible / 落点
P1 分析  读 Linux 驱动;撞到 API 就用 lookup,撞到缺失子系统就记 trace
P2 映射  ldp lookup <api> 逐条查;ldp scan <linux源码> --report <草稿> 揪缺口符号 + 报告草稿落盘
P2.5 门  ldp requirements gen --from-scan && confirm;ldp scan <源码> --gate 通过(退出 0)才进 P3
P3 生成  按 templates/driver-skeleton.md 写代码;位段声明进 bitfield-defs.json
P4 报告  往 P2 草稿填证据(逐条处置/参考序列对照表/集成结论);blocker 决策已在 P2.5 完成
P5 集成  build/clippy(build-workflow)
P6 带机 真机校验
P7 沉淀  ldp trace add 记撞墙点 → ldp staging aggregate/review/promote
```

### 5.2 数据维护者(改了 JSON 之后)

```bash
ldp check-data              # 数据完整性
ldp check-layout            # 位段声明 ↔ 参考真值(改了 bitfield-defs 必跑)
ldp golden                  # walkthrough 断言不漂移
ldp gen-doc                 # 重生成 knowledge/*.md
```

`make ldp-check` 已把前三步里 `ref check`/`check-data`/`check-layout` 串成一条命令。

### 5.3 Ch8 经验沉淀

```bash
ldp trace add <port> --kind gap --linux <sym> --note '...'   # 在线:撞到就记
ldp staging aggregate                                          # 离线:聚合
ldp staging review                                             # 看哪些 ≥2 支持
ldp staging promote --id <id>                                  # 标 promoted + 指引
```

然后把候选手动并入正式 `data/*.json`,再跑 §5.2 的验证门;任一失败回滚。

### 5.4 需求决策

```bash
ldp requirements gen --from-scan <linux源码>    # 缺口 → issue
ldp requirements list                           # 看 proposed
ldp requirements show <id>                      # 审阅
ldp requirements confirm <id> --status confirmed
# 实现后:gaps.json 对应条目 status → landed;check-data && gen-doc;confirm --status landed
```

## 6. 退出码与 CI 语义

- 所有**校验类**命令 fail-closed:出错退出 1(`check-data` / `ref check` /
  `check-layout` / `check-bitfields` / `check-provider-crate` / `golden`)。
- **信息类**命令(除参数错误外)退出 0:`help` / `summary` / `lookup`(未命中也是 0)/
  `gen-doc` / `trace list|show` / `staging review`。`scan` 不带 `--gate` 时有命中也是 0;
  带 `--gate` 且存在未决策的可阻塞缺口则退出 1(决策门)。
  `gaps` 正常退出 0,但 `--status` 给了非法状态值时退出 1(拼写保护)。
- **写命令**成功退出 0,参数/状态错误退出 1:`trace add`(非法 kind/port 名)、
  `staging promote`(支持数不足/已 promoted)、`requirements gen|confirm`(参数错/终态)。
- 进程级资源释放,无自定义 Drop;每次调用是一次性进程,无共享状态、无锁。

## 7. 数据编辑速查(必填字段)

### 7.1 api-mapping.json

```json
{
  "linux": "readl",
  "xkernel": "Io::read32(offset)",
  "note": "…",
  "source": {"path": "drivers/device-res/src/mmio.rs", "line": 42},
  "keywords": ["readl", "writel", "mmio"],
  "status": "landed"
}
```

- `source.path` 必须存在且为仓库相对路径(无 source 时该条不计错,但尽量补)。
- `status`:`landed` / `partial` / `missing`(默认 landed)。

### 7.2 gaps.json

```json
{
  "id": "clk-framework",
  "name": "clk framework",
  "symbols": ["clk_get", "clk_prepare_enable", "clk_get_rate"],
  "status": "partial",
  "xkernel_status": "…",
  "severity": "conditional",
  "dimensions": [{"name": "clk-rate", "linux": ["clk_get_rate"], "severity": "…", "workaround": "…"}],
  "infra_type": "multi-provider"
}
```

- `status` 状态机:`open` / `workaround` / `partial` / `missing` / `landed`。
- 分级可三选一:主 `severity`、`dimensions[]`、`severity_branches`(与 dimensions 二选一)。
- `infra_type` 五选一:`multi-provider` / `single-provider` / `framework` /
  `subsystem-class` / `driver-missing`。

### 7.3 bitfield-defs.json(参考锚定范式)

```json
{
  "id": "scmi-msg-header",
  "scope": "protocol",
  "ref": {"tree": "linux", "file": "drivers/firmware/arm_scmi/common.h"},
  "driver_src": {"path": "platforms/kplat-aarch64/src/peripherals/scmi.rs", "anchor": "pack_header"},
  "width": 32,
  "fields": [
    {"name": "message", "ref_symbol": "MSG_ID_MASK", "claim": {"lsb": 0, "msb": 7}}
  ]
}
```

- `scope`:`protocol`(全局)或 `port`(配 `port` 字段,`check-layout --port` 才查)。
- `ref.tree` 必须是 `references.json` 注册的本地参考树 id。
- `fields[].claim` 是**被验证的断言**,不是自由字段;`check-layout` 从参考机械导出真值比对。
- `driver_src.anchor` 指向驱动里的 `const fn`(位段打包)或 `const`(单字段/偏移)。

### 7.4 references.json

```json
{"id": "linux", "path": "docs/ai/skills/linux-driver-port/data/references/linux",
 "kind": "linux", "pin": "", "required": true}
```

`required=true` 时缺失即 fail-closed。vendored 目录必须有 `VENDORED.txt`(首行 pin),
submodule 用 `.git` 作信号(pin 由 .gitmodules 钉)。

### 7.5 golden/<name>.json

```json
{
  "id": "dwcmshc",
  "source_doc": "examples/dwcmshc-walkthrough.md",
  "expected_api_mappings": [{"linux": "readl", "xkernel_contains": "read32", "source_substring": ""}],
  "expected_gaps": [{"id": "clk-framework", "label": "clk", "severity": "works-around", "handling": "…"}],
  "expected_verdict": "受限可用"
}
```

带 id 的缺口断言必须命中 gaps.json 且 severity 一致;空 id 只提示。

## 8. 注意事项与已知坑汇总

1. **lookup 用子串打分**:短查询词(如 `up`)可能命中多条;全是弱子串命中(无 keyword/
   linux 名精确命中)时,工具末尾会补 ⚠ 缺口提示,不会把假 API 静默当"已映射"。命中过多
   换更精确的词。
2. **scan 默认只提示;要当门禁/出报告请显式加 `--gate`/`--report`**。决策门在
   P2→P3 边界(先报告、后生成),不是事后检查:`--gate` 退出 1 = 还有可阻塞缺口没决策,
   先 `ldp requirements gen --from-scan` + `confirm`,别急着写代码。
3. **check-layout 必须 vendor 参考树**:未 vendor 时 fail-closed 是设计要的"长牙",不是
   bug(vendor 方法见 references/README.md)。只想快速自检数据、暂不 vendor 时跑
   `make ldp-check-data`(check-data + golden + gen-doc 子集,不依赖参考树)。
4. **check-provider-crate 只适用于 OS-agnostic provider crate**,不要对内核胶水 crate 跑
   (如 `drivers/kdriver` 必然依赖 `k*`,会报一堆错——它本来就不是 provider crate)。
5. **trace 不可变**:没有 edit/delete,写错当历史保留(真错靠 `staging promote` 的 ≥2
   门槛挡住)。读取遇坏行自动跳过 + warn,不连坐整文件。
6. **staging promote 不自动改 data/**:标 promoted 后要人手动入库并跑验证门(Ch8 安全
   边界:入库是判断,不是机械操作)。
7. **gen-doc 的附录块受保护**:`GEN-DOC:APPENDIX` 之间的内容不会被覆盖,要改参考块就
   直接编辑那个区间。
8. **改数据必须跑验证门**:`make ldp-check-data`(轻量,不依赖 vendor)或完整
   `make ldp-check`(含 check-layout,需 vendor);审计清单见 design.md。

## 9. 相关文档

- [`../SKILL.md`](../SKILL.md):总纲、五条铁律、工作流一览
- [`design.md`](design.md):harness 架构、参考锚定算法、Ch8 双循环、设计决策
- [`../data/schema.md`](../data/schema.md):数据字段语义
- [`../data/references/README.md`](../data/references/README.md):参考树 vendor 流程
- [`../data/trace/README.md`](../data/trace/README.md) / [`../data/staging/README.md`](../data/staging/README.md):Ch8 轨迹与候选
- [`../data/requirements/README.md`](../data/requirements/README.md):需求 issue
- [`../workflow/port-workflow.md`](../workflow/port-workflow.md):Phase 0–7 移植流程
- [`../examples/dwcmshc-walkthrough.md`](../examples/dwcmshc-walkthrough.md):golden 基线的完整 walkthrough
