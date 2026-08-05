# linux-driver-port skill — 设计文档

> 本文件是 skill 自身的架构文档,解释它如何从"一堆 markdown"升级成一个
> **Harness**(上下文 + 工具 + 约束 + 验证 + 纠正)。度量依据:Agent = Model + Harness 公式。

## 定位

linux-driver-port skill 把"以 Linux C 驱动为蓝本,翻译成 x-kernel Rust 驱动"这件事,
从**纯靠 agent 记住散文铁律**升级为**有可执行约束守护**的工作流。它服务两类消费者:

- **AI agent**(主):按工作流翻译驱动时,用 `ldp` 查询/校验,而非肉眼读 markdown。
- **人**:维护缺口库、位段定义、golden 基线;读生成的 `knowledge/*.md`。

## 背景

升级的直接动因是一次真机 bug:SCMI 消息头 `message_id`/`token` 位段被凭记忆写反,
而 VERSION(`msg=0`/`token=0`)恰好通,于是位段错误一路没人质疑,被误判成"固件 stub"。
这暴露了纯散文 skill 的根本缺陷:**铁律只是声明,没有任何执行层强制**——动作都执行了,
任务从未真正完成(假成功)。本设计把"位段错了 VERSION 会骗人"这句散文,变成 `ldp check-layout`
里一个会失败的检查——通过从钉死的参考树机械导出布局真值,diff 移植方声明。

## 范围

| 文件/目录 | 角色 |
|-----------|------|
| `SKILL.md` | L0 入口:总纲、铁律、工作流、文件地图(含反例 description) |
| `data/*.json` | **真相源**:api-mapping / gaps / bitfield-defs / golden |
| `data/trace/*.jsonl` | 不可变轨迹(Ch8 在线循环产出) |
| `data/staging/candidates.json` | 候选区(Ch8 离线循环产出) |
| `data/requirements/*.md` + `index.json` | 需求 issue(需求决策产出) |
| `knowledge/*.md` | 生成物:`ldp gen-doc` 从 JSON 产出(api-mapping.md / gaps.md) |
| `knowledge/{driver-anatomy,clk-subsystem}.md` | 叙事参考(手写,非生成) |
| `templates/`, `workflow/`, `examples/` | 模板、流程、实例(手写) |
| `xtask/ldp/` | **执行层**:`ldp` CLI(Rust xtask) |

## 架构

五支柱映射(对照 Agent = Model + [上下文 + 工具 + 约束 + 验证 + 纠正]):

```
┌─────────────────────────────────────────────────────────────────┐
│ 上下文支柱  data/*.json(真相源)+ 叙事 knowledge + examples       │
│   双层记忆:ldp summary(概览常驻)+ ldp lookup(细节按需)         │
│   每条映射带 source 证据 + keywords 检索前缀;gaps 带 status 状态机│
├─────────────────────────────────────────────────────────────────┤
│ 工具支柱    ldp CLI(整合的多子命令,不拆成一堆独立工具)          │
│   lookup/gaps/summary/check-data/check-bitfields/scan/gen-doc/   │
│   golden。返回结构化候选 + 显式截断;description 含反例            │
├─────────────────────────────────────────────────────────────────┤
│ 约束支柱    fail-closed 默认:                                    │
│   check-data(source.path 必须存在)、check-bitfields(位段自洽)、 │
│   scan(用到的缺口符号强制提示入报告)                             │
├─────────────────────────────────────────────────────────────────┤
│ 验证支柱    golden 回归(walkthrough 断言 ↔ 当前数据)、           │
│   check-bitfields(false-pass 检测)、gen-doc 幂等                 │
├─────────────────────────────────────────────────────────────────┤
│ 纠正支柱    trace(不可变轨迹)+ staging(候选区 + 门槛发布)。     │
│   promote fail-closed(支持<2/重复/状态错);并入 data/ 后靠       │
│   check+golden 验证。详见下方 Ch8 双循环                          │
└─────────────────────────────────────────────────────────────────┘
```

### 数据层(上下文支柱)

- `api-mapping.json`:每条 `{linux, xkernel, note, source?, keywords, status}`。
  `source.path` 必须在仓库真实存在(`check-data` 校验)——这是铁律 2 的数据化。
  `keywords` 是 Ch3 上下文前缀,`lookup` 据此匹配,不只匹配字面。
