---
name: linux-driver-port
description: >-
  Use when porting a Linux C driver (platform/PCI) to x-kernel Rust by
  translating from the upstream driver and adapting to existing x-kernel APIs
  (MMIO/IRQ/DMA/sync/publish), reporting every missing subsystem as a gap.
  Covers API mapping, the two-layer driver structure, gap reporting, and a
  worked ls7a-rtc example. Trigger with /linux-driver-port.
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
ldp scan data/references/linux/drivers/pci/controller/dwc/pcie-dw-rockchip.c \
  --report docs/ai/skills/linux-driver-port/examples/rk3588-pcie-gap-report.md --gate
# 扫源码 → 报告草稿落盘(先报告)→ blocker 决策门(未决策退出 1,禁止先写代码)
ldp golden ls7a-rtc         # 跑 golden 回归(walkthrough 断言 vs 当前数据)
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

## 铁律 6:**序列与可观测事实**同样要落数据(最贵的漏项在这里)

铁律 1 说"初始化序列原样翻译",但**没有形状的数据不会失败**。实践里最贵的
bug 不是常量抄错,而是:

- 参考实现调了 `rtl_hw_init_8125` 才 reset,**我们的驱动里一行都没有**;
- `IntrStatus` 读的是 `0x3e`(816x 布局),8125 在 `0x3c` → 永远读到 0,
  于是"设备毫无反应"这个假事实支撑了后面十几轮推理。

两者都有同一个根因:**harness 只建模了"数据",没建模"控制流"与"事实"**。
所以新增两条,执行层是 `ldp sequence`:

### 6.1 序列即数据:`data/sequences/<port>.toml`

- 每个 port 为 `init` / `start` / `stop` / `link` / `irq` 等**命名路径**写一份
  **有序步骤表**,每步记录:参考 `file:line`、操作(`write`/`rmw`/`poll`/`wait`/`call`)、
  目标(寄存器/OCP 地址/函数)、mask/value、生效条件。
- 生成方式不是手写:`ldp sequence extract <src.c> --function <fn> --path init`
  从参考源码按出现顺序抽草稿,**人只做归并**(把多行写合并成块)与状态填写。
- v2 侧每步必须三选一:
  - `anchored`:代码里有 `// ldp:step <id>`(一行可登记多个 id:
    `// ldp:steps a.b, a.c`)**,且**锚点所在函数能从 `entries` 调用到;
  - `waived`:明说不做,**必须写理由**(与 E-0x 例外表同规格);
  - `unported`:还没做,`--list-unported` 会列出来。
- 门禁 `make ldp-sequence`(默认告警) / `PORT=x SEQUENCE_STRICT=1 make ldp-sequence`
  (未移植即失败)检查 **覆盖 / 可达 / 顺序**。顺序只在**同一函数内**比较——
  源码行号是文件布局,不是执行顺序。

### 6.2 每个被触碰的寄存器都必须有**可观测事实**

读错偏移得到的 0 与"设备真的没动作"不可区分,这是最贵的误判来源。规则:

> 代码里读/写的每一个寄存器,都要在 `data/observations/<port>.toml` 里有至少
> 一条能判真假的事实(恒非零 / 跨某操作变化 / init 后某位为 1 / 等于某值)。

`ldp obs check <board-log>` 用它校验真机日志;事实不成立就当场失败,而不是被
解释成"设备沉默"。

### 6.3 权威源码要先证明,再动手

`port.authoritative` + `port.provenance` 必填,后者要给出**可复现命令 + 预期输出**。
典型踩坑:主线上是 `r8169`,而板子实际绑的是 vendor `r8125`,两者初始化序列不同
(见 `examples/rk3588-pcie/bring-up-log.md` §4.1.1)。上板第一件事:

```bash
lspci -k | grep -A3 -i ethernet      # 或 dmesg | grep -i <chip> / modinfo <drv>
```

### 6.4 每轮上板前写"可判别预测"

> 如果 X 是原因 → 日志会出现 Y;否则会出现 Z。

一轮一条,不允许"先改了再看"。**第一轮只观测**(dump 工作实例状态 + 确认寄存器
偏移),不写假设性改动。这一条比任何工具都省时间:某个 port 的 10 个假设里 9 个
都是"看起来很像",而 3 个真正解决问题的动作全是观测。

