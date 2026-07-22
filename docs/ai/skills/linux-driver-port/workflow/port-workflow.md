# 移植工作流（Phase 0–5）

把一个 Linux 驱动变成 x-kernel 驱动的标准流程。每阶段有明确**输入 / 步骤 / 产出 / 该读哪个文件**。全程受 [SKILL.md 五条铁律](../SKILL.md#五条铁律决定产出质量必须遵守) 约束。

> 第一次跑请同步对照 [`examples/dwcmshc-walkthrough.md`](../examples/dwcmshc-walkthrough.md)——它是这份流程的完整实例。

```
P0 采集 → P1 分析 → P2 映射 → P3 生成 → P4 缺口报告 → P5 集成校验
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

**产出**：一份"驱动画像"文档（markdown），含上面 6 项。后续 P2/P3 都基于它。

---

## Phase 2 — 映射（Map）

**目标**：把画像里的每条 Linux API 翻译成 x-kernel API。**铁律 2（绝不编造 API）**。

**步骤**：
1. 对"依赖的子系统"和"设备类操作"里的**每一个** Linux 调用，查 [`api-mapping.md`](../knowledge/api-mapping.md)。
2. **命中** → 记下 x-kernel 替换（含源码依据），归入"已映射"。
3. **未命中** → 查 [`gaps.md`](../knowledge/gaps.md)：
   - 若 gaps 已列该子系统 → 按其"替代"方案处理，标阻塞程度，归入"缺口"。
   - 若 gaps 未列 → 按 [gaps.md 判断框架](../knowledge/gaps.md#判断框架) 定级，自己补一条缺口（现状"项目未找到实现"）。
4. 对寄存器序列：标注哪些位操作可直接翻译、哪些依赖缺失子系统（如"这步 `clk_set_rate` 决定频率，无 clk 框架→TODO"）。

**产出**：两份清单——「已映射 API」（Linux→x-kernel 对应表）+「缺口」（子系统/用法/处置）。

> 关键纪律：**任何你"想当然"写出的 x-kernel 调用都要回到源码验证**。用 `grep -rn "<fn_name>" drivers/` 确认签名。找不到 = 缺口，不要硬写。

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

**产出**：两个 .rs 文件 + 三处改动。

---

## Phase 4 — 缺口报告（Report）

**目标**：把 P2 的缺口清单 + P3 的处置，整理成给人看的报告。**铁律 4**。

**步骤**：按 [`gap-report.md`](../templates/gap-report.md) 填：
1. 头部（compatible、DeviceKind、落点、移植结论）。
2. 依赖子系统清单（勾选有无）。
3. 逐条缺口表（子系统 / 阻塞程度 / Linux 用法 / x-kernel 处置 / 建议）——"处置"必须具体到改了哪段代码。
4. blocker 汇总 + 可编译性结论 + 后续行动优先级。

**产出**：`<name>-gap-report.md`（建议放 `docs/ai/skills/linux-driver-port/examples/` 旁或驱动目录）。

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

**产出**：通过的构建 + 填完的集成状态清单 + （可选）实机/QEMU 冒烟。

---

## 常见陷阱

- **跳过 P1 直接翻译**：不读完 Linux probe 就动手，会漏掉 clk/phy/reset 等隐藏依赖，P4 报告失真。**先画像再写码**。
- **凭印象写 x-kernel API**：把 `devm_ioremap` 写成不存在的 `ioremap`。**每条都查表/查源码**（铁律 2）。
- **悄悄 stub blocker**：把 `phy_power_on` 删掉当没事。**gap-report 必须写明**（铁律 4）。
- **改了硬件序列**：自作主张合并/省略寄存器步骤或改延时。**照搬**（铁律 1）。
- **漏注册三连之一**：只写了 .rs 没加 feature，驱动根本不进镜像。**用骨架检查表逐项打钩**。