- `gaps.json`:每项带 `status`(open/workaround/partial/missing/landed)状态机(Ch3 治理)
  + `dimensions`(clk 的 enable/rate 二维度)或 `severity_branches`(条件定级)。
- `bitfield-defs.json`:每个 def 带 `fields`(每项锡到参考符号 `ref_symbol` + 移植方 `claim`)
  + `ref{tree,file}`(锡 references.json 注册的本地参考树) + 可选 `driver_src{path,anchor}`。
  旧的自指字段(`known_wrong_layouts`/`verification_traces`/`genmask`)已删除。
- `golden/dwcmshc.json`:walkthrough 的期望映射/缺口/结论,机器可查。

### 执行层(ldp)

一个 `ldp` 多子命令二进制(整合 > 拆分,Ch4 rule 5),`xtask/ldp/`,跟随仓库工具链构建。
每个子命令 description 含反例(Ch4 rule 1,见 `ldp help`)。模块划分:

| 模块 | 职责 |
|------|------|
| `data.rs` | JSON 反序列化模型(忠实建模 schema,`#[allow(dead_code)]` 保留未消费字段) |
| `repo.rs` | 仓库根定位(`CARGO_MANIFEST_DIR` 上溯两级)+ 数据文件加载 |
| `bitfield.rs` | 位段打包工具(供 diff 提示用;旧 `classify_trace`/`Verdict` 已删) |
| `extract.rs` | A 档 C 宏提取(GENMASK/BIT/裸 0x)+ D2 Rust 提取(`(p&M)<<S`/`1<<N`/`0xNN`) |
| `references.rs` | `ref list/check`:参考树本地就位 + pin 标记(fail-closed) |
| `layout.rs` | `check-layout`:claim ↔ reference ↔ driver 三方 diff |
| `validate.rs` | check-data / scan / check-provider-crate |
| `query.rs` | summary / lookup(结构化候选+显式截断)/ gaps |
| `golden.rs` | golden 回归 + severity 多形态匹配 |
| `trace.rs` | port 轨迹(Ch8 在线循环):append-only + 路径防护 |
| `staging.rs` | 轨迹聚合 + 候选管理 + promote 门槛(Ch8 离线循环) |
| `requirements.rs` | 缺口→分类→需求 issue 生成/确认(需求决策环节) |
| `gendoc.rs` | gen-doc(表格从 JSON 生成 + 受保护附录块) |
| `cmd.rs` / `main.rs` | 分发 + 自描述 help + 退出码 |

## 核心算法:布局参考锡定(约束支柱)

把"VERSION 会骗人"变成精确检查的方式,**不是**旧的自指范式,而是**参考锡定**:
布局真值从钉死的本地参考树机械导出,diff 移植方声明。

### 为什么旧范式(自指)被重构

旧 `check-bitfields` 检查的是 `bitfield-defs.json` **跟自己**的关系(`known_wrong_layouts`
vs `verification_traces`),从不检查 JSON 跟 Linux 参考的关系。SCMI `message_id`/`token`
记反那类 bug 的根因是"人从参考抄错",而旧检查只能抓"已被手写进 `known_wrong_layouts`
的那一种"抄错方式 —— 是已修 bug 的事后回归,不是预防。另外存在结构性不对称:
`api-mapping.json` 的 `source.path`(指 x-kernel 自己源码)被 `check-data` 硬验存在(强、对);
一到 Linux 侧参考,就退化成不可验的散文串(`genmask`/`spec_ref`)。**我方证据硬、参考证据软**
—— 正是 bug 能溜进去的地方。

### 新范式(参考锡定)

三方比对,三方都是机器可读:

1. **claim ↔ reference**:`fields[].claim`(移植方声明)必须等于参考树里 `ref_symbol`
   机械导出的真值。抽取器认 Linux 受限词汇表:`#define NAME GENMASK(h,l)` / `GENMASK_ULL` /
   `BIT(n)` / `(1 << n)` / 裸 `#define NAME 0xNN`(偏移)。这抓住"人从参考抄错了"
   (SCMI message_id/token 记反,重构后可预防)。
2. **driver ↔ claim**(可选,填了 `driver_src` 才查):驱动代码实际写法必须等于 claim。
   抽取器认 x-kernel 高度统一的位惯法:`const fn name(p..) { (p & M) << S | .. }`
   (位段打包函数)、`const NAME = 1 << N`(单位位)、`const NAME = 0xNN`(偏移)。这抓住
   "声明对、代码错"(最后一层转录)。

