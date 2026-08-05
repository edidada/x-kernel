# 移植工作流（Phase 0–5）

把一个 Linux 驱动变成 x-kernel 驱动的标准流程。每阶段有明确**输入 / 步骤 / 产出 / 该读哪个文件**。全程受 [SKILL.md 五条铁律](../SKILL.md#五条铁律决定产出质量必须遵守) 约束。

> 第一次跑请同步对照 [`examples/dwcmshc-walkthrough.md`](../examples/dwcmshc-walkthrough.md)——它是这份流程的完整实例。

```
P0 采集 → P1 分析 → P2 映射 → P3 生成 → P4 缺口报告 → P4.5 需求 issue → P5 集成校验 → P6 带机校验/triage → P7 经验沉淀
```

---

## Phase 0 — 采集（Intake）

**目标**：确定移植目标和落点，避免动错地方。

**输入**：用户指定的 Linux 驱动（文件路径或子系统名）。

**步骤**：
1. 定位 Linux 源文件（如 `drivers/mmc/host/sdhci-of-dwcmshc.c`）。若无本地源码树，向用户索取或用内核源码 URL。
2. 找出 `of_device_id` / `of_match_table` → 记下 **compatible 字符串**（这是 DT 匹配键）。
3. 判定**设备类**：Linux 把它注册成什么（netdev / block / tty / misc / i2c_adapter / platform）→ 映射到 x-kernel `DeviceKind`（见 [api-mapping §3](../knowledge/api-mapping.md)）。**若无对应 kind**（i2c/spi/watchdog）→ 这是 P4 的 blocker，先记下，继续。
4. 确认 x-kernel **落点**：硬件逻辑层 `drivers/<class>/src/<name>.rs` + 胶水层 `drivers/kdriver/src/driver_registry/<bus>/<name>.rs`（见 [driver-anatomy](../knowledge/driver-anatomy.md)）。
5. 检查项目里**是否已存在**同名/同 compatible 的实现（`grep -rn "<compat>" drivers/`）——若有，转为"补全/对照"模式（如 sdmmc）。

**产出**：一张"移植任务卡"（compatible、DeviceKind、落点、是否已存在）。

---

## Phase 1 — 分析（Analyze）

**目标**：把 Linux 驱动读透，产出"驱动画像"。**这是铁律 1（硬件是真相）的主战场**。

**步骤**：通读 Linux 驱动，记录：
1. **probe 流程**：按顺序列出 probe 里做了什么（使能 clk → 解除 reset → init phy → ioremap → 注册 irq → 写寄存器初始化 → register_*）。
2. **依赖的子系统**：列出所有 `devm_*_get` / `clk_*` / `phy_*` / `reset_*` / `gpiod_*` / `regmap_*` 调用 → 这就是缺口候选清单。
3. **寄存器初始化序列**：把 probe 和 hw_init 里的 `writel`/`readl`/`rmw` 序列**原样抄下来**（偏移、值、bit 操作、顺序、中间的 `udelay`）。这是必须忠实翻译的部分。
4. **中断处理**：top-half（`irq_handler_t`）做了什么，是否 schedule 下半部（workqueue/tasklet/threaded_irq）。
5. **私有数据结构**：`struct xxx_priv` 的字段 → 映射到 Rust 结构体。
6. **设备类操作**：`net_device_ops` / `block_device_operations` / `file_operations` 的每个回调 → 对应 trait 方法。

> **铁律 1 强化——"参考序列"必须钉死在一个来源上**：probe/初始化序列**选一个**参考实现（U-Boot **或** Linux，通常是那个在你目标板上已知能跑的——RK3588 SD 卡就选 U-Boot `dwmci_init`，因为 U-Boot `mmc info` 证明它能枚举同一张卡），把它的初始化函数**逐行编号**抄成一张清单：
>
> ```text
> ① PWREN = 1                      // dwmci_init 第 2 行
> ② ctrl_reset(ALL_RESET_FLAGS)    // wait_reset
> ③ setup_bus(f_min=400kHz)        // 含 CLKDIV/CLKENA/UPD_CLK 三段
> ④ RINTSTS=0xFFFFFFFF; INTMASK=0; TMOUT=0xFFFFFFFF
> ⑤ FIFOTH=...
> ...
> ```
>
> 这张"编号清单"是 Phase 3 可追溯校验的输入。**禁止把 U-Boot 和 Linux 各抄一半混着用**——混抄最容易各自漏步骤（dw_mmc 移植就因为这样漏了 U-Boot 的 `PWREN=1` 和 Linux 的 `bus_hz/=RK3288_CLKGEN_DIV`）。

**产出**：一份"驱动画像"文档（markdown），含上面 6 项 + **编号参考序列清单**。后续 P2/P3 都基于它。

---

## Phase 2 — 映射（Map）

**目标**：把画像里的每条 Linux API 翻译成 x-kernel API。**铁律 2（绝不编造 API）**。

**步骤**：
1. 对"依赖的子系统"和"设备类操作"里的**每一个** Linux 调用，跑 `ldp lookup <api>`（或读 [`api-mapping.md`](../knowledge/api-mapping.md)）。`ldp lookup` 返回结构化候选 + 源码依据，命中/未命中一目了然。
2. **命中** → 记下 x-kernel 替换（含源码依据），归入"已映射"。
3. **未命中** → 查 `ldp gaps`（或 [`gaps.md`](../knowledge/gaps.md)）：
   - 若 gaps 已列该子系统 → 按其"替代"方案处理，标阻塞程度，归入"缺口"。
   - 若 gaps 未列 → 按 [gaps.md 判断框架](../knowledge/gaps.md#判断框架) 定级，自己补一条缺口（现状"项目未找到实现"）。
4. 对寄存器序列：标注哪些位操作可直接翻译、哪些依赖缺失子系统（如"这步 `clk_set_rate` 决定频率，无 clk 框架→TODO"）。
5. **撞到位段/协议常量**（SCMI header、寄存器位段等）：位段定义写进 `data/bitfield-defs.json`（附 GENMASK 依据），跑 `ldp check-bitfields` 守护——它能揪出"用全零字段(VERSION)验证位段"这类假阳性。绝不凭记忆写位段（铁律 1/2）。

**产出**：两份清单——「已映射 API」（Linux→x-kernel 对应表）+「缺口」（子系统/用法/处置）。

> **经验沉淀（Ch8）**：撞到 `ldp lookup` 未命中的 API 或缺口子系统用法时，跑
> `ldp trace add <port-name> --kind <lookup-miss|gap> --linux <符号> --note '<上下文>'`
> 记一条不可变轨迹。多次 port 撞到同一东西会聚成可发布候选（见下方「Phase 7 — 经验沉淀」）。

> 关键纪律：**任何你"想当然"写出的 x-kernel 调用都要回到源码验证**。`ldp lookup` 命中的每条都带 source 依据；手写查不到的用 `grep -rn "<fn_name>" drivers/` 确认签名。找不到 = 缺口，不要硬写。`ldp check-data` 会校验 source.path 真实存在，防编造。

---

## Phase 3 — 生成（Generate）

**目标**：按骨架写出 Rust 代码。**铁律 1 + 3**。

**步骤**（按 [`driver-skeleton.md`](../templates/driver-skeleton.md) 填空）：
1. **硬件逻辑层** `drivers/<class>/src/<name>.rs`：
   - 寄存器常量/bit（照搬 Linux `#define`）。
   - 私有结构体（`SpinNoIrq` 串行化）。
   - `unsafe fn new(vaddr)` + `# Safety` + `hw_init`（P1 的寄存器序列，逐条翻译）。
   - `impl Device` + `impl <Class>Device`（P1 的设备类操作）。
2. **胶水层** `driver_registry/<bus>/<name>.rs`：`impl DeviceDriver` + `probe_device`（取 MMIO→new→publish）+ `descriptor()`。
3. **注册三连**：`firmware_specs.rs`（compatible）+ `<bus>/mod.rs`（DRIVER_FACTORIES）+ `Cargo.toml`（feature）。
4. 每处 `unsafe` 写 `// SAFETY:`；每个缺口点留 `// TODO(gap): <子系统> 缺失，见 gap-report` 并按 P2 的处置实现替代/TODO。
5. **可追溯校验（铁律 1 的验收）**：拿 Phase 1 的"编号参考序列清单"逐条对代码——**每一条编号步骤必须在生成的 Rust 代码里找到对应**（寄存器写、延时、顺序）。任何一条对不上 = 漏翻译，回去补。这一步专治"画像写了、代码漏了"：dw_mmc 移植的画像里写了 `bus_hz/=RK3288_CLKGEN_DIV(2)`，代码却没落地，就是缺这道核对。

**产出**：两个 .rs 文件 + 三处改动 + **一张"编号步骤→代码位置"对照表**（贴进 gap-report 或 PR 描述）。

---

## Phase 4 — 缺口报告（Report）

**目标**：把 P2 的缺口清单 + P3 的处置，整理成给人看的报告。**铁律 4**。

**步骤**：按 [`gap-report.md`](../templates/gap-report.md) 填：
1. 头部（compatible、DeviceKind、落点、移植结论）。
2. 依赖子系统清单（勾选有无）。**跑 `ldp scan <Linux源码路径>` 自动揪出这份驱动用到的、且当前是缺口的子系统符号**，确保不漏报（铁律 4 的自动化）。
3. 逐条缺口表（子系统 / 阻塞程度 / Linux 用法 / x-kernel 处置 / 建议）——"处置"必须具体到改了哪段代码。
4. blocker 汇总 + 可编译性结论 + 后续行动优先级。

**产出**：`<name>-gap-report.md`（建议放 `docs/ai/skills/linux-driver-port/examples/` 旁或驱动目录）。

### P4.5 — 缺口转需求 issue（决策门）

**目标**：把 P4 发现的缺口，尤其 blocker，转成**可决策的需求 issue**——每个缺口一个 md，
给出分类 + 实现路径 + 验收标准，等人工确认后再进 P5。这避免了"撞到 blocker 就静默 stub
或凭记忆硬写"。

**步骤**：

```bash
ldp requirements gen --from-scan <Linux源码路径>
# 每个缺口生成一个 data/requirements/req-<id>.md，自动分类：
#   multi-provider  → clk/gpio/phy/reset/regulator/iommu/pinctrl（phandle 键多 provider）
#   single-provider → dma 模式（单全局 provider）
#   framework       → regmap/workqueue/timer/completion（子框架）
#   subsystem-class → i2c/spi/watchdog/rtc（需新 DeviceKind）
ldp requirements list            # 看哪些待确认（📝 proposed）
ldp requirements show <id>       # 审阅：背景/影响/实现路径/验收标准
ldp requirements confirm <id> --status confirmed   # 人工确认
```

**判据**：
- **blocker 缺口无替代** → 必须生成需求 issue 并获 confirmed，才能进 P5（否则驱动不可用，
  P5 没意义）。或人工明确接受"该驱动当前内核不可用"，在 gap-report 标注。
- **works-around 缺口** → 可继续 P5（有限制），需求 issue 照生成供后续补全。
- **分类不对** → 编辑 `data/gaps.json` 对应 subsystem 的 `infra_type` 字段，重新 gen。

**产出**：`data/requirements/` 下若干 issue md + `index.json`（状态机：
proposed→confirmed→in-progress→landed/rejected）。

> 这一步与 P7 的区别：P4.5 是"这个缺口要不要实现"（需求侧），P7 是"这次 port 的经验
> 值不值得沉淀进知识库"（知识侧）。一个缺口可以先 P4.5 提需求、P7 记轨迹。

---

## Phase 5 — 集成校验（Integrate & Verify）

**目标**：确认驱动能编进镜像、能被发现。**铁律 5**。

**步骤**（命令以 [`build-workflow`](../../build-workflow/SKILL.md) 为准）：
1. 在 `drivers/kdriver/Cargo.toml` 加 feature，在平台 defconfig（如 `platforms/kplat-aarch64/rk3588_defconfig`）设 `KFEAT_<NAME>=y`。
2. 构建：按 rk3588/aarch64 target 跑 `make` / `cargo build`。
3. 检查：`cargo clippy`（按 build-workflow 的范围）；确认无新 warning/错误。
4. 确认驱动被注册：启动日志里应有 `registered driver: <name>`（见 [driver_registry/mod.rs:80](../../../../../drivers/kdriver/src/driver_registry/mod.rs#L80) 的 debug 日志），且 DT 匹配后 probe 被调用。
5. 若有 blocker 导致 probe 返回 `Unsupported`，日志应体现，并在 gap-report 标注。
6. 把 build/clippy 结果回填 gap-report 的"可编译性结论"与"集成状态"清单。

**产出**：通过的构建 + 填完的集成状态清单。

> ⚠️ **编译过 + 注册上 ≠ 驱动可用**。P5 只证明驱动进得了镜像、会被 probe。驱动**真正在硬件上工作**要靠 Phase 6 验收——dw_mmc 移植就是 P5 全绿、真机却 `failed to initialize card: Io`。"寄存器能读出 verid/fifo_depth"也只说明 MMIO 通了，**不代表卡识别/数据通路 OK**。不要在 P5 就宣告完成。

---

## Phase 6 — 带机校验与 triage（Bring-up & Triage）

**目标**：证明驱动**在目标硬件上真的工作**，而不只是编进镜像。这是"受限可用"结论的唯一依据。**没有跑通 P6，不得在缺口报告里写"可用"**。

### 6.1 可观测性是前提（先加再加不得）

在跑真机**之前**，给驱动的**每条错误路径**加状态转储，否则出了问题只能瞎猜：

- 每个返回 `Io`/超时/错误的地方，连同失败点的**状态寄存器**一起 `log::error!`（MMC：`RINTSTS`/`STATUS`/`CMD`；网卡的 `ISR`/`STATUS`；…），并标注是哪一步（哪条 CMD / 哪个操作）。
- 错误码要能区分原因：MMC 把 `RTO`（response timeout）和 `RCRC`/`RESP_ERR`/`HLE` 分别命名打印——`RTO` 通常意味着"没卡 / 没上电 / 时钟错"，`HLE` 意味着"CIU 还忙就发了命令"。
- 参考：[block/src/sdmmc/host.rs](../../../../../drivers/block/src/sdmmc/host.rs) 的 `send_command` 在失败时打印 `CMD{opcode} error rintsts={..} ({cmd_err_name})`。

> 一条裸的 `return Err(DriverError::Io)` 在真机 bring-up 阶段是**不可接受**的——它把"挂在哪一步、什么原因"全吞了。

### 6.2 验收标准（smoke gate——按设备类选）

P5 的"probe 被调用 + publish"只是门槛。真正的验收是**该设备类的代表操作在真机（或高保真 QEMU 模型）上跑通**：

- **块设备 / SD / eMMC**：卡片完整枚举——`CMD0→CMD8→CMD55+ACMD41(OCR power-up)→CMD2(CID)→CMD3(RCA)→CMD9(CSD, 解出容量)→CMD7(select)→ACMD6(4-bit)→读出一个块`。日志里应看到 RCA、容量、`card ready`，且 `read_block` 返回正确数据。
- **网卡**：link up、能收发（ping/ARP 通）。
- **char（UART 等）**：能读写、波特率正确。
- 通用：**`publish_*` 之后，用户态或内核侧能完成一次完整 I/O 并拿到正确结果**。

> 部分通过也要记：dw_mmc 当前是"枚举通 + 单块读写"——高速/UHS/DMA/多块是缺口。把**跑通的边界**写进 gap-report，别夸大。

### 6.3 差异诊断表（症状 → 原因）

真机失败时按此定位（以 MMC 为例，其他设备类同理）：

| 症状（日志） | 首选怀疑 | 怎么验 |
|--------------|---------|--------|
| `CMD0` 之后所有 CMD 都 `RTO` | 卡没上电（`PWREN`）/ 没时钟（`CLKENA`/`CLKDIV`）/ `bus_hz` 错 | 对照 U-Boot dump `PWREN/CLKENA/CLKDIV/RINTSTS` |
| 仅 `CMD8`/`ACMD41` `RTO`，`CMD0` 过 | 非 SDHC 卡 / 老卡；或卡在但电压不对 | 看 `CMD8` 回显、`OCR` |
| `HLE`（hardware locked） | CIU 还忙就发了下一条命令（`UPD_CLK` 没等完） | 查 `send_cmd` 是否等 `START` 自清 |
| `RCRC` 一片 | 信号完整性 / 相位（缺 phy tuning）/ 时钟过速 | 降到基础速率再试 |
| 读块数据错（容量对、内容错） | FIFO 宽度/`data_shift`、字节序、`BLKSIZ/BYTCNT` | 核对 `HCON.hdata_width`、PIO 取数 |
| probe 没被调用 | 注册三连断（feature 链 / compatible 不匹配） | 回 P5 |

### 6.4 对照"已知可用"参考（最高效的定位手段）

当被移植的控制器在**同一块板**上已有 Linux/U-Boot 在驱动时（RK3588 几乎总是如此），**别从零猜**：

1. 在 U-Boot 下用其调试命令读真值：MMC 用 `mmc info` / `mmc dev` / 读寄存器；记下 bus speed、bus width、RCA、容量。
2. 在 x-kernel 下 dump 同一组寄存器（`PWREN/CLKENA/CLKDIV/CTYPE/UHS_REG/RINTSTS`），**逐个对比**。
3. 第一个不一致的寄存器 = bug 所在。dw_mmc 的 `PWREN=0`（应为 1）就是这么一眼看出来的。

**产出**：一份"带机验证记录"（哪些设备类的代表操作跑通、边界在哪、对照 U-Boot 的寄存器差异、遗留缺口），回填 gap-report 的"集成状态/可用性结论"。P6 没过的项 = 驱动在该维度**不可用**，如实标注。

### 6.5 固件后端子系统（SCMI / SMC / PSCI）triage

当驱动的某条路径把请求转发给**安全固件**（SCMI 时钟/复位、PSCI、其它 SMCCC 服务）时，错误不再来自设备寄存器，而来自固件返回的状态码。别拿 6.3 的寄存器 dump 那套去猜——按**命令探针阶梯**定位（由弱到强各发一次，`log::warn!` 结果）：

| 探针 | 通了的含义 |
|------|------|
| 最简命令、**无 payload**（如 SCMI base `VERSION`） | 传输（SMC/HVC）、共享内存布局对；**但不证明消息头位段对**（VERSION 的 msg=0/token=0 会掩盖位段错误，见下） |
| 目标协议的最简命令（如 clock 协议 `VERSION`） | 协议路由对 |
| **规范强制、带 payload 的命令**（如 base `MESSAGE_ATTRIBUTES`） | 通 → 头位段**和** payload 帧格式都对（关键判据） |
| 真正想发的命令（如 clock `RATE_GET`） | 目标 |

判读（**⚠ VERSION 会骗人**）：

- 无 payload 的命令（如 VERSION）通、**带 payload 的命令挂** → **先怀疑消息头位段，再怀疑 payload 帧格式**。SCMI 头位段记反时（`message_id`/`token` 位置错）VERSION 仍然通（msg=0、token=0，放哪都一样），但 `message_id≠0` 的命令会被固件当成 VERSION → 带 payload 的报 `PROTOCOL_ERROR`，**症状和"固件 stub"几乎一样**。判据：拿已知能用的实现（Linux `pack_scmi_header` / 上游 `arm-scmi-rs`，`MSG_ID_MASK=GENMASK(7,0)`、`MSG_TOKEN_ID_MASK=GENMASK(27,18)`）逐位核对，再怀疑固件。
- 头位段确认无误、带 payload 的强制命令仍挂 → 才看 payload 帧格式（length 约定 / 偏移）。
- 上述都通、唯独目标命令挂 → 固件不支持该命令。
- 真正的固件 stub（只实现 VERSION）**存在但很罕见**；认定前必须先用已知正确的头编码排除自己这边。
- SCMI 状态码速查：`0` SUCCESS、`-1` NOT_SUPPORTED、`-2` INVALID_PARAMETERS、`-4` NOT_FOUND、`-10` PROTOCOL_ERROR。响应 `length`（错误响应 = header+status = 8 字节）可反推 length 字段约定，核对自己的请求格式。

> 实例（RK3588 SCMI 时钟，见 [`clk-subsystem.md`](../knowledge/clk-subsystem.md)）：`base VERSION` 返回 `0x20000`、但 `MESSAGE_ATTRIBUTES`/`RATE_GET` 都 `-10`，一度**误判为固件 stub**——实为 SCMI 消息头位段记反（`message_id` 应在 `[7:0]`、`token` 在 `[27:18]`），VERSION 把它掩盖了；改正后 `ciu` 经 `RATE_GET` 解析到 ~49.5 MHz。教训：VERSION 通 ≠ 头位段对。

---

## Phase 7 — 经验沉淀（Continuous Evolution，Ch8）

**目标**：把这次 port 的“撞墙经验”沉淀回知识库，让下次更顺。这不是可选的“锦上添花”——
它是纠错支柱的闭环：没有它，同一个缺口会被反复撞、反复推断。

**在线（port 进行中，P1–P6 任何阶段撞到就记）**：

```bash
ldp trace add <port-name> --kind lookup-miss --linux <未命中的API> --note '<上下文>'
ldp trace add <port-name> --kind gap --linux <缺口子系统符号> --note '<用法>' --evidence '<Linux源码位置>'
ldp trace add <port-name> --kind bitfield-pitfall --linux '<位段名>' --note '<易错点>'
```

轨迹不可变（append-only）。port 名只允许字母数字/连字符/下划线。

**离线（port 完成后，人执行）**：

```bash
ldp staging aggregate          # 聚合所有 trace/*.jsonl → staging/candidates.json
ldp staging review             # 看哪些候选 ≥2 条不同 port 轨迹支持（★ 可发布）
ldp staging promote --id <id>  # 门槛检查 → 标 promoted + 打印入库指引
```

`promote` **不自动改 `data/`**（Ch8 安全边界：候选/正式分离）。它标 promoted 后，
人/agent 手动把候选转成完整 schema 条目加进 `data/{api-mapping,gaps,bitfield-defs}.json`，
再跑 `ldp check-data && check-bitfields && golden && gen-doc` 验证（Ch8 验证器门槛）。
任一失败 → 回滚 data/ 改动。

> 为什么 ≥2 支持？Ch8：“至少两条非失败轨迹支持才写入正式文档。” 单次 port 的
> 观察可能是偶发或误解；两次独立 port 都撞到同一缺口，才值得正式入库。门槛硬编码
> 在 `ldp` 二进制（验证器不可自我修改）。

---

## 常见陷阱

- **跳过 P1 直接翻译**：不读完 Linux probe 就动手，会漏掉 clk/phy/reset 等隐藏依赖，P4 报告失真。**先画像再写码**。
- **凭印象写 x-kernel API**：把 `devm_ioremap` 写成不存在的 `ioremap`。**每条都查表/查源码**（铁律 2）。
- **悄悄 stub blocker**：把 `phy_power_on` 删掉当没事。**gap-report 必须写明**（铁律 4）。
- **改了硬件序列**：自作主张合并/省略寄存器步骤或改延时。**照搬**（铁律 1）。
- **凭记忆写位段/协议格式常量**：把 SCMI 消息头的 `message_id`/`token` 位段记反（应 `message_id[7:0]`、`token[27:18]`），编得过、还可能恰好跑通。**即使记得也要打开 Linux 源码逐位核对；优先抄参考实现的掩码**（铁律 1/2 延伸，详见 SKILL.md"协议/位段常量纪律"）。源码本地没有（`drivers/*` 内部 .h/.c）时联网取或标 `UNVERIFIED`，不得静默凭记忆落地。位段定义进 `data/bitfield-defs.json`，`ldp check-bitfields` 会守护（含已知错误布局的 false-pass 检测）。
- **拿全零字段当基线验证位段**：SCMI `VERSION`（`msg=0`/`token=0`）位段错也照样通——用它"验证 header 编码对"是假阳性。**验证位段必须用非零字段**（见 §6.5）。`ldp check-bitfields` 会把 VERSION 这类标为文档化 false-pass。
- **漏注册三连之一**：只写了 .rs 没加 feature，驱动根本不进镜像。**用骨架检查表逐项打钩**。