### 6.6 参考驱动的 `init/start` 往往在 **open(链路 up)** 时被调用，不在 probe

Linux 的 `ndo_open`（vendor 驱动里的 `*_open`）在**接口 up 之后**才跑
`hw_init → hw_reset → hw_phy_config → hw_config → hw_start`，此时 PHY 通常已协商完。
把同一段塞进 `probe`（枚举时）等于"给一个还在协商的链路装 MAC 配置"，而那套状态
**之后不一定会自己变正确**——本次 RTL8125 就是这样卡了一整轮：probe 时的 init 从未
可用，把同序列移到链路稳定后重跑立刻能收发。

规则：**probe 只取资源、读身份、做无副作用探测；真正的 start 放到"链路 up 之后"**
（轮询 link 状态或 link-change 事件），并把这一步记进 sequence 数据。

### 6.7 对比两个状态之前，先确认它们是同一个状态

本次最贵的错误是把"init 刚跑完的瞬间"与"几秒后的运行态"当成同一份设备状态来对比，
于是"CPU/上下文相关"这种假象成立了（CPU 号只是"距 init 多久"的代理变量）。
**A/B 要在同一次启动、同一个函数里紧邻地做**（交叉实验：A 的初始化 + B 的动作），
不要跨时间点比日志。

### 6.8 「收不到包」先证明设备到底写了什么

接收路径出问题时，最贵的错误是把"我没收到"当成"设备没发/没写"。完成状态字
是有布局的，而且布局随驱动**当前**选的描述符环类型变化（8125B 支持 type-3/4，
但只有开 RSS/PTP 才用；默认 type-1 的判错位只有 `RxRES(bit21)`，而 bit18 在
每个好帧上都置位）。位集合写错 —— 多加一位或抄了别的环型的位 —— 的症状与
"MAC 完全不动"一模一样：整条接收路径静默丢包，日志里什么错都没有。

判据：把**描述符 `opts1` 原值和帧头十六进制**一起打出来。

- 帧头像以太网帧（`01:00:5e:…`、`ff:ff:ff:…`、你自己的 MAC）→ 硬件收得很好，
  问题在你的判错/长度/交给协议栈那一步；
- `opts1` 恒为你 arm 进去的值 → 硬件真的没碰这个环，回到环基址/引擎/accept 位；
- 帧头是垃圾而长度合理 → 槽地址或 D-cache 一致性问题。

配套建立接收侧的可观测事实（环消耗计数、帧计数），并记住**轮询模式需要节拍**：
没有中断路由时，"谁来周期性触发接收"必须有一个明确答案，否则会看到整秒级的
RTT 尖峰和随机丢包 —— 那不是链路问题。

### 6.9 「写不进去」先怀疑 write-mask 语义，不要先怀疑权限

RK3588（以及同代 Rockchip）大量寄存器是 **write-mask 形式**：一个 32 位字里，
**高半是写使能掩码，低半才是数据**。写裸值时高半为 0，硬件理解为"没有任何位
需要改"，于是寄存器纹丝不动 —— 这跟"bank 被写保护""电源域没开""ATF 拦住了"
完全无法区分，而后面这些解释会把人带去查一整圈无关的东西。

已确认属于这套约定的寄存器：

| 寄存器 | 写法 |
|---|---|
| CRU 门控 `CLKGATE_CON*` | `(bits << 16) \| value`，且 `SET_TO_DISABLE`（1=关） |
| GPIO v2 `SWPORT_DR/DDR` | 拆低/高半字：`BIT(n%16) \| BIT(n%16+16)`，读要 `readl(reg+4)<<16 \| readl(reg)` |
| GRF（如 `USB3OTGn_CFG`） | `0xFFFF_0000 \| data`（高半全 1 表示低 16 位都可写） |

**判据（一行数学就能定案）**：写一个显眼的花纹值再读回。
若读回值 ≈ `写入值 & 0xFFFF_FFFF` 的高半与低半**按位与**（例：`0x12345678` →
`0x1230`，因为 `0x5678 & 0x1234 = 0x1230`），那就是 write-mask，而不是故障。