```text
reference(Linux C header) -- 机械导出 --> layout_ref   (真值)
        │                                            ↑
        │ claim ↔ reference diff        (抄对了吗?)
        ↓                                            │
claim(JSON) --driver ↔ claim diff (可选)--> driver(Rust) 实际写法
```

对比旧范式的额外收益:**VERSION 假阳性问题消失** —— 新范式比的是"字段在哪"
(位置 diff),不是"打包出什么数",无需凑非零字段躲全零假阳性。算法同时更正确(对到参考)
且更简单(位置 diff,无需 `Verdict` 四态枚举、无需 `known_wrong_layouts`)。

### 参考树(为什么必须本地、拒绝 web search)

参考必须是本地目录(submodule 或 vendored 快照),带 git pin。理由:harness 要能**重新**
机械导出真值;web 链接无法满足"机器可重新导出 + 钉死版本"。缺参考 → `ref check` /
`check-layout` 都 **fail-closed**。这把"太容易忘了拉参考"变成结构性强制:必须在场才能过
(见 `data/references/README.md` 的 vendor 流程)。

### 局限(诚实)

- A 档(C 宏提取)抓不了 `struct foo { u32 a; }` 字段偏移 —— 那要 B 档(pahole/BTF),未实现。
  但 Linux MMIO 驱动多数用 `#define` 偏移而非 struct,A 档覆盖面已很大。
- 提取器是受限词法器,非通用 C/Rust 解析器;认明确词汇表(GENMASK/BIT/裸 0x/`(p&M)<<S`)。
  撞到未知写法会 fail-closed("找不到符号的位声明"),不会静默通过。
- 真实参考需人工 vendor(见 README);机制保证未 vendor 即 fail-closed,不靠纪律。

## 验证支柱:golden 回归

`golden/<name>.json` 把 walkthrough 声明的"用了哪些映射、报了哪些缺口、结论是什么"
变成断言。`ldp golden <name>` 对照当前数据:

- 映射断言:每条期望映射必须在 `api-mapping.json` 找到对应(FAIL_TO_PASS——walkthrough 没引用编造 API)。
- 缺口断言:带标准 id 的期望缺口必须命中 `gaps.json` 且 severity 一致(PASS_TO_PASS——缺口库与 walkthrough 不漂移)。
- severity 匹配覆盖三种形态:主 `severity` / `dimensions` 数组 / `severity_branches` 映射。

改了数据若把 walkthrough 依赖的映射删掉,`golden` 立刻告诉你下游漂移了。

## 数据为源:gen-doc

`knowledge/api-mapping.md` 与 `knowledge/gaps.md` 是**构建产物**,顶部带 banner。
gen-doc 从 JSON 生成表格部分;源码 trait 签名等**参考块**用 `GEN-DOC:APPENDIX` 标记保护,
从旧文件抽出、原样回填(不会被覆盖)。`write_if_changed` 保证幂等(内容不变不写,不抖 mtime)。

原则:**改数据(改 JSON),不改文档;文档是构建产物。**

## 调用约束 / 执行上下文

- `ldp` 是**宿主 std 程序**(xtask),不是内核代码:不跑在 no_std,不碰 `unsafe` 内核内存。
- 仓库根通过编译期 `CARGO_MANIFEST_DIR` 定位,与 CWD 无关——agent 可在任意目录调用。
- 所有检查命令**fail-closed**:出错退出码 1,agent/CI 据此判断。
- 写文件仅 `gen-doc`(且幂等);`scan`/`check-*`/`golden` 只读不写。

## 并发模型

无共享可变状态:每次 `ldp` 调用是一次性进程,数据从 JSON 读入后只读。无锁、无原子、无热路径。

## 设计决策

- **为什么用 Rust(xtask)而非 Python**:贴合仓库工具链(`rust-toolchain.toml` + xtask 惯例),
  不引入 Python/uv 运行时依赖,`ldp` 跟内核一起 build,CI 无需额外环境。
- **为什么数据是 JSON 而非继续用 markdown 表**:markdown 表无法聚合查询(Ch3"黑猫白猫计数"),
  无法表达 status 状态机,无法做 false-pass 计算。JSON 是查询/校验的唯一可行真相源。
- **为什么 gen-doc 保留手写附录块**:trait 签名是源码引用,非结构化数据;强行塞进 JSON 会失去可读性。
  受保护附录块让"表格=生成、参考=手写"两者共存且不互相破坏。
