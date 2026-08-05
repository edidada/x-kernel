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
任务从未真正完成(假成功)。本设计把"位段错了 VERSION 会骗人"这句散文,变成 `ldp check-bitfields`
里一个会失败的检查。

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
- `bitfield-defs.json`:每个 def 带 `fields`(附 GENMASK)+ `known_wrong_layouts`
  + `verification_traces`(自评 `distinguishes_layout`)。
- `golden/dwcmshc.json`:walkthrough 的期望映射/缺口/结论,机器可查。

### 执行层(ldp)

一个 `ldp` 多子命令二进制(整合 > 拆分,Ch4 rule 5),`xtask/ldp/`,跟随仓库工具链构建。
每个子命令 description 含反例(Ch4 rule 1,见 `ldp help`)。模块划分:

| 模块 | 职责 |
|------|------|
| `data.rs` | JSON 反序列化模型(忠实建模 schema,`#[allow(dead_code)]` 保留未消费字段) |
| `repo.rs` | 仓库根定位(`CARGO_MANIFEST_DIR` 上溯两级)+ 数据文件加载 |
| `bitfield.rs` | 位段打包 + false-pass 判定(`classify_trace`) |
| `query.rs` | summary / lookup(结构化候选+显式截断)/ gaps |
| `validate.rs` | check-data / check-bitfields / scan |
| `golden.rs` | golden 回归 + severity 多形态匹配 |
| `trace.rs` | port 轨迹(Ch8 在线循环):append-only + 路径防护 |
| `staging.rs` | 轨迹聚合 + 候选管理 + promote 门槛(Ch8 离线循环) |
| `requirements.rs` | 缺口→分类→需求 issue 生成/确认(需求决策环节) |
| `gendoc.rs` | gen-doc(表格从 JSON 生成 + 受保护附录块) |
| `cmd.rs` / `main.rs` | 分发 + 自描述 help + 退出码 |

## 核心算法:位段 false-pass 检测(约束支柱)

把"VERSION 会骗人""验证位段必须用非零字段"变成精确的、无启发式的检查。

**判据**:对每条 `verification_trace`,在**正确布局**与每个 `known_wrong_layout` 下分别打包;
打包值相同 → 这条 trace **无法区分**这两种布局。

```
pack(layout, values) = Σ_field (values[field] & widthmask(field)) << field.lsb
```

`known_wrong_layout` 以正确字段名为准、按名覆盖位段(忠实表达"记反了某些字段的位置")。

**交叉验证 trace 自评**(`distinguishes_layout` 字段):

| 打包结果 | 自评=true | 自评=false |
|----------|-----------|------------|
| 相同(无法区分) | **矛盾 → fail-closed**(数据撒谎) | 文档化 false-pass(正常,如 VERSION) |
| 不同(能区分) | 确认(正常) | 欠精确(无害提示) |

实测:VERSION(msg=0/token=0)在正确/错误布局下都打包成 `0x4000` → 文档化 false-pass;
若有人把它标"能区分",`check-bitfields` 报矛盾并退出码 1。这就是 SCMI bug 的可执行化。

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

## Drop / 资源释放

进程级,无自定义 Drop。`ldp` 进程结束即释放所有资源(文件句柄由 OS 回收)。

## 已知限制

- **位段检测依赖人正确填写 `known_wrong_layouts`**:能抓"已知错误布局"的 false-pass,
  但抓不了尚未记录的新错误布局。trace 的 `bitfield-pitfall` 事件 + staging 是
  补充新错误布局的入口(但仍需人逐位核对后填进 `bitfield-defs.json`)。
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