配套两条：

- **别用返回值当证据**：`clk enable()` / `write32()` 返回 Ok 完全不代表硬件动了。
  每次改状态都在前后各读一次寄存器（本次就是靠 `con42 0x75 -> 0x05` 与
  `hw-disable ... (count 0)` 两处日志定位的）。
- **参考实现的写法就是规格**：Linux `rockchip_gpio_writel_bit`、U-Boot 的 `gpio`
  命令都按掩码写，只有裸 `mw.l` 不会。用 bootloader 的 `mw` 做寄存器实验时，
  务必记得它按 write-mask 解释你的值。

### 6.5 辅助函数的**语义**也要逐字转录（案例）

`mac_ocp_modify` 这类"读-改-写"包装最容易被"顺手写得更好懂"而改坏。上游是：

```c
data = __r8168_mac_ocp_read(tp, reg);
__r8168_mac_ocp_write(tp, reg, (data & ~mask) | set);   // set 不与 mask
```

若写成 `(data & ~mask) | (set & mask)`，所有 `mask = 0x0000` 的**置位行就全变成
空操作**。rtl8125 移植里因此静默丢了 `0xc0ac |= 0x1f80`(ASPM L1 退出事件)、
`0xc0b4 |= 0x0c`、`0xe860 |= 0x80` 三行——代码里看起来"照抄了参考表"，实际一个
都没生效。教训：**照抄的判定标准是"语义等价"，不是"行数齐全"**；辅助函数要连
参数含义一起抄，并在注释里写明 `set` 不参与 mask。

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
  golden/ls7a-rtc.json           golden 回归基线(walkthrough 断言)
knowledge/                     ← 生成物(ldp gen-doc 产出,勿手改)
  api-mapping.md                 由 api-mapping.json 生成
  gaps.md                        由 gaps.json 生成
```

**原则:改数据(改 JSON),不改文档;文档是构建产物。** `schema.md` 解释字段语义。

## 数据目录新增项

| 路径 | 内容 | 门禁 |
|---|---|---|
| `data/sequences/<port>.toml` | 参考实现的**有序步骤表**（init/start/stop/...） | `make ldp-sequence`（覆盖/可达/顺序） |
| `data/observations/<port>.toml` | 每个被触碰寄存器的**可观测事实** | `ldp obs check <log>`（接线中） |

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
| [`examples/ls7a-rtc-walkthrough.md`](examples/ls7a-rtc-walkthrough.md) | 需要参照时 | ★ 完整 demo:Linux rtc-loongson → x-kernel(v2,golden 基线) |

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
- **P5 集成校验**:接线组合根(`drivers/init` 调 register())+ Kconfig feature;按 `build-workflow` 跑 build + clippy。**编译过≠可用**。
- **P6 带机校验/triage**:加错误路径可观测性;真机跑通该设备类的代表操作。**P6 没过 = 驱动不可用**。
- **P7 经验沉淀(Ch8)**:`ldp trace` 记撞墙点,`ldp staging` 把 ≥2 支持的候选沉淀进正式知识库。

## 一次产出的交付物

1. **硬件逻辑层**:`drivers/<class>/src/<name>.rs`——寄存器操作、初始化序列、中断处理。
2. **绑定与注册**:驱动文件内 `impl DriverOps<PlatformBus>`(probe 取资源+发布)+ `static IDS` + `driver_base::declare_driver!(…)`。
3. **接线改动**:`drivers/init` 组合根调 register();crate 加进平台 Kconfig/defconfig。
4. **缺口报告**:缺失子系统、阻塞程度、替代方案、可编译性结论。

## 触发方式

- 命令式:在 Claude Code 里输入 `/linux-driver-port`(由 `.claude/skills/linux-driver-port/SKILL.md` 入口加载本共享知识)。
- 文档式:直接读本目录下文件,或经 `AGENTS.md`/`CLAUDE.md` 引用加载。

## 相关 skill

- [`build-workflow`](../build-workflow/SKILL.md) —— 构建、clippy、运行、平台 target。
- [`code-guidelines`](../code-guidelines/SKILL.md) —— Rust 内核编码与审查规范(unsafe、命名、并发)。