- **为什么 golden 的 framework 缺口用空 id**:sdhci/mmc-core 是本项目独有的"子框架"概念,
  不在标准 gap 目录里(走自研重新实现),硬塞进 gaps.json 会污染目录。空 id = "仅提示,不硬断言"。
- **为什么 check-data 不对"无 source"报错**:std 等价物(Mutex/atomic/log/Box)本就无源码路径,
  报错会产生 30+ 噪音。只对"声明了 source.path 但不存在"(编造)fail-closed。
- **为什么不单列 `ldp scaffold` / `ldp validate`**:`scaffold` 要按具体驱动的
  compatible/class/name 填空,那是 agent 工作(读 `templates/driver-skeleton.md` 即可),
  CLI 生成半填骨架价值低且易错;`validate` 的 FAIL_TO_PASS+PASS_TO_PASS 语义本就是
  `make build && make clippy`(全量构建:新驱动编过=FAIL_TO_PASS,不破坏其他=PASS_TO_PASS),
  在 ldp 里重做编译验证=重复造 build-workflow 的轮子。故两者委托给现有工具,ldp 只做
  markdown 做不到的事(查询/约束/位段/golden/gen-doc)。

## 纠正支柱 + 持续进化(Ch8 双循环)

这是闭环经验沉淀的机制,对齐 Ch8 的在线/离线双循环 + 三道安全边界。

```
在线循环(port 进行中)        离线循环(port 完成后)
─────────────────────         ─────────────────────────
ldp trace add → trace/*.jsonl  ldp staging aggregate → staging/candidates.json
  (不可变 append-only)          ldp staging review     (标★可发布)
                                ldp staging promote    (门槛≥2 → 标 promoted + 指引)
                                          │
                                          ▼ 人/agent 手动改 data/*.json
                                          ▼ ldp check-data && check-bitfields && golden && gen-doc
                                          ▼ 通过 → 知识库进化
```

三道安全边界:
1. **证据/指令隔离**:`trace` 只记观察(what happened),不执行任何动作;
   证据引用(Linux 源码位置)是 `evidence` 字段,不会被执行。
2. **候选/正式分离**:`staging/` 与 `data/{api-mapping,gaps,bitfield-defs}.json`
   物理分离;`promote` 只标状态 + 打印指引,**不自动改正式 data/**——因为
   "加进哪个 section / 用什么 xkernel / 是 mapping 还是 gap"是判断不是机械操作。
3. **验证器不可自我修改**:发布门槛 `PROMOTE_THRESHOLD=2` 硬编码在二进制;
   `promote` 不碰 `golden/` 与门槛规则。并入 data/ 后的 check/golden 由人跑,
   门槛脚本不会被生成物改写。

关键设计:`aggregate` 保留已 promoted 候选的状态(不会被重新覆盖回 new),
避免重复发布;`promote` 对已 promoted/单支持候选 fail-closed。trace 是不可变的
(无 edit/delete 命令),写错的当场观察当历史保留,真错靠 promote 门槛挡。

## 需求决策(Requirements)

把"发现缺口"从"记下来"升级为"可决策的需求 issue"。这是工作流 P4.5 的落地。

```
scan/trace 发现缺口
  → 查 gaps.json 的 infra_type 分类(multi-provider/single-provider/framework/subsystem-class/driver-missing)
  → gen:每个缺口一个 requirements/req-<id>.md(背景+影响+实现路径+验收) + index.json
  → confirm:人工标状态(proposed→confirmed→in-progress→landed/rejected)
```

核心价值:**不同 infra_type 对应不同实现路径**(clk 要多 provider 注册表,i2c 要新
DeviceKind,单纯驱动缺失直接写)。需求 issue 按分类给模板,让人能判断"要不要做、怎么做"。

安全边界(与 staging 一致):`gen` 不改 gaps.json;`confirm` 才改状态;终态保护
(landed/rejected 拒绝重复变更);幂等(同 gap_id+symbol 重新 gen 保留 status)。

### Provider 边界(provider-boundary.json)

provider 类 gap(multi-provider/single-provider)在 issue 里额外渲染「分类依据(provider
判据)」节,强制把"该不该抽 provider"从默认值变成必须逐条过的清单——堵住"顺手标
multi-provider"的滑坡。五判据(物理基础设施 / 多生产者 / 消费者泛化 / 契约跨内核稳定 /
收益>成本)定义在 provider-boundary.json。判据是判断而非机械检查,故:

- `requirements gen` 渲染判据表 + 该 gap 的已记录评估(✓/✗/?),未记录留 `?` + ⚠ 提示。
- provider 类 issue 还渲染「实现质量清单(Stage 2)」节:三 cross-kernel 坑(注册键/trait 完整性/注册表可注入),从 clk 实证归纳。
- `check-data` 对 provider 类 gap 无判据记录 → WARN(非 error,因为留空≠错,只=没论证)。
- `check-provider-crate <Cargo.toml>` 机械查红线:声称 OS-agnostic 的 provider crate 不得依赖 k*/of。这是三坑里唯一机械可查的(其余两条是设计判断)。

判据评估(per_gap_assessment)采用**渐进填充**(b 选项):现有分类是凭直觉的,未逐条
论证;clk 已论证(全 pass,因 clk-subsystem.md 实际写过),其余 provider gap 留空待填。
这诚实承认现状,把判据变成"今后新分类必须过、旧分类慢慢回填"的闸门。

**为什么判据不做成 `check-provider-boundary` 命令?** 它没有机械可判的核心(契约跨内核
稳定吗/消费者写不写泛化代码——无法从代码自动判定),单独成命令会产出假阳性噪音。
判据走"决策产物"(requirements issue),nudge 走现有 check-data。这与位段 false-pass
不同(位段能机械判定 → 会失败的 check)。

## Drop / 资源释放

进程级,无自定义 Drop。`ldp` 进程结束即释放所有资源(文件句柄由 OS 回收)。

## 已知限制

- **位段提取是受限词法器(A 档 C 宏)**:认 GENMASK/BIT/裸 `0xNN` 等 Linux 词汇,抓不了
  `struct foo { u32 a; }` 字段偏移(那要 B 档 pahole/BTF,未实现)。撞到未知写法会
  fail-closed("找不到符号的位声明"),不会静默通过。trace 的 `bitfield-pitfall` 事件
  仍是记录新坑的入口。
- **真实参考需人工 vendor**:机制保证未 vendor 即 fail-closed;但 vendor 哪些子树、
  钉哪个 pin 是人工决策。合成 test-fixture 仅供提取器单元测试,非真实 Linux。
- **D2 driver 提取认统一惯法**:`const fn`(打包)/`1<<N`/`0xNN`。驱动若用花样写法
  (bitfield crate/复杂表达式),提取器报"没找到"而非误断 —— 覆盖面可扩展。
- **golden 覆盖面**:目前只有 dwcmshc 一份基线。新 walkthrough 应补对应 golden。
- **scan 是字面符号匹配**:不解析 C 语法,可能漏掉宏包装的调用;过短符号(如 `RCU`)
  有轻微误匹配风险(代价低:多提示一条缺口,不会静默漏报)。
- **source.path 路径校验是轻量级**:check-data 防绝对路径/`..`逃逸 + 校验存在性,
  但不验证行号准确性(行号是给人工定位的提示)。数据是维护者写的可信输入。
- **promote 不自动改 data/**:这是 Ch8 安全边界的有意设计(避免单次观察污染正式库),
  代价是人需手动把候选转成完整 schema 条目。未来可加 `ldp staging import` 半自动
  填充(但仍需人确认)。

## 审计清单

- [ ] 改 `data/*.json` 后跑过 `ldp check-data && ldp check-bitfields && ldp golden && ldp gen-doc`?
- [ ] 新增/改映射时,`source.path` 在仓库真实存在?
- [ ] 新增位段定义时,`verification_traces` 至少有一条非零字段且 `distinguishes_layout` 与实际打包一致?
- [ ] 改了 walkthrough 后,`golden` 对应基线更新且 PASS?
- [ ] `knowledge/api-mapping.md`/`gaps.md` 顶部有生成 banner,未被手改?
- [ ] port 过程撞到缺口/未映射 API,用 `ldp trace add` 记轨迹了?
- [ ] 发布候选前 `ldp staging aggregate && review`,确认 ≥2 支持再 `promote`?
- [ ] blocker 缺口用 `ldp requirements gen --from-scan` 生成需求 issue 并人工 `confirm` 了?
- [ ] 新缺口的 `infra_type` 分类正确?(不对应 gaps.json,重新 gen 验证)
